//! Twitch cloud-bot transport.
//!
//! Incoming chat and stream state arrive over one EventSub conduit shard
//! (`WebSocket`, `wss://eventsub.wss.twitch.tv/ws`). Outgoing chat uses the
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
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    future::pending,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

mod conduit;
mod eventsub;
mod streams;

use self::{
    conduit::{Conduit, Subscriptions},
    eventsub::{Backoff, Connection, Incoming, Welcome},
};

/// Room for the HTTP attach call after a welcome; Twitch closes an unattached
/// session ten seconds after the welcome.
const SHARD_ATTACH_DEADLINE: Duration = Duration::from_secs(9);
/// Revalidate the managed bot grant well inside Twitch's validation window.
const AUTH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const AUTH_DEADLINE: Duration = Duration::from_secs(20);
/// Remote subscription audit; heals subscriptions revoked behind our back.
const AUDIT_INTERVAL: Duration = Duration::from_secs(600);
/// Coalesce targeted category lookups triggered by `stream.online` bursts.
const TARGET_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_RESYNC_ATTEMPTS: u32 = 5;
const READER_QUEUE: usize = 64;
/// Time allowed for a graceful WebSocket close before the task is aborted.
const GRACEFUL_CLOSE: Duration = Duration::from_secs(2);
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
    let capacity = config.request_queue_capacity.clamp(1, 4096);
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
            requests_tx,
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
    requests: mpsc::Sender<(Message, Beatmap)>,
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
            &config,
            &store,
            &providers,
            &auth,
            identity,
            requests.clone(),
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

struct Runtime<'a> {
    config: &'a Config,
    store: &'a Store,
    providers: &'a Providers,
    auth: &'a TwitchAuth,
    identity: BotIdentity,
    requests: mpsc::Sender<(Message, Beatmap)>,
    conduit: Conduit,
    subs: Subscriptions,
    pending: Pending,
    seen: Seen,
    known: BTreeSet<i64>,
    known_granted: BTreeSet<i64>,
}

impl<'a> Runtime<'a> {
    async fn new(
        config: &'a Config,
        store: &'a Store,
        providers: &'a Providers,
        auth: &'a TwitchAuth,
        identity: BotIdentity,
        requests: mpsc::Sender<(Message, Beatmap)>,
    ) -> Result<Self> {
        let conduit = conduit::ensure(config, store, providers).await?;
        let subs = Subscriptions::new(conduit.id.clone(), identity.user_id.clone());
        Ok(Self {
            config,
            store,
            providers,
            auth,
            identity,
            requests,
            conduit,
            subs,
            pending: Pending::default(),
            seen: Seen::default(),
            known: BTreeSet::new(),
            known_granted: BTreeSet::new(),
        })
    }

    fn bot_id(&self) -> &str {
        &self.identity.user_id
    }

    /// Initial connect: welcome, shard attach, remote subscription list, owned
    /// subscriptions from the persisted state, then a snapshot that corrects
    /// it — so no transition can slip between subscription setup and the first
    /// reconciliation.
    async fn prepare(&mut self, shutdown: &CancellationToken) -> Result<Link> {
        let mut link = Link::new();
        let generation = link
            .open(&self.config.twitch_eventsub_url, shutdown)
            .await?;
        link.current = generation;
        let welcome = link.welcome(generation).await?;
        self.attach(&welcome.session_id, shutdown).await?;
        self.subs.refresh_remote(self.providers, shutdown).await?;
        // State and chat subscriptions are established before the snapshot so
        // no transition is missed; the snapshot then corrects any stale state.
        self.reconcile(shutdown).await?;
        self.known = self.store.enabled_twitch_ids().await?.into_iter().collect();
        self.known_granted = self.store.granted_twitch_ids().await?.into_iter().collect();
        self.snapshot().await;
        self.reconcile(shutdown).await?;
        tracing::info!(
            conduit = %self.conduit.id,
            session = %welcome.session_id,
            bot = %self.identity.login,
            "EventSub conduit connected"
        );
        Ok(link)
    }

