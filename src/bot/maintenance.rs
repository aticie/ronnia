//! One owner for membership, subscription reconciliation and stream snapshots.
//! EventSub owns timely transitions; Helix snapshots fill in and repair the
//! truth, revision guarded so a slow response never overwrites a newer event.
//! HTTP work never runs on the foreground EventSub dispatcher.

use super::{backoff::Backoff, conduit, transport::Topology};
use crate::{config::Config, domain::StreamSnapshot, providers::Providers, storage::Store};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use rusqlite::OptionalExtension;
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ops::Bound::{Excluded, Unbounded},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

/// Remote subscription audit; heals subscriptions revoked behind our back.
const AUDIT_INTERVAL: Duration = Duration::from_secs(600);
/// Coalesce targeted category lookups triggered by `stream.online` bursts.
const TARGET_DEBOUNCE: Duration = Duration::from_millis(300);
const MAX_RESYNC_ATTEMPTS: u32 = 5;
pub(super) const TARGET_CAPACITY: usize = 1024;
const RETRY_BASE: Duration = Duration::from_secs(1);
const RETRY_CAP: Duration = Duration::from_secs(60);

// --- Stream snapshots ------------------------------------------------------

/// Observe the live state of every enabled channel (`None`) or of the given
/// revision-guarded `ids`. Returns channels whose snapshot lost the revision
/// race and therefore need a targeted retry.
pub(super) async fn snapshot(
    store: &Store,
    providers: &Providers,
    expected: Option<BTreeMap<i64, i64>>,
) -> Result<Vec<i64>> {
    let expected: BTreeMap<i64, i64> = match expected {
        Some(expected) => {
            let ids: Vec<i64> = expected.keys().copied().collect();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let current: BTreeMap<i64, i64> =
                store.snapshot_versions(ids).await?.into_iter().collect();
            expected
                .into_iter()
                .filter(|(id, revision)| current.get(id) == Some(revision))
                .collect()
        }
        None => {
            let ids = store.enabled_twitch_ids().await?;
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            store.snapshot_versions(ids).await?.into_iter().collect()
        }
    };
    if expected.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<i64> = expected.keys().copied().collect();
    let live = providers.live_streams(&ids).await?;
    apply(store, &ids, &expected, live).await
}

pub(super) async fn apply(
    store: &Store,
    ids: &[i64],
    expected: &BTreeMap<i64, i64>,
    live: Vec<crate::providers::LiveStream>,
) -> Result<Vec<i64>> {
    let mut observed: BTreeMap<i64, (String, String)> = BTreeMap::new();
    for stream in live {
        if !expected.contains_key(&stream.id) {
            continue;
        }
        observed.insert(stream.id, (stream.login, stream.category_id));
    }
    let snapshots: Vec<StreamSnapshot> = ids
        .iter()
        .filter_map(|id| {
            let expected_revision = *expected.get(id)?;
            Some(match observed.get(id) {
                Some((login, category_id)) => StreamSnapshot {
                    twitch_id: *id,
                    expected_revision,
                    online: true,
                    category_id: category_id.clone(),
                    login: Some(login.clone()),
                },
                None => StreamSnapshot {
                    twitch_id: *id,
                    expected_revision,
                    online: false,
                    category_id: String::new(),
                    login: None,
                },
            })
        })
        .collect();
    if snapshots.is_empty() {
        return Ok(Vec::new());
    }
    let desired: BTreeMap<i64, (bool, String)> = snapshots
        .iter()
        .map(|snapshot| {
            (
                snapshot.twitch_id,
                (snapshot.online, snapshot.category_id.clone()),
            )
        })
        .collect();
    store.apply_stream_snapshot(snapshots).await?;
    // A snapshot that lost the revision race leaves the newer state in place.
    // Comparing the compact stored projection detects that without depending on
    // how many events arrived meanwhile.
    let actual = raw_states(store, ids).await?;
    Ok(desired
        .into_iter()
        .filter(|(id, state)| actual.get(id) != Some(state))
        .map(|(id, _)| id)
        .collect())
}

