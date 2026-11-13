//! Owned EventSub conduit: shard attachment and subscription reconciliation.
//!
//! Every call uses the application token and only ever touches the conduit this
//! process persisted; no account is modified here.

use super::backoff::Backoff;
use crate::{config::Config, providers::Providers, storage::Store};
use anyhow::{Context, Result, bail, ensure};
use reqwest::Method;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

/// The only subscription types this bot owns, with their current versions.
pub const STATE_TYPES: [(&str, &str); 3] = [
    ("stream.online", "1"),
    ("stream.offline", "1"),
    ("channel.update", "2"),
];
pub const CHAT_TYPE: (&str, &str) = ("channel.chat.message", "1");
const MAX_LIST_PAGES: usize = 100;
const CREATE_PACE: Duration = Duration::from_millis(200);
const MAX_CREATIONS_PER_PASS: usize = 200;
pub(super) const MAX_EVENTS_SUBSCRIPTIONS: usize = 10_000;
/// Four owned subscriptions per active grant must fit in one complete audit.
pub(super) const MAX_ACTIVE_GRANTS: usize = MAX_EVENTS_SUBSCRIPTIONS / (STATE_TYPES.len() + 1);
const BLOCK_BASE: Duration = Duration::from_secs(60);
const BLOCK_CAP: Duration = Duration::from_secs(600);

pub(super) fn owned_version(kind: &str) -> Option<&'static str> {
    STATE_TYPES
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, version)| *version)
        .or_else(|| (kind == CHAT_TYPE.0).then_some(CHAT_TYPE.1))
}

/// Identity of a subscription row owned by `conduit_id`. When `bot_id` is
/// given, chat subscriptions bound to another bot are rejected. This is the one
/// place subscription identity is parsed, shared with dispatch and revocation.
pub(super) struct Owned<'a> {
    pub(super) kind: &'a str,
    pub(super) broadcaster: i64,
    pub(super) user: Option<&'a str>,
}

