//! EventSub transport: WebSocket framing plus independently supervised conduit
//! shards. Topology mutation has one owner; socket recovery never resets
//! another shard or the subscription cache.

use super::backoff::Backoff;
use super::conduit::{self, Conduit};
use crate::{config::Config, providers::Providers};
use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::StreamExt;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::{Future, pending},
    pin::pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

// --- WebSocket protocol ----------------------------------------------------

/// A validated EventSub message is small; oversized frames are a protocol error.
pub(super) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_FRAME_BYTES: usize = 512 * 1024;
/// Time allowed for the TCP/TLS/HTTP upgrade, and separately for the welcome.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
/// Used when a welcome omits `keepalive_timeout_seconds`.
const KEEPALIVE_FALLBACK: Duration = Duration::from_secs(30);

const MAX_SHARDS: u32 = 64;
const CONNECT_ATTEMPTS: usize = 4;
const EVENT_QUEUE: usize = 1024;
const READER_QUEUE: usize = 64;
pub(super) const SHARD_ATTACH_DEADLINE: Duration = Duration::from_secs(9);
const GRACEFUL_CLOSE: Duration = Duration::from_secs(2);
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
pub(super) struct Welcome {
    pub(super) session_id: String,
    pub(super) keepalive: Duration,
}

/// One decoded server frame. `Ignored` covers control frames and unknown
/// message types; ping and pong never extend the keepalive deadline.
#[derive(Debug)]
pub(super) enum Incoming {
    Welcome(Welcome),
    Keepalive,
    Notification {
        subscription: Value,
        event: Value,
        timestamp: String,
        message_id: String,
    },
    Reconnect {
        url: String,
    },
    Revocation {
        subscription: Value,
    },
    Close {
        code: u16,
        reason: String,
    },
    Failed(String),
    Ignored,
}

pub(super) struct Connection {
    socket: Socket,
}

impl Connection {
    pub(super) async fn open(url: &str) -> Result<Self> {
        let parsed = Url::parse(url).context("invalid EventSub WebSocket URL")?;
        match parsed.scheme() {
            "wss" => {}
            "ws" => ensure!(
                is_loopback(&parsed),
                "plain ws EventSub URLs are only accepted for loopback test hosts"
            ),
            scheme => bail!("EventSub WebSocket URL must use ws or wss, not {scheme}"),
        }
        ensure!(parsed.host().is_some(), "EventSub URL omitted a host");
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES))
            .accept_unmasked_frames(false);
        let (socket, _) = timeout(
            HANDSHAKE_TIMEOUT,
            connect_async_with_config(url, Some(config), true),
        )
        .await
        .map_err(|_| anyhow!("EventSub WebSocket connect timed out"))?
        .context("EventSub WebSocket connect failed")?;
        Ok(Self { socket })
    }

    /// Send a close frame so the server sees a clean shutdown.
    pub(super) async fn close(&mut self) {
        let _ = self.socket.close(None).await;
    }

    /// Read the next message before the application keepalive deadline.
    ///
    /// The caller extends the deadline only for notifications and keepalives;
    /// ping and pong frames are answered by tungstenite but never count as
    /// liveness, exactly as the Twitch protocol requires. A timeout closes the
    /// connection: a partial frame cannot be resumed, and the supervisor
    /// reconnects instead.
    pub(super) async fn read(&mut self, deadline: Instant) -> Result<Incoming> {
        let mut next = pin!(self.socket.next());
        let message = tokio::select! {
            _ = sleep_until(deadline) => {
                bail!("EventSub keepalive deadline elapsed with no notification or keepalive")
            }
            message = &mut next => message,
        };
        match message.transpose()? {
            None => bail!("EventSub WebSocket closed without a close frame"),
            Some(Message::Text(text)) => parse(&text),
            Some(Message::Binary(bytes)) => {
                bail!("unexpected binary EventSub frame ({} bytes)", bytes.len())
            }
            // tungstenite answers pings; neither ping nor pong resets the timer.
            Some(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => Ok(Incoming::Ignored),
            Some(Message::Close(frame)) => Ok(Incoming::Close {
                code: frame.as_ref().map_or(1005, |frame| frame.code.into()),
                reason: frame
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default(),
            }),
        }
    }
}