    async fn attach(&self, session_id: &str, shutdown: &CancellationToken) -> Result<()> {
        let attach = timeout(
            SHARD_ATTACH_DEADLINE,
            conduit::attach(self.providers, &self.conduit.id, session_id),
        );
        tokio::select! {
            _ = shutdown.cancelled() => bail!("conduit shard attach cancelled"),
            result = attach => result.context("conduit shard attach exceeded the shard deadline")?,
        }
    }

    async fn reconcile(&mut self, shutdown: &CancellationToken) -> Result<()> {
        let grants = self.store.granted_twitch_ids().await?;
        let desired = conduit::desired(&grants, self.bot_id());
        let summary = self
            .subs
            .reconcile(self.providers, &desired, shutdown)
            .await?;
        if summary.created != 0 || summary.removed != 0 || summary.deferred != 0 {
            tracing::info!(
                created = summary.created,
                removed = summary.removed,
                kept = summary.kept,
                deferred = summary.deferred,
                "EventSub subscription reconciliation complete"
            );
        }
        Ok(())
    }

    /// A failed snapshot keeps the previous state; it never marks everyone offline.
    async fn snapshot(&mut self) {
        match streams::full(self.store, self.providers).await {
            Ok(skipped) => {
                for id in skipped {
                    self.pending.retry(id, 0);
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "stream snapshot failed; retaining the previous channel state"
                );
            }
        }
    }

