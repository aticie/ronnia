//! Notification dispatch: validates EventSub subscription identity, parses chat
//! requests, applies stream-state and revocation transitions, and admits work.
//!
//! Duplicate suppression has two layers on purpose: a bounded in-memory guard on
//! the provider's notification id drops redeliveries before any user-visible
//! feedback is emitted, and the `deliveries` primary key stays the authoritative
//! guard for an accepted request across concurrent shards.

use super::{
    Config, Outcome,
    conduit::{self, Conduit},
    isolated_hub, maintenance, record_queue_drop,
    requests::{RequestEnvelope, RequestIngress},
    transport::{self, Incoming, Pool},
    validate_identity,
};
use crate::{
    domain::StreamEvent,
    parser,
    providers::{Providers, valid_login},
    storage::Store,
    twitch_auth::{BotIdentity, TwitchAuth},
};
use anyhow::{Result, anyhow};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// Recently dispatched notification ids, bounded like the shard event channel.
///
/// Twitch redelivers a notification when a session handoff races, and a rejected
/// request must not produce a second chat reply, so this guard runs before policy
/// evaluation; the durable `deliveries` key still guards the accepted path.
const SEEN_CAPACITY: usize = 2048;
/// Provider-supplied ids can be large, so the guard keeps a fixed-size digest
/// instead of the id itself: two copies of a 1 MiB id times 2,048 entries would
/// otherwise be a multi-gigabyte allocation reachable from provider input.
type SeenKey = [u8; 16];

#[derive(Default)]
pub(super) struct Seen {
    order: VecDeque<SeenKey>,
    ids: HashSet<SeenKey>,
}

fn seen_key(id: &str) -> SeenKey {
    let digest = Sha256::digest(id.as_bytes());
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

impl Seen {
    /// Returns `true` when the id is new. An empty id is never deduplicated.
    pub(super) fn insert(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        let key = seen_key(id);
        if !self.ids.insert(key) {
            return false;
        }
        self.order.push_back(key);
        while self.order.len() > SEEN_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.ids.remove(&old);
            }
        }
        true
    }
}

#[derive(Clone, Debug)]
pub(super) struct Message {
    pub(super) id: String,
    pub(super) channel: String,
    pub(super) channel_id: i64,
    pub(super) author: String,
    pub(super) author_id: String,
    pub(super) moderator: bool,
    pub(super) subscriber: bool,
    pub(super) vip: bool,
    pub(super) points: bool,
}

pub(super) struct IncomingChat {
    pub(super) message: Message,
    pub(super) text: String,
}

/// EventSub chat message to the request pipeline. Bot posts come back over
/// EventSub (unlike IRC), so the managed identity is filtered explicitly.
pub(super) fn chat_message(event: &Value, bot_id: &str) -> Option<IncomingChat> {
    let channel_id = event["broadcaster_user_id"]
        .as_str()
        .and_then(|id| id.parse::<i64>().ok())?;
    let channel = event["broadcaster_user_login"]
        .as_str()
        .filter(|login| valid_login(login))?
        .to_ascii_lowercase();
    let chatter_id = event["chatter_user_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    if chatter_id == bot_id {
        return None;
    }
    let author = event["chatter_user_login"]
        .as_str()
        .filter(|login| valid_login(login))?
        .to_ascii_lowercase();
    let id = event["message_id"].as_str().filter(|id| !id.is_empty())?;
    let text = event["message"]["text"].as_str()?.to_owned();
    let mut moderator = false;
    let mut subscriber = false;
    let mut vip = false;
    for badge in event["badges"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match badge["set_id"].as_str().unwrap_or_default() {
            "moderator" => moderator = true,
            "subscriber" | "founder" => subscriber = true,
            "vip" => vip = true,
            _ => {}
        }
    }
    let points = event["channel_points_custom_reward_id"]
        .as_str()
        .is_some_and(|id| !id.is_empty());
    Some(IncomingChat {
        text,
        message: Message {
            id: id.to_owned(),
            channel,
            channel_id,
            author,
            author_id: chatter_id.to_owned(),
            moderator,
            subscriber,
            vip,
            points,
        },
    })
}

