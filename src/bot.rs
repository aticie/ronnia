//! Twitch cloud-bot transport.
//!
//! Incoming chat and stream state arrive over independently supervised EventSub
//! conduit shards (`WebSocket`, `wss://eventsub.wss.twitch.tv/ws`). Outgoing chat uses the
//! managed bot user token; conduit and subscription management use the
//! application token manager in [`Providers`]. Only EventSub events for
//! channels this process subscribed to are trusted, and no account is ever
//! modified because a transport call failed.

use crate::{
    config::Config,
    domain::{StreamEvent, User},
    parser::{self, Beatmap, BeatmapKind},
    providers::{Providers, valid_login},
    storage::Store,
    twitch_auth::{BotIdentity, TwitchAuth},
};
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

mod conduit;
mod eventsub;
mod maintenance;
mod scheduler;
mod shards;
mod streams;

use self::{
    conduit::Conduit,
    eventsub::{Backoff, Incoming},
    scheduler::{RequestEnvelope, process_requests},
};

/// Revalidate the managed bot grant well inside Twitch's validation window.
const AUTH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const AUTH_DEADLINE: Duration = Duration::from_secs(20);
/// Remote subscription audit; heals subscriptions revoked behind our back.
const AUDIT_INTERVAL: Duration = Duration::from_secs(600);
/// Coalesce targeted category lookups triggered by `stream.online` bursts.
const TARGET_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_RESYNC_ATTEMPTS: u32 = 5;
const SEEN_CAPACITY: usize = 2048;

#[derive(Clone, Debug)]
struct Message {
    id: String,
    channel: String,
    channel_id: i64,
    author: String,
    author_id: String,
    moderator: bool,
    subscriber: bool,
    vip: bool,
    points: bool,
}

/// Admission always carries the scheduler's single shared memory budget.
#[derive(Clone)]
struct RequestIngress {
    requests: mpsc::Sender<RequestEnvelope>,
    permits: Arc<Semaphore>,
}

struct Echo {
    channel_id: i64,
    text: String,
}

#[derive(Clone, Copy)]
struct Clock {
    epoch: f64,
    started: Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            epoch: chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
            started: Instant::now(),
        }
    }
    fn now(self) -> f64 {
        self.epoch + self.started.elapsed().as_secs_f64()
    }
}

/// The bot worker never takes the API down. Transport failures are logged and
/// retried with capped jitter; a missing grant leaves the process waiting while
/// the HTTP API keeps serving.
pub async fn run(config: Arc<Config>, store: Store, shutdown: CancellationToken) -> Result<()> {
    let providers = Providers::new(config.clone(), shutdown.clone())?;
    let auth = TwitchAuth::new(config.clone(), store.clone())?;
    let clock = Clock::new();
    store
        .call(|db| {
            db.execute("UPDATE deliveries SET status='uncertain',error='process stopped during send; not replayed' WHERE status='sending'", [])?;
            db.execute("UPDATE deliveries SET status='failed',error='process stopped before send; safe to submit a new request' WHERE status='pending'", [])?;
            Ok(())
        })
        .await?;
    let capacity = config.request_queue_capacity;
    let permits = Arc::new(Semaphore::new(capacity));
    let (requests_tx, requests_rx) = mpsc::channel(capacity);
    let (echo_tx, echo_rx) = mpsc::channel(capacity);
    let (identity_tx, identity_rx) = watch::channel::<Option<String>>(None);
    tokio::join!(
        process_requests(
            config.clone(),
            store.clone(),
            providers.clone(),
            requests_rx,
            echo_tx,
            shutdown.clone(),
            clock,
        ),
        send_feedback(
            config.clone(),
            providers.clone(),
            auth.clone(),
            echo_rx,
            identity_rx,
            shutdown.clone(),
        ),
        supervise(
            config,
            store,
            providers,
            auth,
            identity_tx,
            RequestIngress {
                requests: requests_tx,
                permits
            },
            shutdown,
        ),
    );
    Ok(())
}

