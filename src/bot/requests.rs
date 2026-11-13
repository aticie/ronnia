//! Bounded, channel-serial request execution. Admission permits cover ingress,
//! pending queues, running work, and completed jobs awaiting collection.

use super::{
    Config, Providers, Store,
    delivery::{self, Echo, process_one},
    dispatch::Message,
};
use crate::parser::Beatmap;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    future::Future,
    sync::Arc,
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    task::{Id, JoinError, JoinSet},
};
use tokio_util::sync::CancellationToken;

/// Admission always carries the scheduler's single shared memory budget.
#[derive(Clone)]
pub(super) struct RequestIngress {
    pub(super) requests: mpsc::Sender<RequestEnvelope>,
    pub(super) permits: Arc<Semaphore>,
}

pub(super) struct RequestEnvelope {
    pub(super) message: Message,
    pub(super) beatmap: Beatmap,
    pub(super) permit: OwnedSemaphorePermit,
}

#[derive(Default)]
pub(super) struct Scheduler {
    pub(super) pending: BTreeMap<i64, VecDeque<RequestEnvelope>>,
    pub(super) ready: VecDeque<i64>,
    pub(super) active: BTreeSet<i64>,
    pub(super) jobs: JoinSet<OwnedSemaphorePermit>,
    pub(super) channels_by_task: HashMap<Id, i64>,
}

impl Scheduler {
    pub(super) fn admit(&mut self, request: RequestEnvelope) {
        let channel = request.message.channel_id;
        let queue = self.pending.entry(channel).or_default();
        queue.push_back(request);
        if queue.len() == 1 && !self.active.contains(&channel) {
            self.ready.push_back(channel);
        }
    }

    /// Pop the next runnable channel in round-robin order, dropping stale ready
    /// entries and empty channels.
    pub(super) fn next(&mut self) -> Option<RequestEnvelope> {
        while let Some(channel) = self.ready.pop_front() {
            // A stale ready entry must neither run a second job for an active
            // channel nor leave an empty channel resident indefinitely.
            if self.active.contains(&channel) {
                continue;
            }
            let Some(queue) = self.pending.get_mut(&channel) else {
                continue;
            };
            let request = queue.pop_front();
            if queue.is_empty() {
                self.pending.remove(&channel);
            }
            if request.is_some() {
                return request;
            }
        }
        None
    }

    /// Spawn one job. The permit is returned as the job result and released
    /// only on collection; a panic drops it during unwinding instead.
    pub(super) fn spawn(
        &mut self,
        channel: i64,
        permit: OwnedSemaphorePermit,
        work: impl Future<Output = ()> + Send + 'static,
    ) {
        let task = self.jobs.spawn(async move {
            work.await;
            permit
        });
        self.active.insert(channel);
        self.channels_by_task.insert(task.id(), channel);
    }

    pub(super) fn complete(&mut self, result: Result<(Id, OwnedSemaphorePermit), JoinError>) {
        let id = match result {
            Ok((id, permit)) => {
                drop(permit);
                id
            }
            Err(error) => {
                tracing::error!(
                    channel_id = self.channels_by_task.get(&error.id()),
                    panic = error.is_panic(),
                    error = %error,
                    "request worker stopped unexpectedly"
                );
                error.id()
            }
        };
        if let Some(channel) = self.channels_by_task.remove(&id) {
            self.active.remove(&channel);
            if self.pending.contains_key(&channel) {
                self.ready.push_back(channel);
            }
        }
    }

    pub(super) fn discard_pending(&mut self) {
        self.pending.clear();
        self.ready.clear();
    }
}

pub(super) async fn process_requests(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    mut queue: mpsc::Receiver<RequestEnvelope>,
    echoes: mpsc::Sender<Echo>,
    shutdown: CancellationToken,
    clock: delivery::Clock,
) {
    let workers = config.request_workers;
    let mut scheduler = Scheduler::default();
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        while scheduler.jobs.len() < workers {
            let Some(RequestEnvelope {
                message,
                beatmap,
                permit,
            }) = scheduler.next()
            else {
                break;
            };
            let channel = message.channel_id;
            let config = config.clone();
            let store = store.clone();
            let providers = providers.clone();
            let echoes = echoes.clone();
            scheduler.spawn(
                channel,
                permit,
                async move {
                    // Never select cancellation around process_one: its final
                    // delivery, statistics, and cooldown bookkeeping must finish
                    // atomically.
                    if let Err(error) = process_one(
                        &config, &store, &providers, &echoes, &message, &beatmap, clock,
                    )
                    .await
                    {
                        tracing::error!(channel = %message.channel, message_id = %message.id, error = %error, "request processing failed");
                    }
                },
            );
        }
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
    drain_ingress(&mut queue, &mut scheduler).await;
}

/// Stop admission before releasing queued permits. Already-started jobs are
/// joined without cancellation so no delivery bookkeeping gets detached.
pub(super) async fn drain_ingress(
    queue: &mut mpsc::Receiver<RequestEnvelope>,
    scheduler: &mut Scheduler,
) {
    queue.close();
    while queue.try_recv().is_ok() {}
    scheduler.discard_pending();
    while let Some(result) = scheduler.jobs.join_next_with_id().await {
        scheduler.complete(result);
    }
}