fn event_login(event: &Value) -> String {
    event["broadcaster_user_login"]
        .as_str()
        .filter(|login| valid_login(login))
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// One EventSub runtime: owns the shard pool and maintenance worker for a
/// validated bot identity and consumes their events until shutdown.
pub(super) struct Runtime {
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    auth: TwitchAuth,
    identity: BotIdentity,
    ingress: RequestIngress,
    conduit: Conduit,
    channels: watch::Receiver<u64>,
    signals: watch::Sender<maintenance::Signal>,
    actions: mpsc::Sender<maintenance::Action>,
    maintenance_rx: Option<(
        watch::Receiver<maintenance::Signal>,
        mpsc::Receiver<maintenance::Action>,
    )>,
    seen: Seen,
}

impl Runtime {
    pub(super) async fn new(
        config: Arc<Config>,
        store: Store,
        providers: Providers,
        auth: TwitchAuth,
        identity: BotIdentity,
        ingress: RequestIngress,
    ) -> Result<Self> {
        // Subscribe before any startup membership read (and even before ensure's
        // HTTP), so startup changes cannot consume an activation edge.
        let channels = store.channel_changes();
        let conduit = conduit::ensure(&config, &store, &providers).await?;
        let (signals, signal_rx) = watch::channel(maintenance::Signal::default());
        let (actions, action_rx) = maintenance::actions();
        Ok(Self {
            config,
            store,
            providers,
            auth,
            identity,
            ingress,
            conduit,
            channels,
            signals,
            actions,
            maintenance_rx: Some((signal_rx, action_rx)),
            seen: Seen::default(),
        })
    }

    fn bot_id(&self) -> &str {
        &self.identity.user_id
    }

    pub(super) async fn run(&mut self, shutdown: &CancellationToken) -> Result<Outcome> {
        let local = shutdown.child_token();
        let (targets, target_rx) = watch::channel(1);
        let mut pool = Pool::start(
            self.config.clone(),
            self.providers.clone(),
            self.conduit.clone(),
            target_rx,
            local.clone(),
        );
        let (signals, actions) = self.maintenance_rx.take().expect("runtime runs once");
        let worker = maintenance::Maintenance::new(
            self.config.clone(),
            self.store.clone(),
            self.providers.clone(),
            self.conduit.id.clone(),
            self.identity.user_id.clone(),
            maintenance::Channels {
                membership: self.channels.clone(),
                signals,
                actions,
                targets,
                topology: pool.topology.clone(),
            },
        );
        let worker_cancel = local.clone();
        let worker_job = tokio::spawn(async move {
            tokio::select! {
                _ = worker_cancel.cancelled() => {},
                _ = worker.run(worker_cancel.clone()) => {},
            }
        });
        let mut auth_job = tokio::spawn(validate_identity(
            self.auth.clone(),
            self.identity.user_id.clone(),
            local.clone(),
        ));
        let mut auth_finished = false;
        // Foreground consumption begins immediately. Socket welcome/attach,
        // subscription creation and snapshots all progress independently.
        let result = loop {
            tokio::select! {
                _ = shutdown.cancelled() => break Ok(Outcome::Cancelled),
                result = &mut auth_job => {
                    auth_finished = true;
                    break match result {
                        Ok(outcome) => Ok(outcome),
                        Err(error) => Err(anyhow!("authorization monitor failed: {error}")),
                    };
                }
                changed = pool.topology.changed() => {
                    if changed.is_err() {
                        break Err(anyhow!("EventSub shard topology supervisor stopped"));
                    }
                    let topology = *pool.topology.borrow_and_update();
                    if topology.remote_count > 0 && topology.remote_count == topology.attached_count {
                        maintenance::request(&self.signals, true, true);
                    }
                }
                event = pool.events.recv() => {
                    let Some(event) = event else {
                        break Err(anyhow!("EventSub shard event channel closed"));
                    };
                    tracing::trace!(shard = event.shard_id, "dispatching EventSub event");
                    let result = match event.incoming {
                        Incoming::Notification { subscription, event, timestamp, message_id } => {
                            self.notification(&subscription, &event, &timestamp, &message_id).await
                        }
                        Incoming::Revocation { subscription } => self.revocation(&subscription).await,
                        _ => Ok(()),
                    };
                    if let Err(error) = result { break Err(error); }
                }
            }
        };
        local.cancel();
        pool.shutdown().await;
        let _ = worker_job.await;
        if !auth_finished {
            let _ = auth_job.await;
        }
        result
    }

    async fn revocation(&self, subscription: &Value) -> Result<()> {
        let Some(owned) = conduit::owned(subscription, &self.conduit.id, Some(self.bot_id()))
        else {
            return Ok(());
        };
        let status = subscription["status"].as_str().unwrap_or_default();
        tracing::warn!(
            kind = owned.kind,
            status,
            broadcaster = owned.broadcaster,
            "Twitch revoked an EventSub subscription"
        );
        if owned.kind == conduit::CHAT_TYPE.0
            && matches!(status, "authorization_revoked" | "user_removed")
        {
            self.store.revoke_twitch_channel(owned.broadcaster).await?;
            tracing::warn!(
                broadcaster = owned.broadcaster,
                "channel grant marked revoked; the broadcaster must reauthorize before chat resumes"
            );
        }
        // The maintenance owner re-lists before reconciliation; no second
        // owner mutates its subscription cache from the event path.
        maintenance::request(&self.signals, true, false);
        Ok(())
    }

    fn target(&self, action: maintenance::Action) {
        maintenance::enqueue(&self.actions, &self.signals, action);
    }

    async fn notification(
        &mut self,
        subscription: &Value,
        event: &Value,
        timestamp: &str,
        message_id: &str,
    ) -> Result<()> {
        if !self.seen.insert(message_id) {
            tracing::debug!(message_id, "dropping a redelivered EventSub notification");
            return Ok(());
        }
        let Some(owned) = conduit::owned(subscription, &self.conduit.id, Some(self.bot_id()))
        else {
            return Ok(());
        };
        let broadcaster = owned.broadcaster;
        let event_broadcaster = event["broadcaster_user_id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok());
        if event_broadcaster != Some(broadcaster) {
            tracing::warn!(
                kind = owned.kind,
                "ignoring an event whose broadcaster does not match the subscription condition"
            );
            return Ok(());
        }
        let at = match transport::timestamp_nanos(timestamp) {
            Ok(at) => at,
            Err(error) => {
                tracing::warn!(kind = owned.kind, error = %error, "ignoring an event with an invalid timestamp");
                return Ok(());
            }
        };
        match owned.kind {
            "channel.chat.message" => {
                let Some(chat) = chat_message(event, self.bot_id()) else {
                    tracing::debug!(broadcaster, "chat message ignored by identity validation");
                    return Ok(());
                };
                tracing::debug!(
                    broadcaster = chat.message.channel_id,
                    channel = %chat.message.channel,
                    author = %chat.message.author,
                    message_id = %chat.message.id,
                    length = chat.text.len(),
                    "chat message received"
                );
                let Some(beatmap) = parser::parse_message(&chat.text) else {
                    tracing::debug!(
                        channel = %chat.message.channel,
                        author = %chat.message.author,
                        message_id = %chat.message.id,
                        "chat message ignored: no supported beatmap link"
                    );
                    return Ok(());
                };
                let span = tracing::info_span!(
                    "request",
                    sentry.op = "queue.process",
                    message_id = %chat.message.id,
                    channel_id = chat.message.channel_id,
                    beatmap_id = beatmap.id,
                    beatmap_kind = match beatmap.kind {
                        parser::BeatmapKind::Map => "map",
                        parser::BeatmapKind::Mapset => "mapset",
                    },
                );
                let permit = match self.ingress.permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(tokio::sync::TryAcquireError::NoPermits) => {
                        sentry::Hub::run(isolated_hub(), || {
                            span.in_scope(|| {
                                record_queue_drop("request", "full");
                                tracing::warn!(
                                    channel = %chat.message.channel,
                                    queue_capacity = self.config.request_queue_capacity,
                                    permits_available = self.ingress.permits.available_permits(),
                                    "request queue is full; incoming request rejected"
                                );
                            })
                        });
                        return Ok(());
                    }
                    Err(tokio::sync::TryAcquireError::Closed) => {
                        sentry::Hub::run(isolated_hub(), || {
                            span.in_scope(|| {
                                record_queue_drop("request", "closed");
                                tracing::warn!(
                                    channel = %chat.message.channel,
                                    queue_capacity = self.config.request_queue_capacity,
                                    permits_available = self.ingress.permits.available_permits(),
                                    "request scheduler is closed; incoming request rejected"
                                );
                            })
                        });
                        return Ok(());
                    }
                };
                match self.ingress.requests.try_send(RequestEnvelope {
                    message: chat.message,
                    beatmap,
                    permit,
                    span,
                    queued_at: Instant::now(),
                }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(envelope)) => {
                        sentry::Hub::run(isolated_hub(), || {
                            envelope.span.in_scope(|| {
                                record_queue_drop("request", "full");
                                tracing::warn!(
                                    channel = %envelope.message.channel,
                                    queue_capacity = self.config.request_queue_capacity,
                                    permits_available = self.ingress.permits.available_permits(),
                                    "request queue is full; incoming request rejected"
                                );
                            })
                        });
                    }
                    Err(mpsc::error::TrySendError::Closed(envelope)) => {
                        sentry::Hub::run(isolated_hub(), || {
                            envelope.span.in_scope(|| {
                                record_queue_drop("request", "closed");
                                tracing::warn!(
                                    channel = %envelope.message.channel,
                                    queue_capacity = self.config.request_queue_capacity,
                                    permits_available = self.ingress.permits.available_permits(),
                                    "request scheduler is closed; incoming request rejected"
                                );
                            })
                        });
                    }
                }
            }
            "stream.online" => {
                let login = event_login(event);
                if self
                    .store
                    .apply_stream_event(broadcaster, login, StreamEvent::Online, at)
                    .await?
                {
                    tracing::info!(
                        broadcaster,
                        "stream went online; resolving the category before request acceptance"
                    );
                    // Capture this event's revision before accepting another event.
                    // A later offline/category transition invalidates this lookup,
                    // even when the targeted action queue is saturated.
                    match self.store.snapshot_versions(vec![broadcaster]).await {
                        Ok(versions) => {
                            if let Some((id, expected_revision)) = versions.into_iter().next() {
                                self.target(maintenance::Action::Schedule {
                                    id,
                                    expected_revision,
                                });
                            } else {
                                maintenance::request(&self.signals, false, true);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, broadcaster,
                                "could not capture online revision; scheduling a full snapshot correction");
                            maintenance::request(&self.signals, false, true);
                        }
                    }
                    // The category is unknown until the targeted lookup lands.
                }
            }
            "stream.offline" => {
                if self
                    .store
                    .apply_stream_event(broadcaster, event_login(event), StreamEvent::Offline, at)
                    .await?
                {
                    tracing::info!(
                        broadcaster,
                        "stream went offline; request acceptance removed"
                    );
                    self.target(maintenance::Action::Cancel { id: broadcaster });
                }
            }
            "channel.update" => {
                let Some(category) = event["category_id"].as_str() else {
                    tracing::warn!(broadcaster, "channel.update omitted category_id");
                    return Ok(());
                };
                if self
                    .store
                    .apply_stream_event(
                        broadcaster,
                        event_login(event),
                        StreamEvent::Category(category.to_owned()),
                        at,
                    )
                    .await?
                {
                    tracing::info!(
                        broadcaster,
                        category,
                        "channel category updated; request acceptance recomputed"
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}