pub async fn backfill(config: Arc<Config>, store: Store) -> Result<()> {
    let providers = Providers::new(config, CancellationToken::new())?;
    let mut after = 0;
    let mut failures = 0u64;
    loop {
        let ids = store.missing_beatmap_ids(after, 100).await?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            after = id;
            match providers
                .get_beatmap(&Beatmap {
                    id,
                    kind: BeatmapKind::Map,
                    mods: String::new(),
                })
                .await
            {
                Ok((map, _)) => {
                    store.cache_put(map).await?;
                    tracing::info!(id, "backfilled beatmap");
                }
                Err(error) => {
                    failures += 1;
                    tracing::error!(id, error = %error, "backfill failed; continuing remaining IDs");
                }
            }
        }
    }
    if failures != 0 {
        bail!("backfill completed with {failures} failed beatmaps; rerun to retry missing IDs");
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Cancelled,
    Restart,
}

async fn supervise(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    auth: TwitchAuth,
    identity_tx: watch::Sender<Option<String>>,
    ingress: RequestIngress,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new();
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let Some(identity) = wait_for_identity(&auth, &shutdown).await else {
            identity_tx.send_replace(None);
            return;
        };
        identity_tx.send_replace(Some(identity.user_id.clone()));
        let started = Instant::now();
        let result = Runtime::new(
            config.clone(),
            store.clone(),
            providers.clone(),
            auth.clone(),
            identity,
            ingress.clone(),
        )
        .await;
        let result = match result {
            Ok(mut runtime) => runtime.run(&shutdown).await,
            Err(error) => Err(error),
        };
        identity_tx.send_replace(None);
        if shutdown.is_cancelled() {
            return;
        }
        match result {
            Ok(Outcome::Cancelled) => return,
            Ok(Outcome::Restart) => {
                tracing::info!("restarting the EventSub runtime for the updated bot authorization");
                backoff.reset();
            }
            Err(error) => {
                if started.elapsed() > Duration::from_secs(120) {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                tracing::error!(
                    error = %error,
                    retry_ms = delay.as_millis(),
                    "EventSub transport degraded; reconnecting without deleting accounts"
                );
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sleep(delay) => {}
                }
            }
        }
    }
}

/// Wait for a usable managed bot grant. Missing authorization is reported once
/// and polled slowly so an `authorize-bot` run is picked up without a restart.
async fn wait_for_identity(auth: &TwitchAuth, shutdown: &CancellationToken) -> Option<BotIdentity> {
    let mut reported = false;
    let mut backoff = Backoff::new();
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        let result = tokio::select! {
            _ = shutdown.cancelled() => return None,
            result = auth.ready_bot(shutdown) => result,
        };
        match result {
            Ok(Some(identity)) => {
                if reported {
                    tracing::info!(login = %identity.login, "Twitch bot authorization is available");
                }
                return Some(identity);
            }
            Ok(None) => {
                if !reported {
                    tracing::warn!(
                        "no Twitch bot authorization stored; run the authorize-bot command. \
                         The HTTP API stays available while the bot waits."
                    );
                    reported = true;
                }
                backoff.reset();
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = sleep(Duration::from_secs(30)) => {}
                }
            }
            Err(error) => {
                if !reported {
                    tracing::warn!(
                        error = %error,
                        "stored Twitch bot authorization is not usable; run authorize-bot to reauthorize"
                    );
                    reported = true;
                } else {
                    tracing::debug!(error = %error, "Twitch bot authorization still unusable");
                }
                let delay = backoff.next_delay().max(Duration::from_secs(30));
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = sleep(delay) => {}
                }
            }
        }
    }
}