pub(super) fn parse(text: &str) -> Result<Incoming> {
    ensure!(
        text.len() <= MAX_MESSAGE_BYTES,
        "oversized EventSub message"
    );
    let value: Value = serde_json::from_str(text).context("invalid EventSub JSON")?;
    let metadata = &value["metadata"];
    let payload = &value["payload"];
    let message_type = metadata["message_type"]
        .as_str()
        .context("EventSub message omitted metadata.message_type")?;
    Ok(match message_type {
        "session_welcome" => {
            let session = &payload["session"];
            let session_id = session["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("welcome omitted the session id")?
                .to_owned();
            let keepalive = session["keepalive_timeout_seconds"]
                .as_u64()
                .filter(|seconds| (10..=600).contains(seconds))
                .map(Duration::from_secs)
                .unwrap_or(KEEPALIVE_FALLBACK);
            Incoming::Welcome(Welcome {
                session_id,
                keepalive,
            })
        }
        "session_keepalive" => Incoming::Keepalive,
        "notification" => Incoming::Notification {
            message_id: metadata["message_id"]
                .as_str()
                .context("notification omitted message_id")?
                .to_owned(),
            timestamp: metadata["message_timestamp"]
                .as_str()
                .context("notification omitted message_timestamp")?
                .to_owned(),
            subscription: payload["subscription"].clone(),
            event: payload["event"].clone(),
        },
        "session_reconnect" => Incoming::Reconnect {
            url: payload["session"]["reconnect_url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .context("reconnect message omitted the reconnect URL")?
                .to_owned(),
        },
        "revocation" => Incoming::Revocation {
            subscription: payload["subscription"].clone(),
        },
        other => {
            tracing::debug!(
                message_type = other,
                "ignoring unknown EventSub message type"
            );
            Incoming::Ignored
        }
    })
}

/// RFC 3339 with nanoseconds, as used by every EventSub timestamp.
pub(super) fn timestamp_nanos(value: &str) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid EventSub timestamp {value:?}"))?
        .timestamp_nanos_opt()
        .context("EventSub timestamp is outside the supported nanosecond range")
}

/// The reconnect URL is used verbatim, but only when it stays on the configured
/// origin; Twitch never redirects EventSub to another host.
pub(super) fn validate_reconnect_url(configured: &str, candidate: &str) -> Result<String> {
    let base = Url::parse(configured).context("invalid configured EventSub URL")?;
    let next = Url::parse(candidate).context("invalid EventSub reconnect URL")?;
    ensure!(
        matches!(next.scheme(), "ws" | "wss"),
        "reconnect URL is not a WebSocket URL"
    );
    ensure!(
        next.scheme() == base.scheme(),
        "reconnect URL changed the transport scheme"
    );
    ensure!(next.host() == base.host(), "reconnect URL changed the host");
    ensure!(
        next.port_or_known_default() == base.port_or_known_default(),
        "reconnect URL changed the port"
    );
    if next.scheme() == "ws" {
        ensure!(
            is_loopback(&next),
            "plain ws reconnect URLs are only accepted for loopback test hosts"
        );
    }
    Ok(candidate.to_owned())
}

pub(super) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        // RFC 6761 reserves localhost and its subdomains for loopback use;
        // integration fixtures use names such as recover.localhost to exercise
        // real DNS failure/recovery without weakening plain-ws elsewhere.
        Some(Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
        None => false,
    }
}

// --- Shard supervision -----------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Topology {
    pub(super) remote_count: u32,
    /// Acknowledged attachments for the observed topology; zero while an
    /// ambiguous topology mutation awaits authoritative observation.
    pub(super) attached_count: u32,
}

pub(super) struct Event {
    pub(super) shard_id: u32,
    pub(super) incoming: Incoming,
}