pub(super) fn owned<'a>(
    subscription: &'a Value,
    conduit_id: &str,
    bot_id: Option<&str>,
) -> Option<Owned<'a>> {
    let kind = subscription["type"].as_str()?;
    let version = subscription["version"].as_str()?;
    let expected = owned_version(kind)?;
    if version != expected {
        tracing::warn!(
            kind,
            version,
            expected,
            "ignoring an EventSub subscription with an unexpected version"
        );
        return None;
    }
    if subscription["transport"]["method"].as_str() != Some("conduit")
        || subscription["transport"]["conduit_id"].as_str() != Some(conduit_id)
    {
        tracing::warn!(
            kind,
            "ignoring an EventSub subscription from a foreign transport"
        );
        return None;
    }
    let broadcaster = subscription["condition"]["broadcaster_user_id"]
        .as_str()
        .and_then(|id| id.parse::<i64>().ok())?;
    let user = subscription["condition"]["user_id"].as_str();
    if kind == CHAT_TYPE.0 && bot_id.is_some_and(|bot| user != Some(bot)) {
        tracing::warn!(
            broadcaster,
            "ignoring a chat subscription that is not bound to the managed bot"
        );
        return None;
    }
    Some(Owned {
        kind,
        broadcaster,
        user,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Desired {
    pub(super) kind: &'static str,
    pub(super) version: &'static str,
    pub(super) broadcaster: i64,
    /// Bot user ID; present only for chat subscriptions.
    pub(super) user: Option<String>,
}

/// Subscription set for one runtime identity. Both state and chat subscriptions
/// cover every enabled+granted channel; stream state only gates request handling.
pub(super) fn desired(grants: &[i64], bot_id: &str) -> Result<Vec<Desired>> {
    ensure!(
        grants.len() <= MAX_ACTIVE_GRANTS,
        "EventSub degraded capacity: {} active grants exceed the {MAX_ACTIVE_GRANTS}-grant / {MAX_EVENTS_SUBSCRIPTIONS}-subscription audit safety bound; retaining existing subscriptions without partial reconciliation",
        grants.len()
    );
    let mut desired = Vec::with_capacity(grants.len() * (STATE_TYPES.len() + 1));
    for id in grants {
        for (kind, version) in STATE_TYPES {
            desired.push(Desired {
                kind,
                version,
                broadcaster: *id,
                user: None,
            });
        }
        desired.push(Desired {
            kind: CHAT_TYPE.0,
            version: CHAT_TYPE.1,
            broadcaster: *id,
            user: Some(bot_id.to_owned()),
        });
    }
    Ok(desired)
}

#[derive(Clone, Debug)]
struct Remote {
    kind: String,
    broadcaster: i64,
    user: Option<String>,
}

/// Locally cached view of the subscriptions owned by this conduit. The remote
/// list is refreshed at startup and by the ten minute audit; ordinary chat
/// traffic never triggers a full HTTP list.
pub(super) struct Subscriptions {
    conduit: String,
    bot_id: String,
    remote: BTreeMap<String, Remote>,
    warm: bool,
    blocked: BTreeMap<String, Backoff>,
}

#[derive(Debug, Default)]
pub(super) struct Summary {
    pub(super) created: usize,
    pub(super) removed: usize,
    pub(super) kept: usize,
    pub(super) deferred: usize,
}

impl Subscriptions {
    pub(super) fn new(conduit: String, bot_id: String) -> Self {
        Self {
            conduit,
            bot_id,
            remote: BTreeMap::new(),
            warm: false,
            blocked: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn is_warm(&self) -> bool {
        self.warm
    }

    /// Replace the cache with the complete (paginated) remote list for our
    /// conduit. Failures leave the previous cache intact.
    pub(super) async fn refresh_remote(
        &mut self,
        providers: &Providers,
        shutdown: &CancellationToken,
    ) -> Result<usize> {
        let mut query = vec![("first".to_owned(), "100".to_owned())];
        let mut seen_cursors: BTreeSet<String> = BTreeSet::new();
        let mut rows = BTreeMap::new();
        let mut pages = 0usize;
        loop {
            if shutdown.is_cancelled() {
                bail!("subscription refresh cancelled");
            }
            let value = providers
                .twitch_request(Method::GET, "eventsub/subscriptions", &query, None)
                .await?;
            let data = value["data"]
                .as_array()
                .context("Helix subscriptions response omitted data")?;
            for row in data {
                let id = row["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .context("subscription omitted id")?;
                let Some(row) = owned(row, &self.conduit, None) else {
                    continue;
                };
                rows.insert(
                    id.to_owned(),
                    Remote {
                        kind: row.kind.to_owned(),
                        broadcaster: row.broadcaster,
                        user: row.user.map(str::to_owned),
                    },
                );
                ensure!(
                    rows.len() <= MAX_EVENTS_SUBSCRIPTIONS,
                    "owned subscription list exceeds the safety bound"
                );
            }
            pages += 1;
            let next = value["pagination"]["cursor"]
                .as_str()
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            match next {
                Some(cursor) if pages < MAX_LIST_PAGES => {
                    ensure!(
                        seen_cursors.insert(cursor.clone()),
                        "subscription pagination repeated a cursor"
                    );
                    query.retain(|(name, _)| name != "after");
                    query.push(("after".to_owned(), cursor));
                }
                Some(_) => bail!("subscription pagination exceeded the page bound"),
                None => break,
            }
        }
        self.remote = rows;
        self.warm = true;
        Ok(self.remote.len())
    }

    /// Bring the remote subscription set to `desired`, deleting only entries
    /// owned by this conduit that are obsolete or bound to another bot user.
    /// Incomplete topology gates only missing creations; refresh and obsolete
    /// deletion continue while shard recovery is pending.
    pub(super) async fn reconcile(
        &mut self,
        providers: &Providers,
        desired: &[Desired],
        allow_create: bool,
        shutdown: &CancellationToken,
    ) -> Result<Summary> {
        if !self.warm {
            self.refresh_remote(providers, shutdown).await?;
        }
        for _ in 0..2 {
            match self
                .reconcile_once(providers, desired, allow_create, shutdown)
                .await?
            {
                Some(summary) => return Ok(summary),
                // A 409 means our cache lagged the remote state; re-list once.
                None => {
                    self.refresh_remote(providers, shutdown).await?;
                }
            }
        }
        Ok(Summary::default())
    }

    async fn reconcile_once(
        &mut self,
        providers: &Providers,
        desired: &[Desired],
        allow_create: bool,
        shutdown: &CancellationToken,
    ) -> Result<Option<Summary>> {
        let mut summary = Summary::default();
        let desired_map: BTreeMap<(&str, i64), &Desired> = desired
            .iter()
            .map(|row| ((row.kind, row.broadcaster), row))
            .collect();
        let obsolete: Vec<String> = self
            .remote
            .iter()
            .filter(|(_, remote)| {
                desired_map
                    .get(&(remote.kind.as_str(), remote.broadcaster))
                    .is_none_or(|wanted| {
                        remote.kind == CHAT_TYPE.0
                            && remote.user.as_deref() != wanted.user.as_deref()
                    })
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in obsolete {
            if shutdown.is_cancelled() {
                bail!("subscription reconcile cancelled");
            }
            let query = vec![("id".to_owned(), id.clone())];
            match providers
                .twitch_request(Method::DELETE, "eventsub/subscriptions", &query, None)
                .await
            {
                Ok(_) => {
                    self.remote.remove(&id);
                    summary.removed += 1;
                }
                Err(error) => tracing::warn!(
                    subscription = %id,
                    error = %error,
                    "could not delete obsolete owned EventSub subscription; will retry on the audit"
                ),
            }
        }

        let mut present: BTreeSet<(String, i64, Option<String>)> = self
            .remote
            .values()
            .map(|row| (row.kind.clone(), row.broadcaster, row.user.clone()))
            .collect();
        let now = Instant::now();
        let mut created = 0usize;
        for ((kind, broadcaster), wanted) in &desired_map {
            if created >= MAX_CREATIONS_PER_PASS {
                summary.deferred += 1;
                continue;
            }
            if *kind == CHAT_TYPE.0 && wanted.user.as_deref() != Some(self.bot_id.as_str()) {
                tracing::warn!(
                    broadcaster = *broadcaster,
                    "refusing a chat subscription not bound to the managed bot"
                );
                summary.deferred += 1;
                continue;
            }
            let key = (kind.to_string(), *broadcaster, wanted.user.clone());
            if present.contains(&key) {
                summary.kept += 1;
                continue;
            }
            if !allow_create {
                summary.deferred += 1;
                continue;
            }
            let block_key = format!("{kind}:{broadcaster}");
            let blocked = self
                .blocked
                .entry(block_key.clone())
                .or_insert_with(|| Backoff::new(BLOCK_BASE, BLOCK_CAP));
            if !blocked.ready(now) {
                summary.deferred += 1;
                continue;
            }
            if shutdown.is_cancelled() {
                bail!("subscription reconcile cancelled");
            }
            if created != 0 {
                tokio::select! {
                    _ = shutdown.cancelled() => bail!("subscription reconcile cancelled"),
                    _ = sleep(CREATE_PACE) => {}
                }
            }
            let mut condition = json!({"broadcaster_user_id": broadcaster.to_string()});
            if let Some(user) = &wanted.user {
                condition["user_id"] = json!(user);
            }
            let body = json!({
                "type": wanted.kind,
                "version": wanted.version,
                "condition": condition,
                "transport": {"method": "conduit", "conduit_id": self.conduit},
            });
            match providers
                .twitch_request(Method::POST, "eventsub/subscriptions", &[], Some(body))
                .await
            {
                Ok(value) => {
                    let id = value["data"][0]["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .context("subscription creation omitted id")?
                        .to_owned();
                    self.remote.insert(
                        id,
                        Remote {
                            kind: kind.to_string(),
                            broadcaster: *broadcaster,
                            user: wanted.user.clone(),
                        },
                    );
                    present.insert(key);
                    summary.created += 1;
                    created += 1;
                    self.blocked.remove(&block_key);
                    tracing::info!(kind = %kind, broadcaster = *broadcaster, "created EventSub subscription");
                }
                Err(error) if error.status == Some(409) => {
                    tracing::warn!(
                        kind = %kind,
                        broadcaster = *broadcaster,
                        "EventSub subscription already exists remotely; re-listing"
                    );
                    return Ok(None);
                }
                Err(error) => {
                    summary.deferred += 1;
                    self.blocked
                        .entry(block_key)
                        .or_insert_with(|| Backoff::new(BLOCK_BASE, BLOCK_CAP))
                        .fail(now);
                    tracing::warn!(
                        kind = %kind,
                        broadcaster = *broadcaster,
                        error = %error,
                        "EventSub subscription creation failed; retrying with backoff without touching accounts"
                    );
                }
            }
        }
        Ok(Some(summary))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Conduit {
    pub(super) id: String,
    pub(super) shard_count: u32,
}

/// Reuse the persisted conduit and its observed topology when the client
/// identity still matches; otherwise create the one-shard conduit we own.
pub(super) async fn ensure(
    config: &Config,
    store: &Store,
    providers: &Providers,
) -> Result<Conduit> {
    let persisted: Option<(String, String)> = store
        .call(|db| {
            Ok(db
                .query_row(
                    "SELECT client_id,conduit_id FROM twitch_conduit WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?)
        })
        .await?;
    let remote = list_conduits(providers).await?;
    if let Some((client_id, conduit_id)) = persisted
        && client_id == config.twitch_client_id
        && let Some(&shard_count) = remote.get(&conduit_id)
    {
        tracing::info!(conduit = %conduit_id, shard_count, "reusing persisted EventSub conduit");
        return Ok(Conduit {
            id: conduit_id,
            shard_count,
        });
    }
    let value = providers
        .twitch_request(
            Method::POST,
            "eventsub/conduits",
            &[],
            Some(json!({"shard_count":1})),
        )
        .await?;
    let created = single_conduit(&value)?;
    ensure!(
        created.shard_count == 1,
        "conduit creation returned {} shards instead of the requested one",
        created.shard_count
    );
    let id = created.id.clone();
    let client_id = config.twitch_client_id.clone();
    let conduit_id = id.clone();
    store
        .call(move |db| {
            db.execute(
                "INSERT INTO twitch_conduit(singleton,client_id,conduit_id,updated_at) VALUES(1,?1,?2,?3) \
                 ON CONFLICT(singleton) DO UPDATE SET client_id=excluded.client_id,conduit_id=excluded.conduit_id,updated_at=excluded.updated_at",
                params![client_id, conduit_id, chrono::Utc::now().timestamp()],
            )?;
            Ok(())
        })
        .await?;
    tracing::info!(conduit = %id, shard_count = created.shard_count, "created EventSub conduit");
    Ok(created)
}

pub(super) fn parse_conduit(row: &Value) -> Result<Conduit> {
    let id = row["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("conduit response omitted a nonempty id")?
        .to_owned();
    let shard_count = row["shard_count"]
        .as_u64()
        .and_then(|count| u32::try_from(count).ok())
        .filter(|count| *count > 0)
        .context("conduit response omitted a positive u32 shard_count")?;
    Ok(Conduit { id, shard_count })
}

fn single_conduit(value: &Value) -> Result<Conduit> {
    let rows = value["data"]
        .as_array()
        .context("conduit response omitted data")?;
    ensure!(
        rows.len() == 1,
        "conduit response must contain exactly one row"
    );
    parse_conduit(&rows[0])
}

async fn list_conduits(providers: &Providers) -> Result<BTreeMap<String, u32>> {
    let mut query = vec![("first".to_owned(), "100".to_owned())];
    let mut conduits = BTreeMap::new();
    let mut seen_cursors: BTreeSet<String> = BTreeSet::new();
    let mut pages = 0usize;
    loop {
        let value = providers
            .twitch_request(Method::GET, "eventsub/conduits", &query, None)
            .await?;
        for row in value["data"]
            .as_array()
            .context("conduit list omitted data")?
        {
            let conduit = parse_conduit(row)?;
            ensure!(
                conduits.insert(conduit.id, conduit.shard_count).is_none(),
                "conduit list repeated an id"
            );
        }
        pages += 1;
        let next = value["pagination"]["cursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned);
        let Some(next) = next else { break };
        ensure!(
            pages < MAX_LIST_PAGES,
            "conduit pagination exceeded the page bound"
        );
        ensure!(
            seen_cursors.insert(next.clone()),
            "conduit pagination repeated a cursor"
        );
        query.retain(|(name, _)| name != "after");
        query.push(("after".to_owned(), next));
    }
    Ok(conduits)
}

/// Re-observe the known owned conduit after a possibly committed resize.
/// A missing ID is an error: recovery never creates or adopts another conduit.
pub(super) async fn observe(providers: &Providers, conduit: &str) -> Result<Conduit> {
    let shard_count = list_conduits(providers)
        .await?
        .remove(conduit)
        .context("owned conduit is missing from the complete remote list")?;
    Ok(Conduit {
        id: conduit.to_owned(),
        shard_count,
    })
}

/// Resize only the supplied owned conduit. Callers commit local topology only
/// after success, and re-observe after errors because PATCH may have committed.
pub(super) async fn resize(providers: &Providers, conduit: &str, shard_count: u32) -> Result<()> {
    ensure!(!conduit.is_empty(), "cannot resize an empty conduit id");
    ensure!(shard_count > 0, "conduit shard_count must be positive");
    let value = providers
        .twitch_request(
            Method::PATCH,
            "eventsub/conduits",
            &[],
            Some(json!({"id": conduit, "shard_count": shard_count})),
        )
        .await
        .context("resizing owned EventSub conduit failed")?;
    validate_resize(&value, conduit, shard_count)
}

pub(super) fn validate_resize(value: &Value, conduit: &str, shard_count: u32) -> Result<()> {
    let updated = single_conduit(value)?;
    ensure!(
        updated.id == conduit && updated.shard_count == shard_count,
        "conduit resize response did not match requested id {conduit:?} and shard_count {shard_count}"
    );
    Ok(())
}

/// Bind a freshly welcomed WebSocket session to the requested shard. Must
/// happen within ten seconds of welcome; the caller enforces the deadline.
pub(super) async fn attach(
    providers: &Providers,
    conduit: &str,
    shard_id: u32,
    session_id: &str,
) -> Result<()> {
    ensure!(!conduit.is_empty(), "cannot attach an empty conduit id");
    ensure!(
        !session_id.is_empty(),
        "cannot attach an empty WebSocket session"
    );
    let body = json!({
        "conduit_id": conduit,
        "shards": [{"id": shard_id.to_string(), "transport": {"method": "websocket", "session_id": session_id}}],
    });
    let value = providers
        .twitch_request(Method::PATCH, "eventsub/conduits/shards", &[], Some(body))
        .await
        .context("attaching conduit shard to the new WebSocket session failed")?;
    validate_attachment(&value, shard_id, session_id)
}

pub(super) fn validate_attachment(value: &Value, shard_id: u32, session_id: &str) -> Result<()> {
    if let Some(errors) = value["errors"].as_array() {
        ensure!(
            errors.is_empty(),
            "shard update reported errors: {}",
            Value::Array(errors.clone())
        );
    }
    let shard = shard_id.to_string();
    let rows = value["data"]
        .as_array()
        .context("shard update response omitted data")?;
    ensure!(
        rows.len() == 1 && rows[0]["id"].as_str() == Some(shard.as_str()),
        "shard update response did not contain exactly the requested shard {shard_id}"
    );
    let row = &rows[0];
    if let Some(errors) = row["errors"].as_array() {
        ensure!(
            errors.is_empty(),
            "shard update reported errors: {}",
            Value::Array(errors.clone())
        );
    }
    let status = row["status"].as_str().unwrap_or_default();
    ensure!(
        status == "enabled",
        "shard {shard_id} is not enabled after attaching the new session (status {status:?})"
    );
    ensure!(
        row["transport"]["method"].as_str() == Some("websocket")
            && row["transport"]["session_id"].as_str() == Some(session_id),
        "shard {shard_id} update response did not match the requested WebSocket session"
    );
    Ok(())
}