async fn send_feedback(
    config: Arc<Config>,
    providers: Providers,
    auth: TwitchAuth,
    mut echoes: mpsc::Receiver<Echo>,
    identities: watch::Receiver<Option<String>>,
    shutdown: CancellationToken,
) {
    let mut next_send = Instant::now();
    loop {
        let echo = tokio::select! {
            _ = shutdown.cancelled() => return,
            echo = echoes.recv() => echo,
        };
        let Some(echo) = echo else { return };
        if config.dry_run {
            continue;
        }
        let Some(sender) = identities.borrow().clone() else {
            continue;
        };
        let text = sanitize_chat(&echo.text, 450);
        if text.is_empty() {
            continue;
        }
        // Twitch allows only a small number of chat messages per window;
        // feedback is paced instead of being retried after a 429.
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = sleep_until(next_send) => {}
        }
        next_send = Instant::now() + Duration::from_millis(1500);
        if let Err(error) = providers
            .send_twitch_message(&auth, echo.channel_id, &sender, &text)
            .await
        {
            tracing::warn!(
                channel_id = echo.channel_id,
                uncertain = error.uncertain,
                error = %error,
                "Twitch chat feedback was not delivered"
            );
        }
    }
}

/// A single in-flight authorization check, independent of event consumption.
async fn validate_identity(
    auth: TwitchAuth,
    bot_id: String,
    shutdown: CancellationToken,
) -> Outcome {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Outcome::Cancelled,
            _ = sleep(AUTH_INTERVAL) => {},
        }
        let result = tokio::select! {
            _ = shutdown.cancelled() => return Outcome::Cancelled,
            result = timeout(AUTH_DEADLINE, auth.ready_bot(&shutdown)) => result,
        };
        match result {
            Ok(Ok(Some(current))) if current.user_id == bot_id => {}
            Ok(Ok(Some(changed))) => {
                tracing::info!(login = %changed.login, "Twitch bot identity changed; restarting the transport");
                return Outcome::Restart;
            }
            Ok(Ok(None)) => {
                tracing::warn!("Twitch bot authorization was revoked; waiting for authorize-bot");
                return Outcome::Restart;
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "Twitch bot revalidation failed; returning to the authorization wait");
                return Outcome::Restart;
            }
            Err(_) => tracing::warn!("Twitch bot revalidation timed out"),
        }
    }
}

