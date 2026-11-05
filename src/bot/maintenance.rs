//! One owner for membership, subscription reconciliation and stream snapshots.
//! HTTP work never runs on the foreground EventSub dispatcher.

use super::{AUDIT_INTERVAL, MAX_RESYNC_ATTEMPTS, TARGET_DEBOUNCE, conduit, shards, streams};
use crate::{config::Config, providers::Providers, storage::Store};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

const TARGET_CAPACITY: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Signal {
    pub revision: u64,
    pub audit_revision: u64,
    pub snapshot_revision: u64,
}

impl Default for Signal {
    fn default() -> Self {
        Self {
            revision: 1,
            audit_revision: 1,
            snapshot_revision: 1,
        }
    }
}

pub(super) fn request(signals: &watch::Sender<Signal>, audit: bool, snapshot: bool) {
    signals.send_modify(|signal| {
        signal.revision = signal.revision.saturating_add(1);
        if audit {
            signal.audit_revision = signal.audit_revision.saturating_add(1);
        }
        if snapshot {
            signal.snapshot_revision = signal.snapshot_revision.saturating_add(1);
        }
    });
}

#[derive(Debug)]
pub(super) enum Action {
    Schedule { id: i64, expected_revision: i64 },
    Cancel { id: i64 },
}

pub(super) fn actions() -> (mpsc::Sender<Action>, mpsc::Receiver<Action>) {
    mpsc::channel(TARGET_CAPACITY)
}

/// Failed bounded admission is coalesced into a full-snapshot obligation.
pub(super) fn enqueue(
    actions: &mpsc::Sender<Action>,
    signals: &watch::Sender<Signal>,
    action: Action,
) {
    if actions.try_send(action).is_err() {
        request(signals, false, true);
    }
}

#[derive(Clone, Copy)]
struct Lookup {
    due: Instant,
    attempt: u32,
    // None is a membership/snapshot correction, which captures a fresh revision.
    // Some is event-triggered work and must never adopt a newer revision.
    expected: Option<i64>,
}

#[derive(Default)]
struct Pending {
    due: BTreeMap<i64, Lookup>,
}

impl Pending {
    fn insert(&mut self, id: i64, lookup: Lookup) -> bool {
        if self.due.len() >= conduit::MAX_ACTIVE_GRANTS && !self.due.contains_key(&id) {
            return false;
        }
        self.due.insert(id, lookup);
        true
    }

    fn fresh(&mut self, id: i64) -> bool {
        // An authoritative membership correction supersedes an older online retry.
        self.insert(
            id,
            Lookup {
                due: Instant::now(),
                attempt: 0,
                expected: None,
            },
        )
    }

    fn schedule(&mut self, id: i64, expected: i64) -> bool {
        self.insert(
            id,
            Lookup {
                due: Instant::now() + TARGET_DEBOUNCE,
                attempt: 0,
                expected: Some(expected),
            },
        )
    }

    fn retry(&mut self, id: i64, lookup: Lookup) -> bool {
        let delay = Duration::from_secs((1u64 << lookup.attempt.min(6)).min(60));
        self.insert(
            id,
            Lookup {
                due: Instant::now() + delay,
                attempt: lookup.attempt + 1,
                ..lookup
            },
        )
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.due.values().map(|lookup| lookup.due).min()
    }

    fn take_due(&mut self) -> BTreeMap<i64, Lookup> {
        let now = Instant::now();
        let ids: Vec<i64> = self
            .due
            .iter()
            .filter(|(_, lookup)| lookup.due <= now)
            .take(100)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| self.due.remove(&id).map(|lookup| (id, lookup)))
            .collect()
    }
}

#[derive(Default)]
struct Retry {
    until: Option<Instant>,
    attempt: u32,
}

