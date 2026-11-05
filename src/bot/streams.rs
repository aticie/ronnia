//! Hybrid live state: EventSub owns timely transitions, Helix snapshots fill in
//! and repair the truth. Snapshots are revision guarded, so a slow HTTP
//! response can never overwrite a newer event.

use crate::{domain::StreamSnapshot, providers::Providers, storage::Store};
use anyhow::Result;
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

async fn versions(store: &Store, ids: &[i64]) -> Result<BTreeMap<i64, i64>> {
    Ok(store
        .snapshot_versions(ids.to_vec())
        .await?
        .into_iter()
        .collect())
}

/// Full snapshot of every enabled channel, without a category filter so both
/// online and offline are observed. Any HTTP failure propagates and the caller
/// keeps the previous state.
///
/// Returns the channels whose snapshot was skipped because a newer event
/// arrived while the request was in flight; those need a targeted retry.
pub async fn full(store: &Store, providers: &Providers) -> Result<Vec<i64>> {
    let ids = store.enabled_twitch_ids().await?;
    targeted(store, providers, &ids).await
}

/// Targeted lookup used after `stream.online` (which carries no category) and
/// for channels whose full snapshot was revision-guarded.
pub async fn targeted(store: &Store, providers: &Providers, ids: &[i64]) -> Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let expected = versions(store, ids).await?;
    targeted_at(store, providers, &expected).await
}

/// An event-triggered lookup keeps the revision captured by the dispatcher.
/// Discard stale work before HTTP as well as guarding its eventual application.
pub async fn targeted_at(
    store: &Store,
    providers: &Providers,
    expected: &BTreeMap<i64, i64>,
) -> Result<Vec<i64>> {
    let ids: Vec<i64> = expected.keys().copied().collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let current = versions(store, &ids).await?;
    let expected: BTreeMap<i64, i64> = expected
        .iter()
        .filter(|(id, revision)| current.get(id) == Some(revision))
        .map(|(id, revision)| (*id, *revision))
        .collect();
    let ids: Vec<i64> = expected.keys().copied().collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let live = providers.live_streams(&ids).await?;
    apply(store, &ids, &expected, live).await
}

async fn apply(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::StreamEvent;

    #[tokio::test]
    async fn revision_guard_prevents_a_slow_snapshot_from_reactivating() {
        let store = Store::open(":memory:").await.unwrap();
        let user: crate::domain::User = serde_json::from_value(serde_json::json!({
            "osuId":1,"osuUsername":"a","osuAvatarUrl":"","twitchId":1,
            "twitchUsername":"streamer","twitchAvatarUrl":"","excludedUsers":[],
            "settings":{"enable":true}
        }))
        .unwrap();
        store.save_user(user).await.unwrap();
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
        store
            .apply_stream_event(1, "streamer".into(), StreamEvent::Online, 1_000)
            .await
            .unwrap();
        store
            .apply_stream_event(
                1,
                "streamer".into(),
                StreamEvent::Category("21465".into()),
                2_000,
            )
            .await
            .unwrap();
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);

        let expected = versions(&store, &[1]).await.unwrap();
        // A newer offline event lands while the HTTP snapshot is in flight.
        store
            .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 3_000)
            .await
            .unwrap();
        let skipped = apply(
            &store,
            &[1],
            &expected,
            vec![crate::providers::LiveStream {
                id: 1,
                login: "streamer".into(),
                category_id: "21465".into(),
            }],
        )
        .await
        .unwrap();
        assert_eq!(skipped, vec![1], "the stale snapshot must report a retry");
        assert!(
            !store.user_by_twitch(1).await.unwrap().unwrap().is_live,
            "a slow snapshot must not reactivate an offline channel"
        );
    }
}