/// Compact projection of the stored per-channel state, used only to detect a
/// snapshot that was skipped by the revision guard.
async fn raw_states(store: &Store, ids: &[i64]) -> Result<BTreeMap<i64, (bool, String)>> {
    let ids = ids.to_vec();
    store
        .call(move |db| {
            let mut statement = db.prepare(
                "SELECT online,category_id FROM channel_stream_state WHERE twitch_id=?1",
            )?;
            let mut rows = BTreeMap::new();
            for id in ids {
                if let Some(state) = statement
                    .query_row([id], |row| {
                        Ok((row.get::<_, bool>(0)?, row.get::<_, String>(1)?))
                    })
                    .optional()?
                {
                    rows.insert(id, state);
                }
            }
            Ok(rows)
        })
        .await
}

// --- Membership and subscription upkeep ------------------------------------

#[derive(Clone, Copy, Debug)]
pub(super) struct Signal {
    pub(super) revision: u64,
    pub(super) audit_revision: u64,
    pub(super) snapshot_revision: u64,
    pub(super) consent_revision: u64,
}

impl Default for Signal {
    fn default() -> Self {
        Self {
            revision: 1,
            audit_revision: 1,
            snapshot_revision: 1,
            consent_revision: 0,
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
    Revoked { id: i64, expected_revision: i64 },
}

pub(super) fn actions() -> (mpsc::Sender<Action>, mpsc::Receiver<Action>) {
    mpsc::channel(TARGET_CAPACITY)
}

/// Failed admission preserves the appropriate full-snapshot or consent-scan obligation.
pub(super) fn enqueue(
    actions: &mpsc::Sender<Action>,
    signals: &watch::Sender<Signal>,
    action: Action,
) {
    if let Err(error) = actions.try_send(action) {
        if matches!(error.into_inner(), Action::Revoked { .. }) {
            signals.send_modify(|signal| {
                signal.consent_revision = signal.consent_revision.saturating_add(1);
            });
        } else {
            request(signals, false, true);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Lookup {
    pub(super) due: Instant,
    pub(super) attempt: u32,
    // None is a membership/snapshot correction, which captures a fresh revision.
    // Some is event-triggered work and must never adopt a newer revision.
    pub(super) expected: Option<i64>,
}

#[derive(Default)]
pub(super) struct Pending {
    pub(super) due: BTreeMap<i64, Lookup>,
}

impl Pending {
    pub(super) fn insert(&mut self, id: i64, lookup: Lookup) -> bool {
        if self.due.len() >= conduit::MAX_ACTIVE_GRANTS && !self.due.contains_key(&id) {
            return false;
        }
        self.due.insert(id, lookup);
        true
    }

    pub(super) fn fresh(&mut self, id: i64) -> bool {
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

    pub(super) fn schedule(&mut self, id: i64, expected: i64) -> bool {
        self.insert(
            id,
            Lookup {
                due: Instant::now() + TARGET_DEBOUNCE,
                attempt: 0,
                expected: Some(expected),
            },
        )
    }

    pub(super) fn retry(&mut self, id: i64, lookup: Lookup) -> bool {
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

    pub(super) fn take_due(&mut self) -> BTreeMap<i64, Lookup> {
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

/// The worker's bounded work inboxes and topology-control endpoints.
pub(super) struct Channels {
    pub(super) membership: watch::Receiver<u64>,
    pub(super) signals: watch::Receiver<Signal>,
    pub(super) actions: mpsc::Receiver<Action>,
    pub(super) targets: watch::Sender<u32>,
    pub(super) topology: watch::Receiver<Topology>,
}

pub(super) struct Maintenance {
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    revoked: BTreeMap<i64, i64>,
    revocation_due: Option<Instant>,
    revocation_retry: Backoff,
    revocation_cursor: i64,
    consent_scan_after: Option<i64>,
    bot_id: String,
    pub(super) subs: conduit::Subscriptions,
    pub(super) pending: Pending,
    known: BTreeSet<i64>,
    known_granted: BTreeSet<i64>,
    pub(super) grants: Option<Vec<i64>>,
    channels: watch::Receiver<u64>,
    signals: watch::Receiver<Signal>,
    actions: mpsc::Receiver<Action>,
    targets: watch::Sender<u32>,
    topology: watch::Receiver<Topology>,
    capped: bool,
    over_capacity: bool,
    pub(super) snapshot_pending: bool,
    pub(super) snapshot_after_reconcile: bool,
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
            revoked: BTreeMap::new(),
            revocation_due: None,
            revocation_retry: Backoff::new(RETRY_BASE, RETRY_CAP),
            revocation_cursor: 0,
            consent_scan_after: None,
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
    pub(super) fn subscriptions_reconciled(&mut self, summary: &conduit::Summary) {
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

    pub(super) fn drain_actions(&mut self) {
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
                Action::Revoked {
                    id,
                    expected_revision,
                } => self.queue_revocation(id, expected_revision),
            }
        }
    }

    fn queue_revocation(&mut self, id: i64, expected_revision: i64) {
        if self.revoked.len() < conduit::MAX_ACTIVE_GRANTS || self.revoked.contains_key(&id) {
            self.revoked
                .entry(id)
                .and_modify(|revision| *revision = (*revision).max(expected_revision))
                .or_insert(expected_revision);
            self.revocation_due.get_or_insert_with(|| {
                (Instant::now() + TARGET_DEBOUNCE)
                    .max(self.revocation_retry.until().unwrap_or_else(Instant::now))
            });
        } else {
            self.consent_scan_after.get_or_insert(0);
            tracing::warn!(
                id,
                "revocation inbox full; coalescing authoritative consent rescan"
            );
        }
    }

    pub(super) fn require_consent_rescan(&mut self) {
        self.consent_scan_after = Some(0);
    }

    pub(super) async fn rescan_revocations(&mut self) -> Result<()> {
        let Some(after) = self.consent_scan_after else {
            return Ok(());
        };
        let available = conduit::MAX_ACTIVE_GRANTS.saturating_sub(self.revoked.len());
        if available == 0 {
            return Ok(());
        }
        let grants = self.store.call(move |db| {
            let mut statement = db.prepare(
                "SELECT twitch_id,revision FROM twitch_channel_grants WHERE status='granted' AND twitch_id>?1 ORDER BY twitch_id LIMIT ?2"
            )?;
            let grants = statement.query_map([after, available as i64],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(grants)
        }).await?;
        // Scan the durable source in bounded pages, including disabled/grace
        // accounts, instead of assuming all stored grants fit the active cap.
        self.consent_scan_after = if grants.len() == available {
            grants.last().map(|(id, _)| *id)
        } else {
            None
        };
        for (id, revision) in grants {
            self.queue_revocation(id, revision);
        }
        Ok(())
    }

    pub(super) async fn check_revocations(&mut self, shutdown: &CancellationToken) -> Result<()> {
        self.revocation_due = None;
        // Authorization/users accepts ten IDs. Rotate through the bounded inbox
        // so an omitted/deleted user never starves later consent checks.
        let batch: Vec<(i64, i64)> = self
            .revoked
            .range((Excluded(self.revocation_cursor), Unbounded))
            .chain(self.revoked.range(..=self.revocation_cursor))
            .take(10)
            .map(|(id, revision)| (*id, *revision))
            .collect();
        let Some((last, _)) = batch.last() else {
            return Ok(());
        };
        self.revocation_cursor = *last;
        let result: Result<bool> = async {
            let query = batch.iter().map(|(id, _)| ("user_id".to_owned(), id.to_string())).collect::<Vec<_>>();
            let value = tokio::select! {
                _ = shutdown.cancelled() => bail!("authorization lookup cancelled"),
                result = self.providers.twitch_request(Method::GET, "authorization/users", &query, None) => result?,
            };
            let rows = value["data"].as_array().context("authorization lookup omitted data")?;
            let mut authorized = BTreeMap::new();
            for row in rows {
                let id = row["user_id"].as_str().context("authorization row omitted user_id")?.parse::<i64>()?;
                ensure!(batch.iter().any(|(requested, _)| *requested == id), "authorization lookup returned an unrequested user");
                let granted = row["has_authorized"].as_bool().context("authorization row omitted has_authorized")?;
                let scopes = row["scopes"].as_array().context("authorization row omitted scopes")?;
                ensure!(scopes.iter().all(|scope| scope.is_string()), "authorization row contained malformed scopes");
                let consent = granted && scopes.iter().any(|scope| scope.as_str() == Some("channel:bot"));
                ensure!(authorized.insert(id, consent).is_none(), "authorization lookup repeated a user");
            }
            for (id, revision) in &batch {
                let Some(consent) = authorized.get(id) else { continue };
                // Only authoritative broadcaster consent, never bot token
                // health, can remove a streamer's stored authorization.
                if !consent && self.store.revoke_twitch_channel_if_current(*id, *revision).await? {
                    tracing::warn!(broadcaster = *id, "channel grant marked revoked after checking broadcaster authorization");
                }
                self.revoked.remove(id);
            }
            Ok(authorized.len() == batch.len())
        }.await;
        if matches!(result, Ok(true)) {
            self.revocation_retry.reset();
        } else {
            self.revocation_retry.fail();
            tracing::warn!(
                "broadcaster authorization lookup incomplete; retaining pending consent checks"
            );
        }
        if !self.revoked.is_empty() {
            self.revocation_due = Some(
                self.revocation_retry
                    .until()
                    .unwrap_or_else(|| Instant::now() + TARGET_DEBOUNCE),
            );
        }
        result.map(|_| ())
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
        match snapshot(&self.store, &self.providers, Some(expected)).await {
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
        let mut member_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut audit_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut reconcile_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut snapshot_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut consent_scan_retry = Backoff::new(RETRY_BASE, RETRY_CAP);
        let mut consent_ack = 0;
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
            if consent_ack < signal.consent_revision {
                self.require_consent_rescan();
                consent_ack = signal.consent_revision;
            }
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

            if self.consent_scan_after.is_some()
                && self.revoked.len() < conduit::MAX_ACTIVE_GRANTS
                && consent_scan_retry.ready(now)
            {
                match self.rescan_revocations().await {
                    Ok(()) => consent_scan_retry.reset(),
                    Err(error) => {
                        consent_scan_retry.fail();
                        tracing::warn!(error = %error, "could not rescan broadcaster consent; retaining obligation");
                    }
                }
                continue;
            }
            if self.revocation_due.is_some_and(|due| due <= now) {
                if let Err(error) = self.check_revocations(&shutdown).await {
                    tracing::warn!(error = %error, "could not apply channel revocation");
                }
                membership_pending = true;
                reconcile_pending = true;
                continue;
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
                        member_retry.reset();
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
                        audit_retry.reset();
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
                reconcile_retry.reset();
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
                        reconcile_retry.reset();
                        reconcile_pending = summary.deferred > 0;
                        if reconcile_pending {
                            // Progress passes continue promptly; provider/permission
                            // blocks retain their own 60..600-second backoff.
                            reconcile_retry.set_until(
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
                match snapshot(&self.store, &self.providers, None).await {
                    Ok(skipped) => {
                        self.snapshot_pending = false;
                        snapshot_ack = signal.snapshot_revision;
                        snapshot_retry.reset();
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
            if membership_pending && let Some(until) = member_retry.until() {
                deadline = deadline.min(until);
            }
            if audit_pending
                && within_capacity
                && let Some(until) = audit_retry.until()
            {
                deadline = deadline.min(until);
            }
            if reconcile_pending
                && !membership_pending
                && !audit_pending
                && within_capacity
                && let Some(until) = reconcile_retry.until()
            {
                deadline = deadline.min(until);
            }
            if self.snapshot_pending
                && let Some(until) = snapshot_retry.until()
            {
                deadline = deadline.min(until);
            }
            if let Some(until) = self.pending.next_deadline() {
                deadline = deadline.min(until);
            }
            if let Some(until) = self.revocation_due {
                deadline = deadline.min(until);
            }
            if self.consent_scan_after.is_some()
                && self.revoked.len() < conduit::MAX_ACTIVE_GRANTS
                && let Some(until) = consent_scan_retry.until()
            {
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
                        Some(Action::Revoked { id, expected_revision }) => self.queue_revocation(id, expected_revision),
                        None => actions_open = false,
                    }
                },
            }
        }
    }
}
