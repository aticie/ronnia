//! Bounded, channel-serial request execution. Admission permits cover ingress,
//! pending queues, running work, and completed jobs awaiting collection.

use super::{
    Beatmap, Clock, Config, Echo, Message, Providers, Store, isolated_hub, process_one,
    record_duration, record_outcome,
};
use sentry::SentryFutureExt;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
    sync::Arc,
};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    task::{Id, JoinError, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub(super) struct RequestEnvelope {
    pub(super) message: Message,
    pub(super) beatmap: Beatmap,
    pub(super) permit: OwnedSemaphorePermit,
    pub(super) span: tracing::Span,
    pub(super) queued_at: Instant,
}

struct RequestTask {
    channel: i64,
    span: tracing::Span,
    hub: Arc<sentry::Hub>,
}

#[derive(Default)]
struct Scheduler {
    pending: BTreeMap<i64, VecDeque<RequestEnvelope>>,
    ready: VecDeque<i64>,
    active: BTreeSet<i64>,
    jobs: JoinSet<OwnedSemaphorePermit>,
    channels_by_task: HashMap<Id, RequestTask>,
}

impl Scheduler {
    fn admit(&mut self, request: RequestEnvelope) {
        let channel = request.message.channel_id;
        let queue = self.pending.entry(channel).or_default();
        queue.push_back(request);
        if queue.len() == 1 && !self.active.contains(&channel) {
            self.ready.push_back(channel);
        }
    }

    fn fill<F, P>(&mut self, workers: usize, process: &mut P)
    where
        F: Future<Output = ()> + Send + 'static,
        P: FnMut(Message, Beatmap) -> F,
    {
        while self.jobs.len() < workers {
            let Some(channel) = self.ready.pop_front() else {
                break;
            };
            // A stale ready entry must neither run a second job for an active
            // channel nor leave an empty channel resident indefinitely.
            if self.active.contains(&channel) {
                continue;
            }
            let request = self.pending.get_mut(&channel).and_then(VecDeque::pop_front);
            if self.pending.get(&channel).is_some_and(VecDeque::is_empty) {
                self.pending.remove(&channel);
            }
            let Some(RequestEnvelope {
                message,
                beatmap,
                permit,
                span,
                queued_at,
            }) = request
            else {
                continue;
            };
            let work = process(message, beatmap);
            let hub = isolated_hub();
            let task = self.jobs.spawn(
                async move {
                    let queue_wait_ms = queued_at.elapsed().as_secs_f64() * 1000.0;
                    record_duration("ronnia.request.queue_wait", queue_wait_ms);
                    tracing::debug!(queue_wait_ms, "request processing started");
                    let started = Instant::now();
                    work.await;
                    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
                    record_duration("ronnia.request.processing_time", elapsed_ms);
                    tracing::debug!(elapsed_ms, "request processing finished");
                    // Keep the permit even after work finishes, until collection.
                    // A panic drops it during unwinding instead.
                    permit
                }
                .instrument(span.clone())
                .bind_hub(hub.clone()),
            );
            self.active.insert(channel);
            self.channels_by_task
                .insert(task.id(), RequestTask { channel, span, hub });
        }
    }

    fn complete(&mut self, result: Result<(Id, OwnedSemaphorePermit), JoinError>) {
        let id = match result {
            Ok((id, permit)) => {
                drop(permit);
                id
            }
            Err(error) => {
                if let Some(request) = self.channels_by_task.get(&error.id()) {
                    sentry::Hub::run(request.hub.clone(), || {
                        request.span.in_scope(|| {
                            record_outcome(
                                "ronnia.requests.processed",
                                if error.is_panic() {
                                    "panic"
                                } else {
                                    "cancelled"
                                },
                            );
                            tracing::error!(
                                channel_id = request.channel,
                                panic = error.is_panic(),
                                error = %error,
                                "request worker stopped unexpectedly"
                            );
                        });
                    });
                }
                error.id()
            }
        };
        if let Some(request) = self.channels_by_task.remove(&id) {
            self.active.remove(&request.channel);
            if self.pending.contains_key(&request.channel) {
                self.ready.push_back(request.channel);
            }
        }
    }

    fn discard_pending(&mut self) {
        self.pending.clear();
        self.ready.clear();
    }
}

pub(super) async fn process_requests(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    queue: mpsc::Receiver<RequestEnvelope>,
    echoes: mpsc::Sender<Echo>,
    shutdown: CancellationToken,
    clock: Clock,
) {
    let workers = config.request_workers;
    run_scheduler(queue, shutdown, workers, move |message, beatmap| {
        let config = config.clone();
        let store = store.clone();
        let providers = providers.clone();
        let echoes = echoes.clone();
        async move {
            // Never select cancellation around process_one: its final delivery,
            // statistics, and cooldown bookkeeping must finish atomically.
            let result = process_one(
                &config, &store, &providers, &echoes, &message, &beatmap, clock,
            )
            .await;
            record_outcome(
                "ronnia.requests.processed",
                if result.is_ok() { "ok" } else { "error" },
            );
            if let Err(error) = result {
                tracing::error!(
                    channel = %message.channel,
                    message_id = %message.id,
                    error = error.as_ref() as &dyn std::error::Error,
                    error_chain = %format_args!("{error:#}"),
                    "request processing failed"
                );
            }
        }
    })
    .await;
}

async fn run_scheduler<F, P>(
    mut queue: mpsc::Receiver<RequestEnvelope>,
    shutdown: CancellationToken,
    workers: usize,
    mut process: P,
) where
    F: Future<Output = ()> + Send + 'static,
    P: FnMut(Message, Beatmap) -> F,
{
    let mut scheduler = Scheduler::default();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        scheduler.fill(workers, &mut process);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            result = scheduler.jobs.join_next_with_id(), if !scheduler.jobs.is_empty() => {
                if let Some(result) = result {
                    scheduler.complete(result);
                }
            }
            request = queue.recv() => match request {
                Some(request) => scheduler.admit(request),
                None => break,
            }
        }
    }

    // Stop admission before releasing queued permits. Already-started jobs are
    // joined without cancellation so no delivery bookkeeping gets detached.
    queue.close();
    while queue.try_recv().is_ok() {}
    scheduler.discard_pending();
    while let Some(result) = scheduler.jobs.join_next_with_id().await {
        scheduler.complete(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::BeatmapKind;
    use std::{pin::Pin, time::Duration};
    use tokio::sync::{Semaphore, oneshot};

    type Work = Pin<Box<dyn Future<Output = ()> + Send>>;

    struct Controlled {
        gates: BTreeMap<String, oneshot::Receiver<bool>>,
        started: mpsc::UnboundedSender<String>,
        finished: mpsc::UnboundedSender<String>,
    }

    impl Controlled {
        fn work(&mut self, message: Message, _: Beatmap) -> Work {
            let gate = self
                .gates
                .remove(&message.id)
                .expect("one start per request");
            let started = self.started.clone();
            let finished = self.finished.clone();
            Box::pin(async move {
                started.send(message.id.clone()).unwrap();
                let panic = gate.await.expect("test must release active work");
                assert!(!panic, "controlled worker panic");
                finished.send(message.id).unwrap();
            })
        }
    }

    struct Fixture {
        budget: Arc<Semaphore>,
        control: Controlled,
        gates: BTreeMap<String, oneshot::Sender<bool>>,
        started: mpsc::UnboundedReceiver<String>,
        finished: mpsc::UnboundedReceiver<String>,
    }

    impl Fixture {
        fn new(capacity: usize) -> Self {
            let (started_tx, started) = mpsc::unbounded_channel();
            let (finished_tx, finished) = mpsc::unbounded_channel();
            Self {
                budget: Arc::new(Semaphore::new(capacity)),
                control: Controlled {
                    gates: BTreeMap::new(),
                    started: started_tx,
                    finished: finished_tx,
                },
                gates: BTreeMap::new(),
                started,
                finished,
            }
        }

        fn request(&mut self, channel: i64, id: &str) -> RequestEnvelope {
            let (release, gate) = oneshot::channel();
            self.gates.insert(id.to_owned(), release);
            self.control.gates.insert(id.to_owned(), gate);
            RequestEnvelope {
                message: Message {
                    id: id.to_owned(),
                    channel: format!("channel{channel}"),
                    channel_id: channel,
                    author: "requester".into(),
                    author_id: "100".into(),
                    moderator: false,
                    subscriber: false,
                    vip: false,
                    points: false,
                },
                beatmap: Beatmap {
                    id: 1,
                    kind: BeatmapKind::Map,
                    mods: String::new(),
                },
                permit: self.budget.clone().try_acquire_owned().unwrap(),
                span: tracing::Span::none(),
                queued_at: Instant::now(),
            }
        }

        fn release(&mut self, id: &str, panic: bool) {
            self.gates.remove(id).unwrap().send(panic).unwrap();
        }

        async fn started(&mut self) -> String {
            tokio::time::timeout(Duration::from_secs(5), self.started.recv())
                .await
                .unwrap()
                .unwrap()
        }

        async fn finished(&mut self) -> String {
            tokio::time::timeout(Duration::from_secs(5), self.finished.recv())
                .await
                .unwrap()
                .unwrap()
        }
    }

    async fn collect(scheduler: &mut Scheduler) {
        let result =
            tokio::time::timeout(Duration::from_secs(5), scheduler.jobs.join_next_with_id())
                .await
                .unwrap()
                .unwrap();
        scheduler.complete(result);
    }

    fn assert_empty(scheduler: &Scheduler) {
        assert!(scheduler.pending.is_empty());
        assert!(scheduler.ready.is_empty());
        assert!(scheduler.active.is_empty());
        assert!(scheduler.jobs.is_empty());
        assert!(scheduler.channels_by_task.is_empty());
    }

    #[tokio::test]
    async fn one_budget_bounds_workers_pending_and_uncollected_results() {
        let mut fixture = Fixture::new(32);
        let mut scheduler = Scheduler::default();
        for index in 0..32 {
            scheduler.admit(fixture.request(index % 4, &format!("r{index}")));
        }
        assert!(fixture.budget.clone().try_acquire_owned().is_err());
        scheduler.fill(4, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        let mut first = BTreeSet::new();
        for _ in 0..4 {
            first.insert(fixture.started().await);
        }
        assert_eq!(
            first,
            BTreeSet::from(["r0".into(), "r1".into(), "r2".into(), "r3".into()])
        );
        assert!(fixture.started.try_recv().is_err());
        assert_eq!(scheduler.active.len(), 4);
        assert_eq!(scheduler.jobs.len(), 4);
        assert_eq!(
            scheduler.pending.values().map(VecDeque::len).sum::<usize>(),
            28
        );
        fixture.release("r1", false);
        assert_eq!(fixture.finished().await, "r1");
        assert_eq!(
            fixture.budget.available_permits(),
            0,
            "completed but uncollected jobs still own admission"
        );
        collect(&mut scheduler).await;
        assert_eq!(fixture.budget.available_permits(), 1);
        scheduler.fill(4, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        assert_eq!(
            fixture.started().await,
            "r5",
            "another channel progresses while r0 is held"
        );
        assert!(
            fixture.started.try_recv().is_err(),
            "same-channel successors cannot overlap"
        );
        scheduler.discard_pending();
        assert_eq!(fixture.budget.available_permits(), 28);
        for id in ["r0", "r2", "r3", "r5"] {
            fixture.release(id, false);
        }
        while !scheduler.jobs.is_empty() {
            collect(&mut scheduler).await;
        }
        assert_empty(&scheduler);
        assert_eq!(fixture.budget.available_permits(), 32);
    }

    #[tokio::test]
    async fn channel_fifo_is_serial_and_ready_channels_rotate_fairly() {
        let mut fixture = Fixture::new(5);
        let mut scheduler = Scheduler::default();
        for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1"), (2, "b2"), (3, "c1")] {
            scheduler.admit(fixture.request(channel, id));
        }
        // Defensive stale entries cannot resurrect empty channel state.
        scheduler.ready.push_front(999);
        for expected in ["a1", "b1", "c1", "a2", "b2"] {
            scheduler.fill(1, &mut |message, beatmap| {
                fixture.control.work(message, beatmap)
            });
            assert_eq!(fixture.started().await, expected);
            assert!(fixture.started.try_recv().is_err());
            fixture.release(expected, false);
            collect(&mut scheduler).await;
        }
        assert_empty(&scheduler);
        assert_eq!(fixture.budget.available_permits(), 5);
    }

    #[tokio::test]
    async fn panicked_worker_releases_channel_and_admission_without_blocking_peers() {
        let mut fixture = Fixture::new(3);
        let mut scheduler = Scheduler::default();
        for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1")] {
            scheduler.admit(fixture.request(channel, id));
        }
        scheduler.fill(2, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        let mut starts = BTreeSet::new();
        starts.insert(fixture.started().await);
        starts.insert(fixture.started().await);
        assert_eq!(starts, BTreeSet::from(["a1".into(), "b1".into()]));
        fixture.release("a1", true);
        collect(&mut scheduler).await;
        assert_eq!(fixture.budget.available_permits(), 1);
        assert_eq!(scheduler.active, BTreeSet::from([2]));
        assert_eq!(scheduler.channels_by_task.len(), 1);
        scheduler.fill(2, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        assert_eq!(fixture.started().await, "a2");
        fixture.release("a2", false);
        fixture.release("b1", false);
        collect(&mut scheduler).await;
        collect(&mut scheduler).await;
        assert_empty(&scheduler);
        assert_eq!(fixture.budget.available_permits(), 3);
    }

    #[tokio::test]
    async fn cancelled_task_clears_active_channel_and_skips_stale_ready_entries() {
        let mut fixture = Fixture::new(2);
        let mut scheduler = Scheduler::default();
        scheduler.admit(fixture.request(1, "a1"));
        scheduler.admit(fixture.request(1, "a2"));
        scheduler.fill(2, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        assert_eq!(fixture.started().await, "a1");
        scheduler.ready.push_front(1);
        scheduler.fill(2, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        assert!(fixture.started.try_recv().is_err());
        assert_eq!(scheduler.jobs.len(), 1);
        assert!(scheduler.ready.is_empty());
        scheduler.jobs.abort_all();
        collect(&mut scheduler).await;
        assert!(scheduler.active.is_empty());
        assert!(scheduler.channels_by_task.is_empty());
        assert_eq!(fixture.budget.available_permits(), 1);
        scheduler.fill(2, &mut |message, beatmap| {
            fixture.control.work(message, beatmap)
        });
        assert_eq!(fixture.started().await, "a2");
        fixture.release("a2", false);
        collect(&mut scheduler).await;
        assert_empty(&scheduler);
        assert_eq!(fixture.budget.available_permits(), 2);
    }

    #[tokio::test]
    async fn shutdown_closes_ingress_discards_pending_and_joins_active_work() {
        let mut fixture = Fixture::new(4);
        let (tx, rx) = mpsc::channel(4);
        for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1"), (2, "b2")] {
            tx.try_send(fixture.request(channel, id))
                .unwrap_or_else(|_| panic!("queue has capacity"));
        }
        let shutdown = CancellationToken::new();
        let mut control = fixture.control;
        let worker = tokio::spawn(run_scheduler(
            rx,
            shutdown.clone(),
            1,
            move |message, beatmap| control.work(message, beatmap),
        ));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), fixture.started.recv())
                .await
                .unwrap()
                .unwrap(),
            "a1"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), tx.closed())
            .await
            .unwrap();
        assert!(tx.is_closed());
        assert_eq!(fixture.budget.available_permits(), 3);
        assert!(
            !worker.is_finished(),
            "shutdown must join rather than cancel active bookkeeping"
        );
        assert!(fixture.started.try_recv().is_err());
        fixture.gates.remove("a1").unwrap().send(false).unwrap();
        tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fixture.budget.available_permits(), 4);
        assert!(fixture.started.try_recv().is_err());
    }
}