pub(super) struct Pool {
    pub(super) events: mpsc::Receiver<Event>,
    pub(super) topology: watch::Receiver<Topology>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl Pool {
    pub(super) fn start(
        config: Arc<Config>,
        providers: Providers,
        conduit: Conduit,
        targets: watch::Receiver<u32>,
        shutdown: CancellationToken,
    ) -> Self {
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
        let (topology_tx, topology) = watch::channel(Topology {
            remote_count: conduit.shard_count,
            attached_count: 0,
        });
        let cancel = shutdown.child_token();
        let handle = tokio::spawn(manage(
            config,
            providers,
            conduit,
            targets,
            events_tx,
            topology_tx,
            cancel.clone(),
        ));
        Self {
            events,
            topology,
            cancel,
            handle,
        }
    }

    pub(super) async fn shutdown(self) {
        self.cancel.cancel();
        // A receiver held by the caller must not prevent supervised shutdown.
        drop(self.events);
        if let Err(error) = self.handle.await {
            tracing::error!(%error, "EventSub shard pool task failed");
        }
    }
}

pub(super) struct State {
    pub(super) remote_count: u32,
    pub(super) attached: BTreeSet<u32>,
    pub(super) uncertain: bool,
}

impl State {
    pub(super) fn topology(&self) -> Topology {
        Topology {
            remote_count: self.remote_count,
            // A mutation with an unknown outcome cannot acknowledge a complete
            // topology. Keep the actual healthy IDs internally while gating
            // subscription creation until the authoritative read succeeds.
            attached_count: if self.uncertain {
                0
            } else {
                self.attached.len() as u32
            },
        }
    }

    pub(super) fn candidate(&self, target: u32, cap: u32) -> Option<u32> {
        (!self.uncertain
            && self.remote_count > 0
            && self.remote_count < MAX_SHARDS
            && self.remote_count < cap
            && self.remote_count < target
            && self.attached.len() == self.remote_count as usize)
            .then_some(self.remote_count)
    }

    pub(super) fn observe(&mut self, remote_count: u32) {
        self.remote_count = remote_count;
        self.attached
            .retain(|id| *id < remote_count && remote_count <= MAX_SHARDS);
    }