impl Retry {
    fn ready(&self, now: Instant) -> bool {
        self.until.is_none_or(|until| until <= now)
    }
    fn success(&mut self) {
        *self = Self::default();
    }
    fn fail(&mut self) {
        self.until =
            Some(Instant::now() + Duration::from_secs((1u64 << self.attempt.min(6)).min(60)));
        self.attempt = self.attempt.saturating_add(1);
    }
}

/// The worker's bounded work inboxes and topology-control endpoints.
pub(super) struct Channels {
    pub membership: watch::Receiver<u64>,
    pub signals: watch::Receiver<Signal>,
    pub actions: mpsc::Receiver<Action>,
    pub targets: watch::Sender<u32>,
    pub topology: watch::Receiver<shards::Topology>,
}

pub(super) struct Maintenance {
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    bot_id: String,
    subs: conduit::Subscriptions,
    pending: Pending,
    known: BTreeSet<i64>,
    known_granted: BTreeSet<i64>,
    grants: Option<Vec<i64>>,
    channels: watch::Receiver<u64>,
    signals: watch::Receiver<Signal>,
    actions: mpsc::Receiver<Action>,
    targets: watch::Sender<u32>,
    topology: watch::Receiver<shards::Topology>,
    capped: bool,
    over_capacity: bool,
    snapshot_pending: bool,
    snapshot_after_reconcile: bool,
}

impl Maintenance {
    pub(super) fn new(
        config: Arc<Config>,
        store: Store,
        providers: Providers,
        conduit_id: String,
        bot_id: String,
        channels: Channels,
    ) -> Self {
        Self {
            config,
            store,
            providers,
            subs: conduit::Subscriptions::new(conduit_id, bot_id.clone()),
            bot_id,
            pending: Pending::default(),
            known: BTreeSet::new(),
            known_granted: BTreeSet::new(),
            grants: None,
            channels: channels.membership,
            signals: channels.signals,
            actions: channels.actions,
            targets: channels.targets,
            topology: channels.topology,
            capped: false,
            over_capacity: false,
            snapshot_pending: true,
            snapshot_after_reconcile: true,
        }
    }

    /// A snapshot taken between paced creation passes cannot observe channels
    /// that are not subscribed yet. Preserve a final correction independently
    /// of any snapshots needed while reconciliation is still incomplete.
    fn subscriptions_reconciled(&mut self, summary: &conduit::Summary) {
        if summary.deferred > 0 {
            self.snapshot_after_reconcile = true;
        }
        if summary.deferred == 0 && self.snapshot_after_reconcile {
            self.snapshot_pending = true;
            self.snapshot_after_reconcile = false;
        }
    }

    /// Commit neither membership baseline unless both authoritative reads succeed.
    /// One granted snapshot drives sizing, subscriptions and activation edges.
    pub(super) async fn refresh_membership(&mut self) -> Result<()> {
        let enabled: BTreeSet<i64> = self.store.enabled_twitch_ids().await?.into_iter().collect();
        let grants = self.store.granted_twitch_ids().await?;
        let granted: BTreeSet<i64> = grants.iter().copied().collect();
        let fresh: Vec<i64> = enabled
            .iter()
            .copied()
            .filter(|id| {
                !self.known.contains(id)
                    || (granted.contains(id) && !self.known_granted.contains(id))
            })
            .collect();
        let requested = grants
            .len()
            .div_ceil(self.config.eventsub_channels_per_shard)
            .max(1);
        let capped = requested > self.config.eventsub_max_shards as usize;
        if capped && !self.capped {
            tracing::warn!(
                grants = grants.len(),
                max_shards = self.config.eventsub_max_shards,
                "EventSub automatic shard cap reached; eligible channels continue using existing shards within the subscription audit safety bound"
            );
        }
        self.capped = capped;
        let target = requested.min(self.config.eventsub_max_shards as usize) as u32;
        // The pool is grow-only, and retaining the high-water mark avoids shrink requests.
        if target > *self.targets.borrow() {
            self.targets.send_replace(target);
        }
        let over_capacity = grants.len() > conduit::MAX_ACTIVE_GRANTS;
        if over_capacity && !self.over_capacity {
            tracing::error!(
                grants = grants.len(),
                max_grants = conduit::MAX_ACTIVE_GRANTS,
                "EventSub degraded capacity: active grants exceed the 10000-subscription audit bound; retaining cache and existing subscriptions without partial creations"
            );
        }
        self.over_capacity = over_capacity;
        self.known = enabled;
        self.known_granted = granted;
        self.grants = Some(grants);
        self.pending.due.retain(|id, _| self.known.contains(id));
        for id in fresh {
            if !self.pending.fresh(id) {
                self.snapshot_pending = true;
            }
        }
        Ok(())
    }

