//! Owned EventSub conduit, shard attachment and subscription reconciliation.
//!
//! Every call uses the application access token. Relates only to the conduit
//! this process persisted: unrelated conduits and subscriptions are never
//! listed for deletion, and no account is ever touched here.

use crate::{config::Config, providers::Providers};
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

use crate::storage::Store;

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
const MAX_EVENTS_SUBSCRIPTIONS: usize = 10_000;
/// Four owned subscriptions per active grant must fit in one complete audit.
pub const MAX_ACTIVE_GRANTS: usize = MAX_EVENTS_SUBSCRIPTIONS / (STATE_TYPES.len() + 1);

pub fn owned_version(kind: &str) -> Option<&'static str> {
    STATE_TYPES
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, version)| *version)
        .or_else(|| (kind == CHAT_TYPE.0).then_some(CHAT_TYPE.1))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    pub kind: &'static str,
    pub version: &'static str,
    pub broadcaster: i64,
    /// Bot user ID; present only for chat subscriptions.
    pub user: Option<String>,
}

/// Subscription set for one runtime identity. Both state and chat subscriptions
/// cover every enabled+granted channel; stream state only gates request handling.
pub fn desired(grants: &[i64], bot_id: &str) -> Result<Vec<Desired>> {
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

#[derive(Default)]
struct Blocked {
    until: Option<Instant>,
    delay: Duration,
}

impl Blocked {
    fn ready(&mut self, now: Instant) -> bool {
        self.until.is_none_or(|until| now >= until)
    }

    fn fail(&mut self, now: Instant) {
        self.delay = if self.delay.is_zero() {
            Duration::from_secs(60)
        } else {
            (self.delay * 2).min(Duration::from_secs(600))
        };
        self.until = Some(now + self.delay);
    }
}

/// Locally cached view of the subscriptions owned by this conduit. The remote
/// list is refreshed at startup and by the ten minute audit; ordinary chat
/// traffic never triggers a full HTTP list.
pub struct Subscriptions {
    conduit: String,
    bot_id: String,
    remote: BTreeMap<String, Remote>,
    warm: bool,
    blocked: BTreeMap<String, Blocked>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub created: usize,
    pub removed: usize,
    pub kept: usize,
    pub deferred: usize,
}

impl Subscriptions {
    pub fn new(conduit: String, bot_id: String) -> Self {
        Self {
            conduit,
            bot_id,
            remote: BTreeMap::new(),
            warm: false,
            blocked: BTreeMap::new(),
        }
    }

    #[cfg(test)]
    pub fn is_warm(&self) -> bool {
        self.warm
    }

    /// Replace the cache with the complete (paginated) remote list for our
    /// conduit. Failures leave the previous cache intact.
    pub async fn refresh_remote(
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
                let kind = row["type"].as_str().unwrap_or_default();
                if owned_version(kind).is_none() {
                    continue;
                }
                if row["transport"]["method"].as_str() != Some("conduit")
                    || row["transport"]["conduit_id"].as_str() != Some(self.conduit.as_str())
                {
                    continue;
                }
                let Some(broadcaster) = row["condition"]["broadcaster_user_id"]
                    .as_str()
                    .and_then(|id| id.parse::<i64>().ok())
                else {
                    continue;
                };
                let user = row["condition"]["user_id"].as_str().map(|id| id.to_owned());
                rows.insert(
                    id.to_owned(),
                    Remote {
                        kind: kind.to_owned(),
                        broadcaster,
                        user,
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
                .map(|cursor| cursor.to_owned());
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
    /// Incomplete topology gates only missing creations; cache refresh and
    /// obsolete owned deletion must continue while shard recovery is pending.
    pub async fn reconcile(
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
                Err(error) => {
                    tracing::warn!(subscription = %id, error = %error, "could not delete obsolete owned EventSub subscription; will retry on the audit");
                }
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
            let blocked = self.blocked.entry(block_key.clone()).or_default();
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
                    self.blocked.entry(block_key).or_default().fail(now);
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
pub struct Conduit {
    pub id: String,
    pub shard_count: u32,
}

/// Reuse the persisted conduit and its observed topology when the client
/// identity still matches; otherwise create the one-shard conduit we own.
pub async fn ensure(config: &Config, store: &Store, providers: &Providers) -> Result<Conduit> {
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

fn parse_conduit(row: &Value) -> Result<Conduit> {
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
            .map(|cursor| cursor.to_owned());
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
pub async fn observe(providers: &Providers, conduit: &str) -> Result<Conduit> {
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
pub async fn resize(providers: &Providers, conduit: &str, shard_count: u32) -> Result<()> {
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

fn validate_resize(value: &Value, conduit: &str, shard_count: u32) -> Result<()> {
    let updated = single_conduit(value)?;
    ensure!(
        updated.id == conduit && updated.shard_count == shard_count,
        "conduit resize response did not match requested id {conduit:?} and shard_count {shard_count}"
    );
    Ok(())
}

/// Bind a freshly welcomed WebSocket session to the requested shard. Must
/// happen within ten seconds of welcome; the caller enforces the deadline.
pub async fn attach(
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

fn validate_attachment(value: &Value, shard_id: u32, session_id: &str) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_contains_state_and_chat_for_every_granted_channel() {
        let rows = desired(&[1, 2], "bot").unwrap();
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == "stream.online")
                .count(),
            2
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == "channel.update")
                .count(),
            2
        );
        assert_eq!(rows.iter().filter(|row| row.kind == CHAT_TYPE.0).count(), 2);
        assert!(rows.iter().any(|row| row.kind == CHAT_TYPE.0
            && row.broadcaster == 2
            && row.user.as_deref() == Some("bot")));
        assert!(rows.iter().any(|row| row.kind == CHAT_TYPE.0
            && row.broadcaster == 1
            && row.user.as_deref() == Some("bot")));
        assert!(
            rows.iter()
                .filter(|row| row.kind == CHAT_TYPE.0)
                .all(|row| row.user.is_some())
        );
    }

    #[test]
    fn desired_rejects_over_capacity_without_a_partial_set() {
        let grants: Vec<i64> = (1..=MAX_ACTIVE_GRANTS as i64).collect();
        assert_eq!(
            desired(&grants, "bot").unwrap().len(),
            MAX_EVENTS_SUBSCRIPTIONS
        );
        let mut oversized = grants;
        oversized.push(MAX_ACTIVE_GRANTS as i64 + 1);
        let error = desired(&oversized, "bot").unwrap_err().to_string();
        assert!(error.contains("2500-grant"));
        assert!(error.contains("without partial reconciliation"));
    }

    #[test]
    fn remote_conduit_requires_positive_valid_topology_without_truncation() {
        let conduit = parse_conduit(&json!({"id":"owned","shard_count":65})).unwrap();
        assert_eq!(
            conduit.shard_count, 65,
            "pool must observe over-bound topology"
        );
        for row in [
            json!({"id":"owned"}),
            json!({"id":"owned","shard_count":0}),
            json!({"id":"owned","shard_count":-1}),
            json!({"id":"owned","shard_count":"3"}),
            json!({"id":"owned","shard_count":u64::from(u32::MAX) + 1}),
            json!({"id":"","shard_count":1}),
            json!({"shard_count":1}),
        ] {
            assert!(parse_conduit(&row).is_err(), "accepted invalid row: {row}");
        }
    }

    #[test]
    fn resize_response_must_match_the_owned_id_and_requested_count() {
        assert!(
            validate_resize(
                &json!({"data":[{"id":"owned","shard_count":3}]}),
                "owned",
                3
            )
            .is_ok()
        );
        for value in [
            json!({"data":[{"id":"foreign","shard_count":3}]}),
            json!({"data":[{"id":"owned","shard_count":2}]}),
            json!({"data":[{"id":"owned","shard_count":0}]}),
            json!({"data":[{"id":"owned"}]}),
            json!({"data":[]}),
            json!({"data":[{"id":"owned","shard_count":3},{"id":"foreign","shard_count":1}]}),
        ] {
            assert!(validate_resize(&value, "owned", 3).is_err());
        }
    }

    #[test]
    fn attachment_response_requires_exact_numeric_shard_and_session() {
        let attached = json!({
            "data":[{"id":"12","status":"enabled","transport":{
                "method":"websocket","session_id":"session-new"
            }}]
        });
        assert!(validate_attachment(&attached, 12, "session-new").is_ok());
        assert!(validate_attachment(&attached, 0, "session-new").is_err());
        assert!(validate_attachment(&attached, 12, "session-old").is_err());

        for (field, invalid) in [
            ("id", json!("012")),
            ("id", json!(12)),
            ("status", json!("websocket_disconnected")),
            (
                "transport",
                json!({"method":"websocket","session_id":"other"}),
            ),
            (
                "transport",
                json!({"method":"webhook","session_id":"session-new"}),
            ),
            ("transport", Value::Null),
            ("errors", json!([{"code":"websocket_disconnected"}])),
        ] {
            let mut value = attached.clone();
            value["data"][0][field] = invalid;
            assert!(
                validate_attachment(&value, 12, "session-new").is_err(),
                "accepted invalid attachment: {value}"
            );
        }
        let mut top_level_error = attached.clone();
        top_level_error["errors"] = json!([{"id":"12","code":"websocket_disconnected"}]);
        assert!(validate_attachment(&top_level_error, 12, "session-new").is_err());
        let mut duplicate = attached;
        let row = duplicate["data"][0].clone();
        duplicate["data"].as_array_mut().unwrap().push(row);
        assert!(validate_attachment(&duplicate, 12, "session-new").is_err());
    }

    #[test]
    fn owned_types_ignore_unrelated_subscriptions() {
        assert!(owned_version("channel.chat.message").is_some());
        assert!(owned_version("channel.update").is_some());
        assert!(owned_version("conduit.shard.disabled").is_none());
        assert!(owned_version("channel.follow").is_none());
    }

    #[test]
    fn blocked_backoff_grows_capped_and_recovers() {
        let now = Instant::now();
        let mut blocked = Blocked::default();
        assert!(blocked.ready(now));
        blocked.fail(now);
        assert!(!blocked.ready(now));
        assert_eq!(blocked.delay, Duration::from_secs(60));
        blocked.fail(now);
        assert_eq!(blocked.delay, Duration::from_secs(120));
        for _ in 0..10 {
            blocked.fail(now);
        }
        assert_eq!(blocked.delay, Duration::from_secs(600));
        blocked.until = None;
        assert!(blocked.ready(now));
    }
}