    pub(super) fn set_attached(&mut self, id: u32, attached: bool) {
        if attached && id < self.remote_count && self.remote_count <= MAX_SHARDS {
            self.attached.insert(id);
        } else {
            self.attached.remove(&id);
        }
    }
}

enum Control {
    Attached {
        id: u32,
        attached: bool,
    },
    Grow {
        id: u32,
        deadline: Instant,
        reply: oneshot::Sender<Result<()>>,
    },
}

async fn manage(
    config: Arc<Config>,
    providers: Providers,
    conduit: Conduit,
    mut targets: watch::Receiver<u32>,
    events: mpsc::Sender<Event>,
    topology: watch::Sender<Topology>,
    shutdown: CancellationToken,
) {
    let mut state = State {
        remote_count: conduit.shard_count,
        attached: BTreeSet::new(),
        uncertain: false,
    };
    let attempts = Arc::new(Semaphore::new(CONNECT_ATTEMPTS));
    let (tx, mut rx) = mpsc::channel(128);
    let mut jobs = JoinSet::new();
    let mut workers: BTreeMap<u32, CancellationToken> = BTreeMap::new();
    let mut task_ids = BTreeMap::new();
    let mut observe_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
    let mut observe_at = Instant::now();
    let mut targets_open = true;
    let cap = config.eventsub_max_shards.min(MAX_SHARDS);
    let mut reported_remote = None;

    loop {
        if reported_remote != Some(state.remote_count) {
            if state.remote_count > MAX_SHARDS {
                tracing::error!(
                    remote_count = state.remote_count,
                    hard_bound = MAX_SHARDS,
                    "EventSub bot degraded: manually reduce the owned conduit to at most 64 shards; leaving it untouched"
                );
            } else if state.remote_count > cap {
                tracing::warn!(
                    remote_count = state.remote_count,
                    configured_cap = cap,
                    "persisted conduit exceeds the automatic-growth cap; attaching all remote shards and freezing growth"
                );
            }
            reported_remote = Some(state.remote_count);
        }
        topology.send_if_modified(|value| {
            let next = state.topology();
            if *value == next {
                false
            } else {
                *value = next;
                true
            }
        });
        let target = (*targets.borrow()).max(1).min(cap);
        let candidate = state.candidate(target, cap);
        for (&id, token) in &workers {
            if state.remote_count > MAX_SHARDS
                || (id >= state.remote_count && Some(id) != candidate)
            {
                token.cancel();
            }
        }
        if (1..=MAX_SHARDS).contains(&state.remote_count) {
            for id in (0..state.remote_count).chain(candidate) {
                if workers.contains_key(&id) {
                    continue;
                }
                let token = shutdown.child_token();
                workers.insert(id, token.clone());
                let worker = Shard {
                    id,
                    config: config.clone(),
                    providers: providers.clone(),
                    conduit: conduit.id.clone(),
                    events: events.clone(),
                    control: tx.clone(),
                    topology: topology.subscribe(),
                    attempts: attempts.clone(),
                    shutdown: token,
                };
                let handle = jobs.spawn(async move {
                    worker.run().await;
                    id
                });
                task_ids.insert(handle.id(), id);
            }
        }

        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            changed = targets.changed(), if targets_open => {
                if changed.is_err() { targets_open = false; }
            }
            result = jobs.join_next_with_id(), if !jobs.is_empty() => {
                match result {
                    Some(Ok((task, id))) => {
                        task_ids.remove(&task);
                        workers.remove(&id);
                        state.set_attached(id, false);
                    }
                    Some(Err(error)) => {
                        if let Some(id) = task_ids.remove(&error.id()) {
                            workers.remove(&id);
                            state.set_attached(id, false);
                        }
                        tracing::error!(%error, "EventSub shard supervisor failed");
                    }
                    None => {}
                }
            }
            message = rx.recv() => match message {
                Some(Control::Attached { id, attached }) => state.set_attached(id, attached),
                Some(Control::Grow { id, deadline, reply }) => {
                    if deadline <= Instant::now() || reply.is_closed()
                        || state.candidate((*targets.borrow()).max(1).min(cap), cap) != Some(id)
                    {
                        let _ = reply.send(Err(anyhow!("shard growth is not currently eligible")));
                        continue;
                    }
                    let result = tokio::select! {
                        _ = shutdown.cancelled() => break,
                        result = timeout_at(deadline, conduit::resize(&providers, &conduit.id, id + 1)) =>
                            result.map_err(|_| anyhow!("EventSub resize exceeded the welcome/attach deadline")).and_then(|result| result),
                    };
                    match result {
                        Ok(()) => {
                            state.observe(id + 1);
                            topology.send_replace(state.topology());
                            let _ = reply.send(Ok(()));
                        }
                        Err(error) => {
                            // Even a timeout can mean the PATCH committed remotely.
                            // No further PATCH is allowed until an authoritative read.
                            state.uncertain = true;
                            observe_at = Instant::now();
                            tracing::warn!(shard_id = id, %error, "EventSub growth outcome ambiguous; reobserving the owned conduit before retry");
                            let _ = reply.send(Err(error));
                        }
                    }
                }
                None => break,
            },
            _ = sleep_until(observe_at), if state.uncertain => {
                let observed = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    result = conduit::observe(&providers, &conduit.id) => result,
                };
                match observed {
                    Ok(observed) => {
                        state.observe(observed.shard_count);
                        state.uncertain = false;
                        observe_retry.reset();
                    }
                    Err(error) => {
                        let delay = observe_retry.next_delay();
                        observe_at = Instant::now() + delay;
                        tracing::warn!(%error, retry_ms = delay.as_millis(), "cannot confirm EventSub remote topology; growth remains frozen");
                    }
                }
            }
        }
    }
    shutdown.cancel();
    for token in workers.values() {
        token.cancel();
    }
    while let Some(result) = jobs.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "EventSub shard supervisor failed during shutdown");
        }
    }
}