    fn drain_actions(&mut self) {
        while let Ok(action) = self.actions.try_recv() {
            match action {
                Action::Schedule {
                    id,
                    expected_revision,
                } => {
                    if !self.pending.schedule(id, expected_revision) {
                        self.snapshot_pending = true;
                    }
                }
                Action::Cancel { id } => {
                    self.pending.due.remove(&id);
                }
            }
        }
    }

    pub(super) async fn run_pending(&mut self) -> Result<()> {
        self.drain_actions();
        let mut due = self.pending.take_due();
        if due.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = due.keys().copied().collect();
        let current: BTreeMap<i64, i64> = match self.store.snapshot_versions(ids).await {
            Ok(versions) => versions.into_iter().collect(),
            Err(error) => {
                for (id, lookup) in due {
                    if lookup.attempt < MAX_RESYNC_ATTEMPTS && !self.pending.retry(id, lookup) {
                        self.snapshot_pending = true;
                    }
                }
                return Err(error);
            }
        };
        // In particular, an offline/category event invalidates an online retry,
        // even when its ordered Cancel action could not fit in the bounded queue.
        due.retain(|id, lookup| {
            current.get(id).is_some_and(|revision| {
                lookup.expected.is_none_or(|expected| expected == *revision)
            })
        });
        if due.is_empty() {
            return Ok(());
        }
        let expected = due
            .keys()
            .filter_map(|id| current.get(id).map(|revision| (*id, *revision)))
            .collect();
        match streams::targeted_at(&self.store, &self.providers, &expected).await {
            Ok(skipped) => {
                let skipped: HashSet<i64> = skipped.into_iter().collect();
                for (id, lookup) in due {
                    if skipped.contains(&id)
                        && lookup.attempt < MAX_RESYNC_ATTEMPTS
                        && !self.pending.retry(id, lookup)
                    {
                        self.snapshot_pending = true;
                    }
                }
                Ok(())
            }
            Err(error) => {
                for (id, lookup) in due {
                    if lookup.attempt < MAX_RESYNC_ATTEMPTS && !self.pending.retry(id, lookup) {
                        self.snapshot_pending = true;
                    }
                }
                Err(error)
            }
        }
    }