struct Runtime {
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
    async fn new(
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

    async fn run(&mut self, shutdown: &CancellationToken) -> Result<Outcome> {
        let local = shutdown.child_token();
        let (targets, target_rx) = watch::channel(1);
        let mut pool = shards::Pool::start(
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

    async fn revocation(&mut self, subscription: &Value) -> Result<()> {
        let kind = subscription["type"].as_str().unwrap_or_default();
        let status = subscription["status"].as_str().unwrap_or_default();
        let ours = subscription["transport"]["method"].as_str() == Some("conduit")
            && subscription["transport"]["conduit_id"].as_str() == Some(self.conduit.id.as_str());
        if !ours || conduit::owned_version(kind).is_none() {
            return Ok(());
        }
        if kind == conduit::CHAT_TYPE.0
            && subscription["condition"]["user_id"].as_str() != Some(self.bot_id())
        {
            return Ok(());
        }
        let broadcaster = subscription["condition"]["broadcaster_user_id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok());
        tracing::warn!(kind, status, broadcaster = ?broadcaster, "Twitch revoked an EventSub subscription");
        if kind == conduit::CHAT_TYPE.0
            && matches!(status, "authorization_revoked" | "user_removed")
            && let Some(id) = broadcaster
        {
            self.store.revoke_twitch_channel(id).await?;
            tracing::warn!(
                broadcaster = id,
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
        let Some((kind, version, broadcaster)) =
            subscription_identity(&self.conduit.id, subscription)
        else {
            return Ok(());
        };
        let event_broadcaster = event["broadcaster_user_id"]
            .as_str()
            .and_then(|id| id.parse::<i64>().ok());
        if event_broadcaster != Some(broadcaster) {
            tracing::warn!(
                kind,
                version,
                "ignoring an event whose broadcaster does not match the subscription condition"
            );
            return Ok(());
        }
        if !self.seen.insert(message_id) {
            return Ok(());
        }
        let at = match eventsub::timestamp_nanos(timestamp) {
            Ok(at) => at,
            Err(error) => {
                tracing::warn!(kind, error = %error, "ignoring an event with an invalid timestamp");
                return Ok(());
            }
        };
        match kind {
            "channel.chat.message" => {
                if subscription["condition"]["user_id"].as_str() != Some(self.bot_id()) {
                    tracing::warn!(
                        broadcaster,
                        "ignoring a chat subscription that is not bound to the managed bot"
                    );
                    return Ok(());
                }
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
                let permit = match self.ingress.permits.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(tokio::sync::TryAcquireError::NoPermits) => {
                        tracing::warn!(channel = %chat.message.channel,
                            "request queue is full; incoming request rejected");
                        return Ok(());
                    }
                    Err(tokio::sync::TryAcquireError::Closed) => {
                        tracing::warn!(channel = %chat.message.channel,
                            "request scheduler is closed; incoming request rejected");
                        return Ok(());
                    }
                };
                match self.ingress.requests.try_send(RequestEnvelope {
                    message: chat.message,
                    beatmap,
                    permit,
                }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(envelope)) => {
                        tracing::warn!(channel = %envelope.message.channel,
                            "request queue is full; incoming request rejected");
                    }
                    Err(mpsc::error::TrySendError::Closed(envelope)) => {
                        tracing::warn!(channel = %envelope.message.channel,
                            "request scheduler is closed; incoming request rejected");
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

/// Validate a notification's subscription identity against our conduit. Any
/// mismatch is ignored rather than trusted, including another bot's chat feed.
fn subscription_identity<'a>(
    conduit_id: &str,
    subscription: &'a Value,
) -> Option<(&'a str, &'a str, i64)> {
    let kind = subscription["type"].as_str()?;
    let version = subscription["version"].as_str()?;
    let expected = conduit::owned_version(kind)?;
    if version != expected {
        tracing::warn!(
            kind,
            version,
            expected,
            "ignoring an EventSub notification with an unexpected version"
        );
        return None;
    }
    if subscription["transport"]["method"].as_str() != Some("conduit")
        || subscription["transport"]["conduit_id"].as_str() != Some(conduit_id)
    {
        tracing::warn!(
            kind,
            "ignoring an EventSub notification from a foreign transport"
        );
        return None;
    }
    let broadcaster = subscription["condition"]["broadcaster_user_id"]
        .as_str()
        .and_then(|id| id.parse::<i64>().ok())?;
    Some((kind, version, broadcaster))
}

struct IncomingChat {
    message: Message,
    text: String,
}

/// EventSub chat message to the request pipeline. Bot posts come back over
/// EventSub (unlike IRC), so the managed identity is filtered explicitly.
fn chat_message(event: &Value, bot_id: &str) -> Option<IncomingChat> {
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

/// Bounded duplicate suppression for notification messages. Stream events are
/// also guarded by timestamps in storage; chat by the `deliveries` table.
#[derive(Default)]
struct Seen {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl Seen {
    fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.order.push_back(id.to_owned());
        self.set.insert(id.to_owned());
        while self.order.len() > SEEN_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

fn policy(user: &User, message: &Message, stars: f64) -> std::result::Result<(), String> {
    if user.settings.test {
        return Ok(());
    }
    if message.author.eq_ignore_ascii_case(&message.channel) {
        return Err("Author is broadcaster and not in test mode.".into());
    }
    if user.settings.sub_only && !(message.moderator || message.subscriber || message.vip) {
        return Err("Subscriber only request mode is active.".into());
    }
    if user.settings.points_only && !message.points {
        return Err("Channel Points only mode is active.".into());
    }
    if user
        .excluded_users
        .iter()
        .any(|login| login.eq_ignore_ascii_case(&message.author))
    {
        return Err("Requester is excluded.".into());
    }
    let [low, high] = user.settings.sr;
    if stars < low || (high != -1.0 && stars > high) {
        return Err(format!(
            "@{} Streamer is accepting requests between {low:.1}-{high:.1}* difficulty. Your map is {stars:.1}*.",
            message.author
        ));
    }
    Ok(())
}

fn queue_echo(echoes: &mpsc::Sender<Echo>, message: &Message, text: String) {
    if let Err(error) = echoes.try_send(Echo {
        channel_id: message.channel_id,
        text,
    }) {
        tracing::warn!(channel = %message.channel, error = %error, "Twitch feedback not queued");
    }
}

async fn process_one(
    config: &Config,
    store: &Store,
    providers: &Providers,
    echoes: &mpsc::Sender<Echo>,
    message: &Message,
    beatmap: &Beatmap,
    clock: Clock,
) -> Result<()> {
    let Some(user) = store.user_by_twitch(message.channel_id).await? else {
        return Ok(());
    };
    if !user.twitch_username.eq_ignore_ascii_case(&message.channel) {
        tracing::warn!(channel = %message.channel, room_id = message.channel_id, "request rejected: stale channel identity");
        return Ok(());
    }
    // Enable remains the master switch. Test mode additionally bypasses the
    // live/osu-category gate and the per-request policy checks below.
    if !user.settings.enable {
        tracing::debug!(
            channel = %message.channel,
            message_id = %message.id,
            "request ignored: bot is disabled for this channel"
        );
        return Ok(());
    }
    if !user.settings.test && !user.is_live {
        tracing::debug!(
            channel = %message.channel,
            author = %message.author,
            message_id = %message.id,
            "request ignored: channel is offline or not streaming osu!"
        );
        return Ok(());
    }
    let id = message.id.clone();
    if store.call(move |db| Ok(db.query_row("SELECT 1 WHERE EXISTS(SELECT 1 FROM deliveries WHERE twitch_message_id=?1) OR EXISTS(SELECT 1 FROM statistics WHERE source_id='twitch:' || ?1)", [id], |_| Ok(())).optional()?.is_some())).await? { return Ok(()); }
    let map = match beatmap.kind {
        BeatmapKind::Map => store.cache_get(beatmap.id).await?,
        BeatmapKind::Mapset => store.cache_get_set(beatmap.id).await?,
    };
    let (map, set) = match map {
        Some(map) => {
            let set = map
                .get("beatmapset")
                .context("cached beatmap omitted beatmapset")?
                .clone();
            (map, set)
        }
        None => {
            let (map, set) = providers.get_beatmap(beatmap).await?;
            match beatmap.kind {
                BeatmapKind::Map => store.cache_put(map.clone()).await?,
                BeatmapKind::Mapset => store.cache_put_set(beatmap.id, map.clone()).await?,
            }
            (map, set)
        }
    };
    let stars = map["difficulty_rating"]
        .as_f64()
        .filter(|n| n.is_finite())
        .context("invalid beatmap difficulty_rating")?;
    if let Err(reason) = policy(&user, message, stars) {
        if !config.dry_run && reason.starts_with('@') {
            queue_echo(echoes, message, reason.clone());
        }
        tracing::info!(channel = %message.channel, requester = %message.author, reason, "request rejected");
        return Ok(());
    }
    let author_id = message.author_id.clone();
    let channel_id = user.twitch_id;
    let last = store.call(move |db| Ok(db.query_row("SELECT last_success FROM request_cooldowns WHERE channel_id=?1 AND requester_id=?2", params![channel_id, author_id], |row| row.get::<_, f64>(0)).optional()?)).await?;
    if !user.settings.test && last.is_some_and(|last| clock.now() - last < user.settings.cooldown) {
        tracing::info!(channel = %message.channel, requester = %message.author, "request rejected: cooldown");
        return Ok(());
    }
    let pm = prepare_message(message, &map, &set, &beatmap.mods)?;
    if config.dry_run {
        tracing::info!(channel = %message.channel, beatmap_id = beatmap.id, "dry run: request accepted without chat or statistics");
        return Ok(());
    }
    let id = message.id.clone();
    let target = user.osu_id;
    let text = pm.clone();
    let now = chrono::Utc::now().timestamp();
    let reserved = store.call(move |db| {
        Ok(db.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(twitch_message_id) DO NOTHING", params![id, target, text, now])? == 1)
    }).await?;
    if !reserved {
        return Ok(());
    }
    let id = message.id.clone();
    store.call(move |db| { db.execute("UPDATE deliveries SET status='sending' WHERE twitch_message_id=?1 AND status='pending'", [id])?; Ok(()) }).await?;
    if let Err(error) = providers.send_message(user.osu_id, &pm, false).await {
        let id = message.id.clone();
        let status = if error.uncertain {
            "uncertain"
        } else {
            "failed"
        };
        let description = error.to_string();
        store
            .call(move |db| {
                db.execute(
                    "UPDATE deliveries SET status=?1,error=?2 WHERE twitch_message_id=?3",
                    params![status, description, id],
                )?;
                Ok(())
            })
            .await?;
        return Err(error.into());
    }
    // One local transaction ties confirmed delivery, statistics and cooldown.
    // A crash before this commit leaves 'sending', hence uncertain on restart.
    let id = message.id.clone();
    let requester = message.author.clone();
    let channel = message.channel.clone();
    let author_id = message.author_id.clone();
    let map_id = map["id"].as_i64().context("beatmap omitted id")?;
    let mods = beatmap.mods.clone();
    let now = chrono::Utc::now().timestamp();
    let cooldown_now = clock.now();
    let update_cooldown = !user.settings.test;
    store.call(move |db| {
        let tx = db.transaction()?;
        tx.execute("UPDATE deliveries SET status='sent',error=NULL WHERE twitch_message_id=?1", [&id])?;
        tx.execute("INSERT INTO statistics(source_id,requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp) VALUES(?1,?2,?3,?4,?5,?6)", params![format!("twitch:{id}"), requester, map_id, channel, mods, now])?;
        if update_cooldown {
            tx.execute("INSERT INTO request_cooldowns(channel_id,requester_id,last_success) VALUES(?1,?2,?3) ON CONFLICT(channel_id,requester_id) DO UPDATE SET last_success=excluded.last_success", params![channel_id, author_id, cooldown_now])?;
        }
        tx.commit()?; Ok(())
    }).await?;
    if user.settings.echo {
        queue_echo(
            echoes,
            message,
            format!("{} - Request sent!", map_title(&map, &set)?),
        );
    }
    tracing::info!(channel = %message.channel, map_id, message_id = %message.id, "osu request delivered and recorded");
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .with_context(|| format!("beatmap metadata omitted {key}"))
}
fn map_title(map: &Value, set: &Value) -> Result<String> {
    Ok(format!(
        "{} - {} [{}]",
        field(set, "artist")?,
        field(set, "title")?,
        field(map, "version")?
    ))
}
fn prepare_message(message: &Message, map: &Value, set: &Value, mods: &str) -> Result<String> {
    let badge = if message.moderator {
        "[MOD] "
    } else if message.subscriber {
        "[SUB] "
    } else if message.vip {
        "[VIP] "
    } else {
        ""
    };
    let status = field(map, "status")?;
    let status = status
        .chars()
        .next()
        .map(|first| {
            format!(
                "{}{}",
                first.to_uppercase(),
                status[first.len_utf8()..].to_lowercase()
            )
        })
        .unwrap_or_default();
    let id = map["id"].as_i64().context("beatmap omitted id")?;
    let bpm = map["bpm"].as_f64().context("beatmap omitted bpm")?;
    let stars = map["difficulty_rating"]
        .as_f64()
        .context("beatmap omitted difficulty_rating")?;
    let length = parser::readable_duration(
        map["hit_length"]
            .as_u64()
            .context("beatmap omitted hit_length")?,
    );
    let points = if message.points { "+ USED POINTS" } else { "" };
    Ok(format!(
        "{badge}{} -> [{status}] [https://osu.ppy.sh/b/{id} {}] ({bpm} BPM, {stars:.2}*, {length}) {mods} {points}",
        message.author,
        map_title(map, set)?
    ))
}

fn sanitize_chat(text: &str, max: usize) -> String {
    let mut clean = String::with_capacity(text.len().min(max));
    for c in text.chars() {
        if clean.len() + c.len_utf8() > max {
            break;
        }
        clean.push(if c.is_control() { ' ' } else { c });
    }
    clean
}

#[cfg(test)]
mod tests;