struct Shard {
    id: u32,
    config: Arc<Config>,
    providers: Providers,
    conduit: String,
    events: mpsc::Sender<Event>,
    control: mpsc::Sender<Control>,
    topology: watch::Receiver<Topology>,
    attempts: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl Shard {
    async fn attached(&self, attached: bool) -> bool {
        tokio::select! {
            _ = self.shutdown.cancelled() => false,
            result = self.control.send(Control::Attached { id: self.id, attached }) => result.is_ok(),
        }
    }

    fn connect(&self, url: String) -> Attempt {
        let token = self.shutdown.child_token();
        let future = prepare(
            self.id,
            url,
            self.providers.clone(),
            self.conduit.clone(),
            self.topology.clone(),
            self.control.clone(),
            self.attempts.clone(),
            token.clone(),
        );
        Attempt {
            handle: tokio::spawn(future),
            cancel: token,
        }
    }

    async fn run(self) {
        let mut link = Link::new();
        let mut attempt = Some(self.connect(self.config.twitch_eventsub_url.clone()));
        // Retain the validated handoff destination across transient failures.
        // The old generation continues serving until a replacement attaches.
        let mut reconnect_url = None;
        let mut retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut retry_at = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                result = finish_attempt(&mut attempt), if attempt.is_some() => {
                    // The handle was polled by mutable reference and is now joined.
                    attempt.take();
                    match result {
                        Ok(Ok(prepared)) => {
                            tracing::info!(shard_id = self.id, session = %prepared.welcome.session_id, "EventSub shard attached");
                            let generation = link.start(prepared, &self.shutdown);
                            let old = link.current.replace(generation);
                            if !self.attached(true).await { break; }
                            retry.reset();
                            reconnect_url = None;
                            if let Some(old) = old { link.retire(old).await; }
                        }
                        result => {
                            let error = match result {
                                Ok(Err(error)) => error,
                                Err(error) => anyhow!(error),
                                Ok(Ok(_)) => unreachable!(),
                            };
                            let delay = retry.next_delay();
                            retry_at = Instant::now() + delay;
                            tracing::warn!(shard_id = self.id, %error, retry_ms = delay.as_millis(),
                                "EventSub shard connection attempt failed; healthy peers remain active");
                        }
                    }
                }
                _ = sleep_until(retry_at), if attempt.is_none() && (link.current.is_none() || reconnect_url.is_some()) => {
                    let url = reconnect_url.clone().unwrap_or_else(|| self.config.twitch_eventsub_url.clone());
                    attempt = Some(self.connect(url));
                }
                message = link.rx.recv() => {
                    let Some((generation, incoming)) = message else { break; };
                    if !link.known(generation) { continue; }
                    match incoming {
                        incoming @ (Incoming::Notification { .. } | Incoming::Revocation { .. }) => {
                            let sent = tokio::select! {
                                _ = self.shutdown.cancelled() => false,
                                result = self.events.send(Event { shard_id: self.id, incoming }) => result.is_ok(),
                            };
                            if !sent { break; }
                        }
                        Incoming::Reconnect { url } if Some(generation) == link.current
                            && attempt.is_none() && reconnect_url.is_none() => {
                            match validate_reconnect_url(&self.config.twitch_eventsub_url, &url) {
                                Ok(url) => {
                                    attempt = Some(self.connect(url.clone()));
                                    reconnect_url = Some(url);
                                }
                                Err(error) => tracing::warn!(shard_id = self.id, %error,
                                    "refusing an EventSub reconnect URL outside the configured origin"),
                            }
                        }
                        Incoming::Close { code, reason } if Some(generation) == link.current => {
                            tracing::warn!(shard_id = self.id, code, %reason, "EventSub shard socket closed");
                            link.current = None;
                            reconnect_url = None;
                            if !self.attached(false).await { break; }
                            link.retire(generation).await;
                            retry_at = Instant::now() + retry.next_delay();
                        }
                        Incoming::Failed(error) if Some(generation) == link.current => {
                            tracing::warn!(shard_id = self.id, %error, "EventSub shard transport failed");
                            link.current = None;
                            reconnect_url = None;
                            if !self.attached(false).await { break; }
                            link.retire(generation).await;
                            retry_at = Instant::now() + retry.next_delay();
                        }
                        Incoming::Welcome(welcome) => tracing::debug!(shard_id = self.id, session = %welcome.session_id, "duplicate EventSub welcome"),
                        Incoming::Close { code, reason } => tracing::debug!(shard_id = self.id, code, %reason, "retired EventSub socket closed"),
                        Incoming::Failed(error) => tracing::debug!(shard_id = self.id, %error, "retired EventSub socket failed"),
                        _ => {}
                    }
                }
            }
        }
        if let Some(attempt) = attempt {
            attempt.stop().await;
        }
        link.shutdown().await;
    }
}

