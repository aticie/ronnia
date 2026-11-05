//! Independently supervised Conduit shards. Topology mutation has one owner;
//! socket recovery never resets another shard or the subscription cache.

use super::{
    conduit::{self, Conduit},
    eventsub::{self, Backoff, Connection, Incoming, Welcome},
};
use crate::{config::Config, providers::Providers};
use anyhow::{Result, anyhow, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::{Future, pending},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep_until, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;

const MAX_SHARDS: u32 = 64;
const CONNECT_ATTEMPTS: usize = 4;
const EVENT_QUEUE: usize = 1024;
const READER_QUEUE: usize = 64;
const SHARD_ATTACH_DEADLINE: Duration = Duration::from_secs(9);
const GRACEFUL_CLOSE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Topology {
    pub remote_count: u32,
    /// Acknowledged attachments for the observed topology; zero while an
    /// ambiguous topology mutation awaits authoritative observation.
    pub attached_count: u32,
}

pub struct Event {
    pub shard_id: u32,
    pub incoming: Incoming,
}

pub struct Pool {
    pub events: mpsc::Receiver<Event>,
    pub topology: watch::Receiver<Topology>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl Pool {
    pub fn start(
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

    pub async fn shutdown(self) {
        self.cancel.cancel();
        // A receiver held by the caller must not prevent supervised shutdown.
        drop(self.events);
        if let Err(error) = self.handle.await {
            tracing::error!(%error, "EventSub shard pool task failed");
        }
    }
}

struct State {
    remote_count: u32,
    attached: BTreeSet<u32>,
    uncertain: bool,
}

impl State {
    fn topology(&self) -> Topology {
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

    fn candidate(&self, target: u32, cap: u32) -> Option<u32> {
        (!self.uncertain
            && self.remote_count > 0
            && self.remote_count < MAX_SHARDS
            && self.remote_count < cap
            && self.remote_count < target
            && self.attached.len() == self.remote_count as usize)
            .then_some(self.remote_count)
    }

    fn observe(&mut self, remote_count: u32) {
        self.remote_count = remote_count;
        self.attached
            .retain(|id| *id < remote_count && remote_count <= MAX_SHARDS);
    }

    fn set_attached(&mut self, id: u32, attached: bool) {
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
    let mut observe_retry = Backoff::new();
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
        let mut retry = Backoff::new();
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
                            match eventsub::validate_reconnect_url(&self.config.twitch_eventsub_url, &url) {
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
        let welcome_deadline = Instant::now() + eventsub::WELCOME_TIMEOUT;
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

async fn attach_before<T>(
    deadline: Instant,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    timeout_at(deadline, operation)
        .await
        .map_err(|_| anyhow!("EventSub resize/attach exceeded nine seconds after welcome"))?
}

/// Readers only decode and enforce protocol liveness. Each shard owns its
/// generation queue, so a failed socket never blocks a peer's supervision.
struct Link {
    rx: mpsc::Receiver<(u64, Incoming)>,
    tx: mpsc::Sender<(u64, Incoming)>,
    readers: BTreeMap<u64, Reader>,
    current: Option<u64>,
    next_generation: u64,
    retired: VecDeque<u64>,
}

impl Link {
    fn new() -> Self {
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

    async fn shutdown(&mut self) {
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

struct Reader {
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl Reader {
    async fn close(mut self) {
        self.cancel.cancel();
        if timeout(GRACEFUL_CLOSE, &mut self.handle).await.is_err() {
            self.handle.abort();
            let _ = (&mut self.handle).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn growth_waits_for_every_observed_shard_and_never_shrinks() {
        let mut state = State {
            remote_count: 3,
            attached: BTreeSet::from([0, 1]),
            uncertain: false,
        };
        assert_eq!(state.candidate(5, 8), None);
        state.set_attached(2, true);
        assert_eq!(state.candidate(5, 8), Some(3));
        assert_eq!(state.candidate(1, 8), None);
        assert_eq!(
            state.topology(),
            Topology {
                remote_count: 3,
                attached_count: 3
            }
        );
        state.observe(4);
        assert_eq!(state.candidate(5, 8), None);
        // A failed attachment retains the committed remote count and blocks
        // the next growth step until the missing shard actually recovers.
        state.set_attached(3, false);
        assert_eq!(state.remote_count, 4);
        state.set_attached(3, true);
        assert_eq!(state.candidate(5, 8), Some(4));
        state.uncertain = true;
        assert_eq!(state.candidate(5, 8), None);
        assert_eq!(state.topology().attached_count, 0);
        assert_eq!(state.attached.len(), 4);
    }

    #[test]
    fn persisted_count_above_cap_is_attached_but_above_hard_bound_is_not() {
        let mut state = State {
            remote_count: 4,
            attached: BTreeSet::new(),
            uncertain: false,
        };
        for id in 0..4 {
            state.set_attached(id, true);
        }
        assert_eq!(state.topology().attached_count, 4);
        assert_eq!(state.candidate(8, 2), None);
        state.observe(65);
        state.set_attached(0, true);
        assert_eq!(
            state.topology(),
            Topology {
                remote_count: 65,
                attached_count: 0
            }
        );
        assert_eq!(state.candidate(100, 100), None);
    }

    #[tokio::test]
    async fn welcome_deadlines_are_independent_and_cover_the_whole_operation() {
        let first_welcome = Instant::now() - SHARD_ATTACH_DEADLINE;
        let second_welcome = Instant::now();
        let (expired, healthy) = tokio::join!(
            attach_before(first_welcome + SHARD_ATTACH_DEADLINE, async {
                pending::<Result<()>>().await
            }),
            attach_before(second_welcome + SHARD_ATTACH_DEADLINE, async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                Ok(())
            }),
        );
        assert!(expired.unwrap_err().to_string().contains("nine seconds"));
        assert!(healthy.is_ok());
        // Both resize and attach share one deadline: finishing the first
        // operation cannot start a new nine-second budget for the second.
        let deadline = Instant::now() + Duration::from_millis(10);
        let result = attach_before(deadline, async {
            tokio::task::yield_now().await;
            pending::<Result<()>>().await
        })
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn reconnect_handoffs_must_keep_the_configured_origin() {
        let configured = "ws://localhost:9876/events";
        assert_eq!(
            eventsub::validate_reconnect_url(configured, "ws://localhost:9876/reconnect?token=one")
                .unwrap(),
            "ws://localhost:9876/reconnect?token=one",
        );
        for candidate in [
            "ws://other.localhost:9876/reconnect",
            "ws://localhost:9877/reconnect",
            "wss://localhost:9876/reconnect",
            "https://localhost:9876/reconnect",
        ] {
            assert!(eventsub::validate_reconnect_url(configured, candidate).is_err());
        }
    }

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn reader_close_joins_cooperative_reader() {
        let dropped = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let guard = Dropped(dropped.clone());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            token.cancelled().await;
        });
        Reader { handle, cancel }.close().await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn reader_close_aborts_and_joins_uncooperative_reader() {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            pending::<()>().await;
        });
        Reader {
            handle,
            cancel: CancellationToken::new(),
        }
        .close()
        .await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn link_shutdown_cancels_and_joins_all_readers() {
        let mut link = Link::new();
        let mut flags = Vec::new();
        for id in 0..4 {
            let dropped = Arc::new(AtomicBool::new(false));
            let guard = Dropped(dropped.clone());
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            let handle = tokio::spawn(async move {
                let _guard = guard;
                token.cancelled().await;
            });
            link.readers.insert(id, Reader { handle, cancel });
            flags.push(dropped);
        }
        link.shutdown().await;
        assert!(link.readers.is_empty());
        assert!(flags.iter().all(|flag| flag.load(Ordering::SeqCst)));
    }
}