    pub(super) async fn run(mut self, shutdown: CancellationToken) {
        let mut channel_ack = None;
        let mut member_signal_ack = 0;
        let mut audit_ack = 0;
        let mut reconcile_ack = 0;
        let mut snapshot_ack = 0;
        let mut membership_pending = true;
        let mut reconcile_pending = true;
        let mut audit_pending = true;
        let mut member_retry = Retry::default();
        let mut audit_retry = Retry::default();
        let mut reconcile_retry = Retry::default();
        let mut snapshot_retry = Retry::default();
        let mut next_periodic =
            Instant::now() + Duration::from_secs(self.config.reconcile_interval_secs.max(1));
        let mut next_audit = Instant::now() + AUDIT_INTERVAL;
        let mut channels_open = true;
        let mut actions_open = true;
        let mut was_attached = false;
        loop {
            if shutdown.is_cancelled() {
                return;
            }
            self.drain_actions();
            let signal = *self.signals.borrow_and_update();
            let channel_revision = *self.channels.borrow_and_update();
            let topology = *self.topology.borrow_and_update();
            let now = Instant::now();
            if now >= next_periodic {
                membership_pending = true;
                reconcile_pending = true;
                self.snapshot_pending = true;
                next_periodic =
                    now + Duration::from_secs(self.config.reconcile_interval_secs.max(1));
            }
            if now >= next_audit {
                audit_pending = true;
                membership_pending = true;
                next_audit = now + AUDIT_INTERVAL;
            }
            if channel_ack != Some(channel_revision) || member_signal_ack < signal.revision {
                membership_pending = true;
            }
            if audit_ack < signal.audit_revision {
                audit_pending = true;
            }
            if reconcile_ack < signal.revision {
                reconcile_pending = true;
            }
            if snapshot_ack < signal.snapshot_revision {
                self.snapshot_pending = true;
            }

            // Only one operation is in flight. Each acknowledgement uses the
            // revision captured before that operation; newer watch values survive.
            if membership_pending && member_retry.ready(now) {
                match self.refresh_membership().await {
                    Ok(()) => {
                        channel_ack = Some(channel_revision);
                        member_signal_ack = signal.revision;
                        membership_pending = false;
                        reconcile_pending = true;
                        member_retry.success();
                    }
                    Err(error) => {
                        member_retry.fail();
                        tracing::warn!(error = %error, "could not refresh channel membership; retaining both activation baselines");
                    }
                }
                continue;
            }
            let within_capacity = self.grants.is_some() && !self.over_capacity;
            if audit_pending && within_capacity && audit_retry.ready(now) {
                match self.subs.refresh_remote(&self.providers, &shutdown).await {
                    Ok(_) => {
                        audit_ack = signal.audit_revision;
                        audit_pending = false;
                        reconcile_pending = true;
                        audit_retry.success();
                    }
                    Err(error) => {
                        audit_retry.fail();
                        tracing::warn!(error = %error, "EventSub audit failed; retaining subscription cache");
                    }
                }
                continue;
            }
            let attached = topology.remote_count >= *self.targets.borrow()
                && topology.remote_count > 0
                && topology.remote_count == topology.attached_count;
            if attached && !was_attached {
                // A topology-gated creation pass must resume immediately when
                // attachment finishes, not wait its no-progress retry interval.
                reconcile_retry.success();
            }
            was_attached = attached;
            if reconcile_pending
                && !membership_pending
                && !audit_pending
                && within_capacity
                && reconcile_retry.ready(now)
            {
                let grants = self.grants.as_ref().expect("membership read completed");
                let result = match conduit::desired(grants, &self.bot_id) {
                    Ok(desired) => {
                        self.subs
                            .reconcile(&self.providers, &desired, attached, &shutdown)
                            .await
                    }
                    Err(error) => Err(error),
                };
                match result {
                    Ok(summary) => {
                        self.subscriptions_reconciled(&summary);
                        reconcile_ack = signal.revision;
                        reconcile_retry.success();
                        reconcile_pending = summary.deferred > 0;
                        if reconcile_pending {
                            // Progress passes continue promptly; provider/permission
                            // blocks retain their own 60..600-second backoff.
                            reconcile_retry.until = Some(
                                Instant::now()
                                    + if summary.created > 0 {
                                        Duration::from_secs(1)
                                    } else {
                                        Duration::from_secs(60)
                                    },
                            );
                        }
                        tracing::info!(
                            created = summary.created,
                            removed = summary.removed,
                            kept = summary.kept,
                            deferred = summary.deferred,
                            "EventSub subscription reconciliation complete"
                        );
                    }
                    Err(error) => {
                        reconcile_retry.fail();
                        tracing::warn!(error = %error, "EventSub reconciliation failed; retaining subscription cache");
                    }
                }
                continue;
            }
            if self.snapshot_pending && snapshot_retry.ready(now) {
                match streams::full(&self.store, &self.providers).await {
                    Ok(skipped) => {
                        self.snapshot_pending = false;
                        snapshot_ack = signal.snapshot_revision;
                        snapshot_retry.success();
                        for id in skipped {
                            if !self.pending.fresh(id) {
                                self.snapshot_pending = true;
                            }
                        }
                    }
                    Err(error) => {
                        snapshot_retry.fail();
                        tracing::warn!(error = %error, "stream snapshot failed; retaining previous state");
                    }
                }
                continue;
            }
            if self
                .pending
                .next_deadline()
                .is_some_and(|deadline| deadline <= now)
            {
                if let Err(error) = self.run_pending().await {
                    tracing::warn!(error = %error, "targeted stream lookup failed; keeping state and retrying");
                }
                continue;
            }

            let mut deadline = next_periodic.min(next_audit);
            if membership_pending && let Some(until) = member_retry.until {
                deadline = deadline.min(until);
            }
            if audit_pending
                && within_capacity
                && let Some(until) = audit_retry.until
            {
                deadline = deadline.min(until);
            }
            if reconcile_pending
                && !membership_pending
                && !audit_pending
                && within_capacity
                && let Some(until) = reconcile_retry.until
            {
                deadline = deadline.min(until);
            }
            if self.snapshot_pending
                && let Some(until) = snapshot_retry.until
            {
                deadline = deadline.min(until);
            }
            if let Some(until) = self.pending.next_deadline() {
                deadline = deadline.min(until);
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = sleep_until(deadline) => {},
                changed = self.channels.changed(), if channels_open => { channels_open = changed.is_ok(); },
                changed = self.signals.changed() => { if changed.is_err() { return; } },
                changed = self.topology.changed() => { if changed.is_err() { return; } },
                action = self.actions.recv(), if actions_open => {
                    match action {
                        Some(Action::Schedule { id, expected_revision }) => {
                            if !self.pending.schedule(id, expected_revision) { self.snapshot_pending = true; }
                        }
                        Some(Action::Cancel { id }) => { self.pending.due.remove(&id); }
                        None => actions_open = false,
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bot::tests::{helix_server, mock_config, seed_user},
        domain::StreamEvent,
    };
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn overflow_coalesces_snapshot_without_losing_later_revisions() {
        let (actions, mut queue) = actions();
        let (signals, mut revisions) = watch::channel(Signal::default());
        let captured = *revisions.borrow_and_update();
        for id in 0..TARGET_CAPACITY {
            enqueue(
                &actions,
                &signals,
                Action::Schedule {
                    id: id as i64,
                    expected_revision: 7,
                },
            );
        }
        enqueue(&actions, &signals, Action::Cancel { id: 0 });
        let overflow = *revisions.borrow_and_update();
        assert_eq!(overflow.snapshot_revision, captured.snapshot_revision + 1);
        assert_eq!(overflow.audit_revision, captured.audit_revision);
        assert_eq!(
            queue.len(),
            TARGET_CAPACITY,
            "overflow never expands the queue"
        );
        // Completing an operation at the captured revision cannot acknowledge
        // the overflow obligation or a later independent revocation.
        request(&signals, true, false);
        let later = *revisions.borrow_and_update();
        assert!(later.snapshot_revision > captured.snapshot_revision);
        assert!(later.revision > overflow.revision);
        assert!(later.audit_revision > overflow.audit_revision);
        for id in 0..TARGET_CAPACITY {
            assert!(matches!(queue.recv().await,
                Some(Action::Schedule { id: queued, expected_revision: 7 }) if queued == id as i64));
        }
        assert!(queue.try_recv().is_err());
    }

    #[test]
    fn pending_debounces_bounds_and_preserves_retry_revision() {
        let mut pending = Pending::default();
        assert!(pending.schedule(1, 10));
        assert!(pending.take_due().is_empty());
        let original = pending.due.remove(&1).unwrap();
        assert!(original.due > Instant::now());
        assert!(pending.retry(1, original));
        let retry = pending.due.get(&1).unwrap();
        assert_eq!(retry.attempt, 1);
        assert_eq!(
            retry.expected,
            Some(10),
            "retry may not recapture an offline revision"
        );
        assert!(retry.due > Instant::now());
        for id in 2..=conduit::MAX_ACTIVE_GRANTS as i64 {
            assert!(pending.schedule(id, 1));
        }
        assert!(!pending.schedule(conduit::MAX_ACTIVE_GRANTS as i64 + 1, 1));
        assert!(
            pending.schedule(1, 11),
            "a newer online revision supersedes existing work"
        );
        assert_eq!(pending.due[&1].expected, Some(11));
        assert_eq!(pending.due.len(), conduit::MAX_ACTIVE_GRANTS);
    }

    #[tokio::test]
    async fn failed_online_lookup_retry_does_not_adopt_newer_offline_revision() {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        store
            .apply_stream_event(1, "streamer".into(), StreamEvent::Online, 100)
            .await
            .unwrap();
        let online_revision = store.snapshot_versions(vec![1]).await.unwrap()[0].1;
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (_signals, signal_rx) = watch::channel(Signal::default());
        let (action_tx, action_rx) = actions();
        let (targets, _target_rx) = watch::channel(1);
        let (_topology_tx, topology) = watch::channel(shards::Topology {
            remote_count: 1,
            attached_count: 1,
        });
        let mut maintenance = Maintenance::new(
            config,
            store.clone(),
            providers,
            "conduit-1".into(),
            "bot".into(),
            Channels {
                membership: store.channel_changes(),
                signals: signal_rx,
                actions: action_rx,
                targets,
                topology,
            },
        );
        // This is the exact event-triggered retry retained after a failed HTTP
        // lookup. Its deadline is due, but its baseline is still the online event.
        maintenance.pending.insert(
            1,
            Lookup {
                due: Instant::now(),
                attempt: 1,
                expected: Some(online_revision),
            },
        );
        store
            .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 200)
            .await
            .unwrap();
        maintenance.run_pending().await.unwrap();
        assert_eq!(
            state.stream_calls.load(Ordering::SeqCst),
            0,
            "a stale retry is discarded before issuing HTTP, not rebound to offline"
        );
        assert!(maintenance.pending.due.is_empty());
        assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        // Ordered cancellation also removes a scheduled online lookup outright.
        action_tx
            .try_send(Action::Schedule {
                id: 1,
                expected_revision: online_revision,
            })
            .unwrap();
        action_tx.try_send(Action::Cancel { id: 1 }).unwrap();
        maintenance.drain_actions();
        assert!(maintenance.pending.due.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn completed_creation_pass_requires_snapshot_after_an_intermediate_correction() {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        store.call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
                [],
            )?;
            Ok(())
        }).await.unwrap();
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (_signals, signal_rx) = watch::channel(Signal::default());
        let (_actions, action_rx) = actions();
        let (targets, _target_rx) = watch::channel(1);
        let (_topology_tx, topology) = watch::channel(shards::Topology {
            remote_count: 1,
            attached_count: 1,
        });
        let mut maintenance = Maintenance::new(
            config,
            store.clone(),
            providers.clone(),
            "conduit-1".into(),
            "bot".into(),
            Channels {
                membership: store.channel_changes(),
                signals: signal_rx,
                actions: action_rx,
                targets,
                topology,
            },
        );
        maintenance.subscriptions_reconciled(&conduit::Summary {
            created: 200,
            deferred: 4,
            ..Default::default()
        });
        // A useful correction can run between paced creation passes, but this
        // broadcaster's subscriptions have not all been established yet.
        state.stream_mode.store(1, Ordering::SeqCst);
        streams::full(&store, &providers).await.unwrap();
        maintenance.snapshot_pending = false;
        assert!(maintenance.snapshot_after_reconcile);
        assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);

        // Twitch changes state before the final subscriptions are created, so
        // no notification can be relied on for that transition.
        state.stream_mode.store(0, Ordering::SeqCst);
        maintenance.subscriptions_reconciled(&conduit::Summary {
            created: 4,
            deferred: 0,
            ..Default::default()
        });
        assert!(
            maintenance.snapshot_pending,
            "the intermediate snapshot must not consume the post-subscription correction"
        );
        assert!(!maintenance.snapshot_after_reconcile);
        streams::full(&store, &providers).await.unwrap();
        maintenance.snapshot_pending = false;
        assert!(
            store.user_by_twitch(1).await.unwrap().unwrap().is_live,
            "the final correction repairs the otherwise missed transition"
        );
        assert_eq!(state.stream_calls.load(Ordering::SeqCst), 2);
        maintenance.subscriptions_reconciled(&conduit::Summary::default());
        assert!(
            !maintenance.snapshot_pending,
            "ordinary no-change reconciliations must not continuously poll streams"
        );
        server.abort();
    }

    #[tokio::test]
    async fn single_pass_grant_activation_uses_only_its_targeted_correction() {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (_signals, signals) = watch::channel(Signal::default());
        let (_actions, actions) = actions();
        let (targets, _target_rx) = watch::channel(1);
        let (_topology_tx, topology) = watch::channel(shards::Topology {
            remote_count: 1,
            attached_count: 1,
        });
        // The shared fixture already owns a chat row for user "bot". Use a
        // different managed identity so this activation really creates all
        // four desired subscriptions rather than retaining that existing row.
        let bot_id = "activation-bot";
        let mut maintenance = Maintenance::new(
            config,
            store.clone(),
            providers.clone(),
            "conduit-1".into(),
            bot_id.into(),
            Channels {
                membership: store.channel_changes(),
                signals,
                actions,
                targets,
                topology,
            },
        );
        // An imported enabled account is already known at startup, without a
        // grant. Complete its offline startup correction before activation.
        state.stream_mode.store(1, Ordering::SeqCst);
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        maintenance.subscriptions_reconciled(&conduit::Summary::default());
        streams::full(&store, &providers).await.unwrap();
        maintenance.snapshot_pending = false;
        let baseline = state.stream_calls.load(Ordering::SeqCst);
        store.call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
                [],
            )?;
            Ok(())
        }).await.unwrap();
        state.stream_mode.store(0, Ordering::SeqCst);
        maintenance.refresh_membership().await.unwrap();
        let desired = conduit::desired(maintenance.grants.as_ref().unwrap(), bot_id).unwrap();
        let summary = maintenance
            .subs
            .reconcile(&providers, &desired, true, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(summary.created, 4);
        assert_eq!(summary.deferred, 0);
        maintenance.subscriptions_reconciled(&summary);
        assert!(
            !maintenance.snapshot_pending,
            "a normal creation pass must not duplicate the grant-activation lookup"
        );
        maintenance.run_pending().await.unwrap();
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        assert_eq!(state.stream_calls.load(Ordering::SeqCst), baseline + 1);
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        maintenance.subscriptions_reconciled(&conduit::Summary::default());
        assert!(!maintenance.snapshot_pending);
        assert_eq!(
            state.stream_calls.load(Ordering::SeqCst),
            baseline + 1,
            "unrelated membership refreshes must not repeat the correction"
        );
        server.abort();
    }

    /// Retry a cheap observation with a bounded timeout instead of sleeping
    /// for a guessed duration.
    async fn poll_until(mut condition: impl FnMut() -> bool, what: &str) {
        for _ in 0..600 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("bounded wait for {what} expired");
    }

    #[tokio::test]
    async fn session_recovery_audits_and_snapshots_without_reprovisioning_subscriptions() {
        use serde_json::json;

        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        store
            .call(|db| {
                db.execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        // The channel was fully subscribed before its shard dropped. Only the
        // live transition happened while disconnected, so the recovery re-list
        // must find every desired subscription already established.
        *state.seeded.lock().unwrap() = Some(vec![
            json!({"id":"state-online","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
            json!({"id":"state-offline","status":"enabled","type":"stream.offline","version":"1","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
            json!({"id":"state-update","status":"enabled","type":"channel.update","version":"2","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
            json!({"id":"chat-1","status":"enabled","type":"channel.chat.message","version":"1","condition":{"broadcaster_user_id":"1","user_id":"bot"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
        ]);
        state.stream_mode.store(1, Ordering::SeqCst);
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (signals, signal_rx) = watch::channel(Signal::default());
        let (_actions, action_rx) = actions();
        let (targets, _target_rx) = watch::channel(1);
        let (topology_tx, topology) = watch::channel(shards::Topology {
            remote_count: 1,
            attached_count: 1,
        });
        let maintenance = Maintenance::new(
            config,
            store.clone(),
            providers,
            "conduit-1".into(),
            "bot".into(),
            Channels {
                membership: store.channel_changes(),
                signals: signal_rx,
                actions: action_rx,
                targets,
                topology,
            },
        );
        let shutdown = CancellationToken::new();
        let worker = tokio::spawn(maintenance.run(shutdown.clone()));
        // Converged: startup membership/audit kept the seeded subscriptions,
        // then the full snapshot and the activation lookup completed.
        poll_until(
            || {
                state.list_calls.load(Ordering::SeqCst) >= 1
                    && state.stream_calls.load(Ordering::SeqCst) >= 2
            },
            "maintenance startup convergence",
        )
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let audited = state.list_calls.load(Ordering::SeqCst);
        let snapshots = state.stream_calls.load(Ordering::SeqCst);
        assert_eq!(
            (audited, snapshots),
            (1, 2),
            "startup ran one audit, the full snapshot and the activation lookup"
        );
        assert_eq!(
            (
                state.creates.load(Ordering::SeqCst),
                state.deletes.load(Ordering::SeqCst)
            ),
            (0, 0),
            "startup must reuse the established subscriptions instead of provisioning them"
        );
        assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);

        // The shard drops while the provider goes live, then reattaches. The
        // foreground topology observer requests exactly this audit and snapshot.
        topology_tx.send_replace(shards::Topology {
            remote_count: 1,
            attached_count: 0,
        });
        state.stream_mode.store(0, Ordering::SeqCst);
        topology_tx.send_replace(shards::Topology {
            remote_count: 1,
            attached_count: 1,
        });
        request(&signals, true, true);

        let mut corrected = false;
        for _ in 0..600 {
            if store.user_by_twitch(1).await.unwrap().unwrap().is_live {
                corrected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            corrected,
            "bounded wait for the recovery snapshot correction expired"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), worker)
            .await
            .expect("maintenance loop stops when cancelled")
            .unwrap();

        // The recovery re-listed the owned subscriptions, found them complete
        // and provisioned nothing: a topology/session change never recreates
        // or deletes subscriptions. Exactly one guarded full snapshot repaired
        // the transition that was missed while disconnected.
        assert_eq!(
            state.list_calls.load(Ordering::SeqCst),
            audited + 1,
            "the recovery signal must run the remote audit"
        );
        assert_eq!(
            state.creates.load(Ordering::SeqCst),
            0,
            "recovery must not recreate existing owned subscriptions"
        );
        assert_eq!(
            state.deletes.load(Ordering::SeqCst),
            0,
            "recovery must not delete existing owned subscriptions"
        );
        assert_eq!(
            state.stream_calls.load(Ordering::SeqCst),
            snapshots + 1,
            "exactly one revision-guarded full snapshot"
        );
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        server.abort();
    }
}