struct Prepared {
    connection: Connection,
    welcome: Welcome,
    welcomed_at: Instant,
}

struct Attempt {
    handle: JoinHandle<Result<Prepared>>,
    cancel: CancellationToken,
}

impl Attempt {
    async fn stop(mut self) {
        self.cancel.cancel();
        // prepare observes cancellation even while waiting for an HTTP call.
        // A result racing cancellation still owns a socket that must be closed.
        if let Ok(Ok(mut prepared)) = (&mut self.handle).await {
            let _ = timeout(GRACEFUL_CLOSE, prepared.connection.close()).await;
        }
    }
}

async fn finish_attempt(
    attempt: &mut Option<Attempt>,
) -> std::result::Result<Result<Prepared>, tokio::task::JoinError> {
    match attempt {
        Some(attempt) => (&mut attempt.handle).await,
        None => pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn prepare(
    id: u32,
    url: String,
    providers: Providers,
    conduit: String,
    topology: watch::Receiver<Topology>,
    control: mpsc::Sender<Control>,
    attempts: Arc<Semaphore>,
    shutdown: CancellationToken,
) -> Result<Prepared> {
    let _permit = tokio::select! {
        _ = shutdown.cancelled() => bail!("EventSub connection attempt cancelled"),
        permit = attempts.acquire_owned() => permit?,
    };
    let mut connection = tokio::select! {
        _ = shutdown.cancelled() => bail!("EventSub connection attempt cancelled"),
        result = Connection::open(&url) => result?,
    };
    let welcome = async {
        let welcome_deadline = Instant::now() + WELCOME_TIMEOUT;
        let welcome = loop {
            match connection.read(welcome_deadline).await? {
                Incoming::Welcome(welcome) => break welcome,
                Incoming::Ignored => {}
                Incoming::Close { code, reason } => {
                    bail!("EventSub closed before welcome: {code} {reason}")
                }
                Incoming::Failed(error) => bail!("EventSub failed before welcome: {error}"),
                _ => bail!("EventSub sent an application message before welcome"),
            }
        };
        let welcomed_at = Instant::now();
        let deadline = welcomed_at + SHARD_ATTACH_DEADLINE;
        attach_before(deadline, async {
            let remote_count = topology.borrow().remote_count;
            ensure!(
                remote_count <= MAX_SHARDS,
                "owned conduit exceeds the 64-shard implementation bound"
            );
            if id >= remote_count {
                ensure!(
                    id == remote_count,
                    "candidate shard no longer follows the observed topology"
                );
                let (reply, received) = oneshot::channel();
                control
                    .send(Control::Grow {
                        id,
                        deadline,
                        reply,
                    })
                    .await
                    .map_err(|_| anyhow!("EventSub topology supervisor closed"))?;
                received
                    .await
                    .map_err(|_| anyhow!("EventSub growth acknowledgement closed"))??;
            }
            conduit::attach(&providers, &conduit, id, &welcome.session_id).await
        })
        .await?;
        Ok((welcome, welcomed_at))
    };
    let result = tokio::select! {
        _ = shutdown.cancelled() => Err(anyhow!("EventSub connection attempt cancelled")),
        result = welcome => result,
    };
    match result {
        Ok((welcome, welcomed_at)) => Ok(Prepared {
            connection,
            welcome,
            welcomed_at,
        }),
        Err(error) => {
            let _ = timeout(GRACEFUL_CLOSE, connection.close()).await;
            Err(error)
        }
    }
}

pub(super) async fn attach_before<T>(
    deadline: Instant,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    timeout_at(deadline, operation)
        .await
        .map_err(|_| anyhow!("EventSub resize/attach exceeded nine seconds after welcome"))?
}

/// Readers only decode and enforce protocol liveness. Each shard owns its
/// generation queue, so a failed socket never blocks a peer's supervision.
pub(super) struct Link {
    pub(super) rx: mpsc::Receiver<(u64, Incoming)>,
    tx: mpsc::Sender<(u64, Incoming)>,
    pub(super) readers: BTreeMap<u64, Reader>,
    pub(super) current: Option<u64>,
    next_generation: u64,
    retired: VecDeque<u64>,
}

impl Link {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel(READER_QUEUE);
        Self {
            rx,
            tx,
            readers: BTreeMap::new(),
            current: None,
            next_generation: 0,
            retired: VecDeque::new(),
        }
    }

    fn known(&self, generation: u64) -> bool {
        self.current == Some(generation) || self.retired.contains(&generation)
    }

    fn start(&mut self, prepared: Prepared, shutdown: &CancellationToken) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1);
        let tx = self.tx.clone();
        let cancel = shutdown.child_token();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            let mut connection = prepared.connection;
            let idle = prepared.welcome.keepalive + Duration::from_secs(1);
            let mut deadline = prepared.welcomed_at + idle;
            loop {
                let result = tokio::select! {
                    _ = token.cancelled() => None,
                    result = connection.read(deadline) => Some(result),
                };
                let Some(result) = result else {
                    connection.close().await;
                    return;
                };
                let (incoming, terminal) = match result {
                    Ok(incoming) => {
                        // Ping/pong and unknown messages do not prove protocol liveness.
                        if matches!(
                            &incoming,
                            Incoming::Keepalive | Incoming::Notification { .. }
                        ) {
                            deadline = Instant::now() + idle;
                        }
                        let terminal = matches!(&incoming, Incoming::Close { .. });
                        (incoming, terminal)
                    }
                    Err(error) => (Incoming::Failed(format!("{error:#}")), true),
                };
                let sent = tokio::select! {
                    _ = token.cancelled() => false,
                    result = tx.send((generation, incoming)) => result.is_ok(),
                };
                if !sent {
                    connection.close().await;
                    return;
                }
                if terminal {
                    return;
                }
            }
        });
        self.readers.insert(generation, Reader { handle, cancel });
        generation
    }

    async fn retire(&mut self, generation: u64) {
        // Mark before closing: messages queued during graceful close remain
        // eligible for the parent's shared notification/durable-ID dedupers.
        self.retired.push_back(generation);
        while self.retired.len() > 4 {
            self.retired.pop_front();
        }
        if let Some(reader) = self.readers.remove(&generation) {
            reader.close().await;
        }
    }

    pub(super) async fn shutdown(&mut self) {
        for reader in self.readers.values() {
            reader.cancel.cancel();
        }
        for (_, reader) in std::mem::take(&mut self.readers) {
            reader.close().await;
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // Panic safety only. All normal paths explicitly cancel and join.
        for reader in self.readers.values() {
            reader.cancel.cancel();
            reader.handle.abort();
        }
    }
}

pub(super) struct Reader {
    pub(super) handle: JoinHandle<()>,
    pub(super) cancel: CancellationToken,
}

impl Reader {
    pub(super) async fn close(mut self) {
        self.cancel.cancel();
        if timeout(GRACEFUL_CLOSE, &mut self.handle).await.is_err() {
            self.handle.abort();
            let _ = (&mut self.handle).await;
        }
    }
}