    async fn run_pending(&mut self, shutdown: &CancellationToken) {
        let due = self.pending.take_due(Instant::now());
        if due.is_empty() {
            return;
        }
        let ids: Vec<i64> = due.iter().map(|(id, _)| *id).collect();
        match streams::targeted(self.store, self.providers, &ids).await {
            Ok(skipped) => {
                let skipped: HashSet<i64> = skipped.into_iter().collect();
                for (id, attempt) in due {
                    if skipped.contains(&id) && attempt < MAX_RESYNC_ATTEMPTS {
                        self.pending.retry(id, attempt);
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "targeted stream lookup failed; keeping known state and retrying");
                for (id, attempt) in due {
                    if attempt < MAX_RESYNC_ATTEMPTS {
                        self.pending.retry(id, attempt);
                    }
                }
            }
        }
        if let Err(error) = self.reconcile(shutdown).await {
            tracing::warn!(error = %error, "subscription reconcile after targeted lookup failed");
        }
    }

    /// Wake path for enable/grant/import/delete changes: poll newly enabled
    /// channels and newly active grants, including already-enabled legacy users.
    async fn refresh_membership(&mut self, shutdown: &CancellationToken) {
        if let Err(error) = self.reconcile(shutdown).await {
            tracing::warn!(error = %error, "subscription reconcile after a channel change failed");
        }
        let granted: BTreeSet<i64> = match self.store.granted_twitch_ids().await {
            Ok(ids) => ids.into_iter().collect(),
            Err(error) => {
                tracing::warn!(error = %error, "could not read granted channels");
                return;
            }
        };
        match self.store.enabled_twitch_ids().await {
            Ok(ids) => {
                let fresh: Vec<i64> = ids
                    .iter()
                    .copied()
                    .filter(|id| {
                        !self.known.contains(id)
                            || (granted.contains(id) && !self.known_granted.contains(id))
                    })
                    .collect();
                self.known = ids.into_iter().collect();
                self.known_granted = granted;
                if !fresh.is_empty() {
                    match streams::targeted(self.store, self.providers, &fresh).await {
                        Ok(skipped) => {
                            for id in skipped {
                                self.pending.retry(id, 0);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "initial state lookup for enabled or newly granted channels failed; retrying");
                            for id in fresh {
                                self.pending.retry(id, 0);
                            }
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(error = %error, "could not read enabled channels"),
        }
    }

    async fn run(&mut self, shutdown: &CancellationToken) -> Result<Outcome> {
        // Subscribe before startup reconciliation so an enable/grant that lands
        // mid-startup still wakes the targeted lookup after it.
        let mut channels = self.store.channel_changes();
        let mut link = self.prepare(shutdown).await?;
        let mut reconcile_tick = interval(Duration::from_secs(
            self.config.reconcile_interval_secs.max(1),
        ));
        reconcile_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        reconcile_tick.tick().await;
        let mut audit_tick = interval(AUDIT_INTERVAL);
        audit_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        audit_tick.tick().await;
        let mut auth_tick = interval(AUTH_INTERVAL);
        auth_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        auth_tick.tick().await;

        enum Wake {
            Cancel,
            Message(Option<(u64, Incoming)>),
            Channels(bool),
            Reconcile,
            Audit,
            Pending,
            Auth,
        }

        loop {
            let pending_deadline = self.pending.next_deadline();
            let pending_wake = async move {
                match pending_deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => pending().await,
                }
            };
            let wake = tokio::select! {
                _ = shutdown.cancelled() => Wake::Cancel,
                message = link.rx.recv() => Wake::Message(message),
                changed = channels.changed() => Wake::Channels(changed.is_ok()),
                _ = reconcile_tick.tick() => Wake::Reconcile,
                _ = audit_tick.tick() => Wake::Audit,
                _ = pending_wake => Wake::Pending,
                _ = auth_tick.tick() => Wake::Auth,
            };
            match wake {
                Wake::Cancel => {
                    link.shutdown().await;
                    return Ok(Outcome::Cancelled);
                }
                Wake::Message(message) => {
                    if let Some(outcome) = self.handle_message(&mut link, message, shutdown).await?
                    {
                        link.shutdown().await;
                        return Ok(outcome);
                    }
                }
                Wake::Channels(alive) => {
                    if alive {
                        self.refresh_membership(shutdown).await;
                    } else {
                        tracing::warn!(
                            "channel change notifications stopped; relying on the periodic audit"
                        );
                    }
                }
                Wake::Reconcile => {
                    self.snapshot().await;
                    if let Err(error) = self.reconcile(shutdown).await {
                        tracing::warn!(error = %error, "periodic subscription reconcile failed");
                    }
                }
                Wake::Audit => {
                    if let Err(error) = self.subs.refresh_remote(self.providers, shutdown).await {
                        tracing::warn!(error = %error, "EventSub subscription audit failed; keeping the local cache");
                    } else if let Err(error) = self.reconcile(shutdown).await {
                        tracing::warn!(error = %error, "audit subscription reconcile failed");
                    }
                }
                Wake::Pending => self.run_pending(shutdown).await,
                Wake::Auth => match timeout(AUTH_DEADLINE, self.auth.ready_bot(shutdown)).await {
                    Ok(Ok(Some(current))) if current.user_id == self.identity.user_id => {}
                    Ok(Ok(Some(changed))) => {
                        tracing::info!(login = %changed.login, "Twitch bot identity changed; restarting the transport");
                        link.shutdown().await;
                        return Ok(Outcome::Restart);
                    }
                    Ok(Ok(None)) => {
                        tracing::warn!(
                            "Twitch bot authorization was revoked; waiting for authorize-bot"
                        );
                        link.shutdown().await;
                        return Ok(Outcome::Restart);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            error = %error,
                            "Twitch bot revalidation failed; returning to the authorization wait"
                        );
                        link.shutdown().await;
                        return Ok(Outcome::Restart);
                    }
                    Err(_) => tracing::warn!("Twitch bot revalidation timed out"),
                },
            }
        }
    }

    async fn handle_message(
        &mut self,
        link: &mut Link,
        message: Option<(u64, Incoming)>,
        shutdown: &CancellationToken,
    ) -> Result<Option<Outcome>> {
        let Some((generation, incoming)) = message else {
            return Err(anyhow!("EventSub reader channel closed"));
        };
        let known = generation == link.current
            || Some(generation) == link.pending
            || link.retired.contains(&generation);
        if !known {
            return Ok(None);
        }
        match incoming {
            Incoming::Welcome(welcome) => {
                if Some(generation) == link.pending {
                    self.attach(&welcome.session_id, shutdown).await?;
                    link.complete(generation).await;
                    tracing::info!(
                        session = %welcome.session_id,
                        "EventSub connection handoff complete"
                    );
                    // Session recovery: re-list owned subscriptions and resync
                    // state now instead of waiting for the periodic audit.
                    if let Err(error) = self.subs.refresh_remote(self.providers, shutdown).await {
                        tracing::warn!(error = %error, "subscription refresh after session recovery failed");
                    }
                    if let Err(error) = self.reconcile(shutdown).await {
                        tracing::warn!(error = %error, "reconcile after session recovery failed");
                    }
                    self.snapshot().await;
                } else if generation == link.current {
                    tracing::debug!(session = %welcome.session_id, "duplicate welcome on the active session");
                }
            }
            Incoming::Keepalive => {}
            Incoming::Ignored => {}
            Incoming::Notification {
                subscription,
                event,
                timestamp,
                message_id,
            } => {
                // A superseded but still draining connection may carry the last
                // events of the handoff; timestamps and message ids deduplicate.
                self.notification(&subscription, &event, &timestamp, &message_id)
                    .await?;
            }
            Incoming::Reconnect { url } => {
                if generation != link.current || link.pending.is_some() {
                    return Ok(None);
                }
                let url = match eventsub::validate_reconnect_url(
                    &self.config.twitch_eventsub_url,
                    &url,
                ) {
                    Ok(url) => url,
                    Err(error) => {
                        tracing::warn!(error = %error, "refusing an EventSub reconnect URL outside the configured origin");
                        return Ok(None);
                    }
                };
                match link.open(&url, shutdown).await {
                    Ok(generation) => {
                        link.pending = Some(generation);
                        tracing::info!(
                            generation,
                            "opened the replacement EventSub session before closing the old one"
                        );
                    }
                    Err(error) => {
                        // Keep the old connection; Twitch closes it with 4004 if
                        // the handoff never completes, which triggers a reconnect.
                        tracing::warn!(error = %error, "could not open the EventSub reconnect URL; keeping the current session");
                    }
                }
            }
            Incoming::Revocation { subscription } => {
                // Revocations are idempotent, so a draining old session may
                // deliver one that the replacement never sees.
                self.revocation(&subscription, shutdown).await?;
            }
            Incoming::Close { code, reason } => {
                if generation == link.current {
                    return Err(anyhow!("EventSub closed the connection: {code} {reason}"));
                }
                if Some(generation) == link.pending {
                    tracing::warn!(code, reason = %reason, "replacement EventSub session closed before welcome; keeping the current session");
                    link.abandon(generation).await;
                }
            }
            Incoming::Failed(error) => {
                if generation == link.current {
                    return Err(anyhow!("EventSub transport failed: {error}"));
                }
                if Some(generation) == link.pending {
                    tracing::warn!(error = %error, "replacement EventSub session failed before welcome; keeping the current session");
                    link.abandon(generation).await;
                }
            }
        }
        Ok(None)
    }

    async fn revocation(
        &mut self,
        subscription: &Value,
        shutdown: &CancellationToken,
    ) -> Result<()> {
        let kind = subscription["type"].as_str().unwrap_or_default();
        let status = subscription["status"].as_str().unwrap_or_default();
        let ours = subscription["transport"]["method"].as_str() == Some("conduit")
            && subscription["transport"]["conduit_id"].as_str() == Some(self.conduit.id.as_str());
        if !ours || conduit::owned_version(kind).is_none() {
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
            self.subs.forget_kind(kind, id);
            tracing::warn!(
                broadcaster = id,
                "channel grant marked revoked; the broadcaster must reauthorize before chat resumes"
            );
            if let Err(error) = self.reconcile(shutdown).await {
                tracing::warn!(error = %error, "subscription reconcile after revocation failed");
            }
        } else if let Some(id) = broadcaster {
            self.subs.forget_kind(kind, id);
        }
        Ok(())
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
                if self.requests.try_send((chat.message, beatmap)).is_err() {
                    tracing::warn!(
                        channel = %event["broadcaster_user_login"].as_str().unwrap_or_default(),
                        "request queue is full; incoming request rejected"
                    );
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
                    self.pending.schedule(broadcaster);
                    // The category is unknown until the targeted lookup lands.
                }
            }
            "stream.offline" => {
                self.pending.cancel(broadcaster);
                if self
                    .store
                    .apply_stream_event(broadcaster, event_login(event), StreamEvent::Offline, at)
                    .await?
                {
                    tracing::info!(
                        broadcaster,
                        "stream went offline; request acceptance removed"
                    );
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

/// Debounced targeted category lookups queued by `stream.online`.
#[derive(Default)]
struct Pending {
    due: BTreeMap<i64, (Instant, u32)>,
}

impl Pending {
    fn schedule(&mut self, id: i64) {
        self.due
            .entry(id)
            .or_insert_with(|| (Instant::now() + TARGET_DEBOUNCE, 0));
    }

    fn cancel(&mut self, id: i64) {
        self.due.remove(&id);
    }

    fn retry(&mut self, id: i64, attempt: u32) {
        let delay = Duration::from_secs((1u64 << attempt.min(6)).min(60));
        self.due.insert(id, (Instant::now() + delay, attempt + 1));
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.due.values().map(|(due, _)| *due).min()
    }

    fn take_due(&mut self, now: Instant) -> Vec<(i64, u32)> {
        let due: Vec<(i64, u32)> = self
            .due
            .iter()
            .filter(|(_, (deadline, _))| *deadline <= now)
            .map(|(id, (_, attempt))| (*id, *attempt))
            .collect();
        for (id, _) in &due {
            self.due.remove(id);
        }
        due
    }
}

/// One owned EventSub WebSocket generation plus any handoff in flight.
struct Link {
    rx: mpsc::Receiver<(u64, Incoming)>,
    tx: mpsc::Sender<(u64, Incoming)>,
    readers: BTreeMap<u64, Reader>,
    current: u64,
    pending: Option<u64>,
    next_generation: u64,
    /// Recently superseded generations. Queued notifications from them are
    /// still processed (and deduplicated) so a handoff cannot drop events.
    retired: VecDeque<u64>,
}

impl Link {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel(READER_QUEUE);
        Self {
            rx,
            tx,
            readers: BTreeMap::new(),
            current: 0,
            pending: None,
            next_generation: 0,
            retired: VecDeque::new(),
        }
    }

    /// Open a connection and spawn its reader. The welcome arrives through the
    /// shared channel so the old connection keeps delivering during a handoff.
    async fn open(&mut self, url: &str, shutdown: &CancellationToken) -> Result<u64> {
        let connection = Connection::open(url).await?;
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let tx = self.tx.clone();
        let token = shutdown.clone();
        let local = CancellationToken::new();
        let local_token = local.clone();
        let reader = Reader {
            handle: tokio::spawn(async move {
                let mut connection = connection;
                let mut idle = eventsub::WELCOME_TIMEOUT;
                let mut deadline = Instant::now() + idle;
                loop {
                    let result = tokio::select! {
                        _ = token.cancelled() => None,
                        _ = local_token.cancelled() => None,
                        result = connection.read(deadline) => Some(result),
                    };
                    let Some(result) = result else {
                        connection.close().await;
                        return;
                    };
                    match result {
                        Ok(incoming) => {
                            match &incoming {
                                Incoming::Welcome(welcome) => {
                                    idle = welcome.keepalive + Duration::from_secs(1);
                                    deadline = Instant::now() + idle;
                                }
                                // Only protocol liveness resets the timer; ping and
                                // pong frames deliberately do not.
                                Incoming::Keepalive | Incoming::Notification { .. } => {
                                    deadline = Instant::now() + idle;
                                }
                                _ => {}
                            }
                            let sent = tokio::select! {
                                _ = token.cancelled() => false,
                                _ = local_token.cancelled() => false,
                                result = tx.send((generation, incoming)) => result.is_ok(),
                            };
                            if !sent {
                                connection.close().await;
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = tx
                                .send((generation, Incoming::Failed(format!("{error:#}"))))
                                .await;
                            return;
                        }
                    }
                }
            }),
            cancel: local,
        };
        self.readers.insert(generation, reader);
        Ok(generation)
    }

    async fn welcome(&mut self, generation: u64) -> Result<Welcome> {
        let deadline = Instant::now() + eventsub::WELCOME_TIMEOUT + Duration::from_secs(2);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = timeout(remaining, self.rx.recv())
                .await
                .map_err(|_| anyhow!("EventSub welcome timed out"))?;
            let Some((seen, incoming)) = message else {
                bail!("EventSub closed before the welcome message");
            };
            if seen != generation {
                continue;
            }
            match incoming {
                Incoming::Welcome(welcome) => return Ok(welcome),
                Incoming::Failed(error) => {
                    bail!("EventSub connection failed before welcome: {error}")
                }
                Incoming::Close { code, reason } => {
                    bail!("EventSub closed before welcome: {code} {reason}")
                }
                _ => {}
            }
        }
    }

    /// Close the old connection only after the replacement answered its welcome.
    async fn complete(&mut self, generation: u64) {
        let old = self.current;
        if let Some(reader) = self.readers.remove(&old) {
            reader.close().await;
        }
        self.retired.push_back(old);
        while self.retired.len() > 4 {
            self.retired.pop_front();
        }
        self.current = generation;
        self.pending = None;
    }

    async fn abandon(&mut self, generation: u64) {
        if let Some(reader) = self.readers.remove(&generation) {
            reader.close().await;
        }
        self.pending = None;
    }

    /// Gracefully close every connection, then abort whatever remains.
    async fn shutdown(&mut self) {
        let readers = std::mem::take(&mut self.readers);
        for (_, reader) in readers {
            reader.close().await;
        }
    }

    fn close_all(&mut self) {
        for (_, reader) in std::mem::take(&mut self.readers) {
            reader.cancel.cancel();
            reader.handle.abort();
        }
    }
}

struct Reader {
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Reader {
    /// Ask the reader to send a close frame and wait briefly for it.
    async fn close(self) {
        self.cancel.cancel();
        let _ = timeout(GRACEFUL_CLOSE, self.handle).await;
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // Readers hold the connection; without this they would linger until the
        // channel closes or the keepalive deadline expires.
        self.close_all();
    }
}

async fn process_requests(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    mut queue: mpsc::Receiver<(Message, Beatmap)>,
    echoes: mpsc::Sender<Echo>,
    shutdown: CancellationToken,
    clock: Clock,
) {
    loop {
        let (message, beatmap) = tokio::select! { _ = shutdown.cancelled() => return, message = queue.recv() => match message { Some(message) => message, None => return } };
        // Do not cancel this future midway through bookkeeping: the provider
        // records a possibly transmitted PM as uncertain on cancellation.
        if let Err(error) = process_one(
            &config, &store, &providers, &echoes, &message, &beatmap, clock,
        )
        .await
        {
            tracing::error!(channel = %message.channel, message_id = %message.id, error = %error, "request processing failed");
        }
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
