//! Behavioural tests for the storage facade: identity, eligibility, stream
//! revisions, legacy deliveries, statistics, caching, and migrations.
use super::{ClaimOutcome, DeliveryOutcome, Store, refresh_eligibility, users::OSU_CATEGORIES};
use crate::domain::{RequestEvent, Settings, StreamEvent, StreamSnapshot, User};
use rusqlite::params;
use serde_json::{Value, json};
use std::time::Duration;

fn user() -> User {
    User {
        osu_id: 1,
        osu_username: "osu".into(),
        osu_avatar_url: String::new(),
        twitch_id: 2,
        twitch_username: "CHANNEL".into(),
        twitch_avatar_url: String::new(),
        excluded_users: vec!["VIEWER".into()],
        is_live: false,
        settings: Settings::default(),
    }
}

fn account(osu_id: i64, twitch_id: i64, login: &str) -> User {
    User {
        osu_id,
        osu_username: format!("player{osu_id}"),
        osu_avatar_url: String::new(),
        twitch_id,
        twitch_username: login.into(),
        twitch_avatar_url: String::new(),
        excluded_users: Vec::new(),
        is_live: false,
        settings: Settings::default(),
    }
}

async fn memory() -> Store {
    Store::open(":memory:").await.unwrap()
}

/// Run one statement with no parameters on the actor thread.
async fn exec(store: &Store, sql: &'static str) {
    store
        .call(move |c| {
            c.execute(sql, [])?;
            Ok(())
        })
        .await
        .unwrap();
}

/// First column of the single row the statement returns.
async fn scalar<T: rusqlite::types::FromSql + Send + 'static>(
    store: &Store,
    sql: &'static str,
) -> T {
    store
        .call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

async fn set_grant(store: &Store, id: i64, status: &str) {
    let status = status.to_owned();
    store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute("INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(?1,0,?2) ON CONFLICT(twitch_id) DO UPDATE SET status=excluded.status", params![id, status])?;
            refresh_eligibility(&tx, id)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn grant(store: &Store, id: i64) {
    set_grant(store, id, "granted").await;
}

async fn disable(store: &Store, id: i64) {
    store
        .set_settings(
            id,
            Settings {
                enable: false,
                ..Settings::default()
            },
        )
        .await
        .unwrap();
}

fn snapshot(
    twitch_id: i64,
    expected_revision: i64,
    online: bool,
    category_id: &str,
    login: Option<&str>,
) -> StreamSnapshot {
    StreamSnapshot {
        twitch_id,
        expected_revision,
        online,
        category_id: category_id.into(),
        login: login.map(Into::into),
    }
}

/// Apply an event for the canonical "channel" login.
async fn event(store: &Store, id: i64, event: StreamEvent, at: i64) -> bool {
    store
        .apply_stream_event(id, "channel".into(), event, at)
        .await
        .unwrap()
}

async fn is_live(store: &Store, id: i64) -> bool {
    store.user_by_twitch(id).await.unwrap().unwrap().is_live
}

async fn legacy_ids(store: &Store, after: i64, limit: i64) -> Vec<i64> {
    store
        .legacy_channels(after, limit)
        .await
        .unwrap()
        .iter()
        .map(|u| u.twitch_id)
        .collect()
}

#[tokio::test]
async fn linking_preserves_preferences_and_enforces_identity() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    let settings = Settings {
        echo: false,
        cooldown: 8.0,
        ..Settings::default()
    };
    store.set_settings(2, settings.clone()).await.unwrap();
    let mut changed = user();
    changed.excluded_users.clear();
    changed.osu_username = "renamed".into();
    store.save_user(changed).await.unwrap();
    let saved = store.user_by_login("CHANNEL").await.unwrap().unwrap();
    assert_eq!(saved.settings, settings);
    assert_eq!(saved.excluded_users, vec!["viewer"]);
    let mut conflicting = user();
    conflicting.twitch_id = 3;
    assert!(store.save_user(conflicting).await.is_err());
}

#[tokio::test]
async fn recycled_login_does_not_conflict_with_signup_or_profile_refresh() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    let mut new_owner = user();
    (
        new_owner.osu_id,
        new_owner.twitch_id,
        new_owner.osu_username,
    ) = (3, 1, "new owner".into());
    store.save_user(new_owner.clone()).await.unwrap();
    grant(&store, 1).await;
    assert_eq!(
        store.snapshot_versions(vec![1]).await.unwrap(),
        vec![(1, 0)]
    );
    store
        .apply_stream_snapshot(vec![snapshot(1, 0, true, "21465", Some("CHANNEL"))])
        .await
        .unwrap();
    assert_eq!(
        store
            .user_by_login("channel")
            .await
            .unwrap()
            .unwrap()
            .twitch_id,
        1
    );
    new_owner.osu_username = "refreshed".into();
    store.save_user(new_owner).await.unwrap();
    assert_eq!(
        store.user_by_twitch(1).await.unwrap().unwrap().osu_username,
        "refreshed"
    );
    let former = store.user_by_twitch(2).await.unwrap().unwrap();
    assert_eq!(former.osu_id, 1);
    assert_eq!(former.twitch_username, "channel");
    assert!(!former.is_live);
    let mut conflicting = user();
    conflicting.osu_id = 4;
    assert!(store.save_user(conflicting).await.is_err());
}

#[tokio::test]
async fn deletion_cascades_session_and_exclusions() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    exec(&store, "INSERT INTO sessions VALUES('hash',2,999999)").await;
    store.delete_user(2).await.unwrap();
    let count: i64 = scalar(
        &store,
        "SELECT (SELECT COUNT(*) FROM sessions)+(SELECT COUNT(*) FROM exclusions)",
    )
    .await;
    assert_eq!(count, 0);
}

#[tokio::test]
async fn history_joins_raw_metadata_and_lists_missing_ids() {
    let store = memory().await;
    store
        .cache_put(
            json!({"id":11,"beatmapset_id":12,"version":"Hard","beatmapset":{"artist":"Artist"}}),
        )
        .await
        .unwrap();
    for id in [11, 11, 13] {
        let event = RequestEvent {
            requester_channel_name: "a".into(),
            requested_beatmap_id: id,
            requested_channel_name: "b".into(),
            mods: None,
            timestamp: 100,
        };
        store.record_request(event).await.unwrap();
    }
    let board = store.leaderboard(100, 100, 0).await.unwrap();
    assert_eq!(board[0]["_id"], 11);
    assert_eq!(board[0]["count"], 2);
    assert_eq!(board[0]["version"], "Hard");
    assert!(board[0].get("result").is_none());
    assert_eq!(store.missing_beatmap_ids(0, 100).await.unwrap(), vec![13]);
    assert!(store.leaderboard(101, 100, 0).await.unwrap().is_empty());
}

#[tokio::test]
async fn set_cache_remembers_only_actual_selection_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sets.sqlite");
    let store = Store::open(&path).await.unwrap();
    let first = json!({"id":1,"beatmapset_id":10,"version":"first"});
    let selected = json!({"id":2,"beatmapset_id":10,"version":"selected","beatmapset":{"id":10,"title":"set"}});
    store.cache_put(first.clone()).await.unwrap();
    store.cache_put(selected.clone()).await.unwrap();
    assert!(store.cache_get_set(10).await.unwrap().is_none());
    store.cache_put_set(10, selected.clone()).await.unwrap();
    store.cache_put(first.clone()).await.unwrap();
    assert_eq!(
        store.cache_get_set(10).await.unwrap(),
        Some(selected.clone())
    );
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(reopened.cache_get_set(10).await.unwrap(), Some(selected));
    reopened.cache_put_set(10, first.clone()).await.unwrap();
    assert_eq!(store.cache_get_set(10).await.unwrap(), Some(first));
}

#[tokio::test]
async fn set_cache_requires_both_fresh_rows_and_stores_selection_atomically() {
    let store = memory().await;
    let selected = json!({"id":2,"beatmapset_id":10,"version":"selected"});
    store.cache_put_set(10, selected.clone()).await.unwrap();
    exec(&store, "UPDATE mapset_cache SET fetched_at=0").await;
    assert!(store.cache_get_set(10).await.unwrap().is_none());
    assert_eq!(store.cache_get(2).await.unwrap(), Some(selected.clone()));
    store.cache_put_set(10, selected.clone()).await.unwrap();
    exec(&store, "UPDATE beatmaps SET fetched_at=0").await;
    assert!(store.cache_get_set(10).await.unwrap().is_none());
    store.cache_put(selected.clone()).await.unwrap();
    assert_eq!(
        store.cache_get_set(10).await.unwrap(),
        Some(selected.clone())
    );
    assert!(
        store
            .cache_put_set(20, json!({"id":2,"beatmapset_id":10,"version":"wrong set"}))
            .await
            .is_err()
    );
    assert!(store.cache_get_set(20).await.unwrap().is_none());
    assert_eq!(store.cache_get(2).await.unwrap(), Some(selected));
    assert!(
        store
            .cache_put_set(10, json!({"id":3,"beatmapset_id":0}))
            .await
            .is_err()
    );
    assert!(store.cache_get(3).await.unwrap().is_none());
}

#[tokio::test]
async fn old_cache_is_retained_for_history_not_served_as_fresh() {
    let store = memory().await;
    store
        .cache_put(json!({"id":11,"beatmapset_id":12}))
        .await
        .unwrap();
    exec(&store, "UPDATE beatmaps SET fetched_at=0").await;
    assert!(store.cache_get(11).await.unwrap().is_none());
    let count: i64 = scalar(&store, "SELECT COUNT(*) FROM beatmaps").await;
    assert_eq!(count, 1);
}

#[tokio::test]
async fn restart_marks_sending_uncertain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let store = Store::open(&path).await.unwrap();
    exec(&store, "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('m',1,'pm','sending',0)").await;
    let second = Store::open(&path).await.unwrap();
    let status: String = scalar(&second, "SELECT status FROM deliveries").await;
    assert_eq!(status, "uncertain");
}

#[tokio::test]
async fn actor_serializes_parallel_calls_off_runtime_threads() {
    let store = memory().await;
    let mut jobs = Vec::new();
    for _ in 0..256 {
        let store = store.clone();
        jobs.push(tokio::spawn(async move {
            store.call(|c| {
                assert_eq!(std::thread::current().name(), Some("ronnia-sqlite"));
                c.execute("INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,timestamp) VALUES('viewer',1,'channel',0)", [])?;
                Ok(())
            }).await
        }));
    }
    for job in jobs {
        job.await.unwrap().unwrap();
    }
    let count: i64 = scalar(&store, "SELECT COUNT(*) FROM statistics").await;
    assert_eq!(count, 256);
}

#[tokio::test]
async fn grants_and_independent_stream_families_drive_eligibility() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    assert_eq!(store.enabled_twitch_ids().await.unwrap(), vec![2]);
    assert!(store.granted_twitch_ids().await.unwrap().is_empty());
    assert!(store.live_channels().await.unwrap().is_empty());
    grant(&store, 2).await;
    assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![2]);

    let mut changes = store.channel_changes();
    let observed = *changes.borrow();
    assert!(event(&store, 2, StreamEvent::Online, 100).await);
    tokio::time::timeout(Duration::from_secs(1), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(*changes.borrow(), observed);
    assert!(
        !is_live(&store, 2).await,
        "an online notice alone is not osu eligibility"
    );

    assert!(event(&store, 2, StreamEvent::Category("21465".into()), 200).await);
    assert!(is_live(&store, 2).await);
    assert_eq!(
        store.live_channels().await.unwrap(),
        vec![(2, "channel".into())]
    );

    let stable_revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
    assert!(!event(&store, 2, StreamEvent::Category("21465".into()), 200).await);
    assert!(!event(&store, 2, StreamEvent::Category("1".into()), 150).await);
    assert_eq!(
        store.snapshot_versions(vec![2]).await.unwrap()[0].1,
        stable_revision
    );
    assert!(is_live(&store, 2).await);

    assert!(event(&store, 2, StreamEvent::Online, 300).await);
    assert!(
        !is_live(&store, 2).await,
        "online clears the stale category"
    );
    assert!(
        !event(&store, 2, StreamEvent::Offline, 250).await,
        "late offline cannot undo a newer online"
    );
    assert!(
        !event(&store, 2, StreamEvent::Category("1".into()), 250).await,
        "a category older than the online notice cannot reinstate it"
    );
    assert!(!is_live(&store, 2).await);
    assert!(event(&store, 2, StreamEvent::Category("21465".into()), 400).await);
    assert!(is_live(&store, 2).await);
    assert!(!event(&store, 2, StreamEvent::Category("1".into()), 350).await);
    assert!(is_live(&store, 2).await);

    store.revoke_twitch_channel(2).await.unwrap();
    let revoked = store
        .user_by_twitch(2)
        .await
        .unwrap()
        .expect("revocation never deletes the account");
    assert_eq!(revoked.osu_username, "osu");
    assert_eq!(revoked.excluded_users, vec!["viewer"]);
    assert!(!revoked.is_live);
    assert!(store.live_channels().await.unwrap().is_empty());
    grant(&store, 2).await;
    assert!(
        is_live(&store, 2).await,
        "regrant uses the known stream state"
    );

    disable(&store, 2).await;
    assert!(store.enabled_twitch_ids().await.unwrap().is_empty());
    assert!(store.granted_twitch_ids().await.unwrap().is_empty());
    assert!(!is_live(&store, 2).await);
    store.set_settings(2, Settings::default()).await.unwrap();
    assert!(is_live(&store, 2).await);
}

#[tokio::test]
async fn osu_lazer_counts_as_osu_but_other_categories_do_not() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    grant(&store, 2).await;
    assert!(event(&store, 2, StreamEvent::Online, 100).await);
    assert!(
        event(
            &store,
            2,
            StreamEvent::Category(OSU_CATEGORIES[1].into()),
            200,
        )
        .await
    );
    assert!(is_live(&store, 2).await, "Osu!Lazer is an osu! stream");
    assert_eq!(
        store.live_channels().await.unwrap(),
        vec![(2, "channel".into())]
    );

    // A discovery snapshot carrying Osu!Lazer keeps the channel eligible.
    let revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
    store
        .apply_stream_snapshot(vec![snapshot(
            2,
            revision,
            true,
            OSU_CATEGORIES[1],
            Some("channel"),
        )])
        .await
        .unwrap();
    assert!(is_live(&store, 2).await);

    // Any other category, such as Just Chatting, is not an osu! stream.
    assert!(event(&store, 2, StreamEvent::Category("509659".into()), 300).await);
    assert!(!is_live(&store, 2).await);
    assert!(store.live_channels().await.unwrap().is_empty());

    // The classic osu! category remains eligible.
    let revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
    store
        .apply_stream_snapshot(vec![snapshot(2, revision, true, OSU_CATEGORIES[0], None)])
        .await
        .unwrap();
    assert!(is_live(&store, 2).await);
}

#[tokio::test]
async fn stale_snapshots_cannot_overwrite_newer_events_or_unknown_rows() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    grant(&store, 2).await;
    assert_eq!(
        store.snapshot_versions(vec![2, 999]).await.unwrap(),
        vec![(2, 0)]
    );
    assert!(
        !is_live(&store, 2).await,
        "capturing versions never implies live"
    );
    assert!(event(&store, 2, StreamEvent::Online, 100).await);
    assert!(event(&store, 2, StreamEvent::Category("21465".into()), 200).await);
    assert!(is_live(&store, 2).await);

    store
        .apply_stream_snapshot(vec![snapshot(2, 0, false, "", Some("stale"))])
        .await
        .unwrap();
    let user_now = store.user_by_twitch(2).await.unwrap().unwrap();
    assert!(
        user_now.is_live,
        "a slow HTTP snapshot cannot undo newer events"
    );
    assert_eq!(user_now.twitch_username, "channel");

    store
        .apply_stream_snapshot(vec![snapshot(2, 2, false, "", Some("Renamed"))])
        .await
        .unwrap();
    let user_now = store.user_by_twitch(2).await.unwrap().unwrap();
    assert!(!user_now.is_live);
    assert_eq!(user_now.twitch_username, "renamed");
    assert!(store.live_channels().await.unwrap().is_empty());

    store
        .apply_stream_snapshot(vec![snapshot(2, 2, true, "21465", None)])
        .await
        .unwrap();
    assert!(
        !is_live(&store, 2).await,
        "replayed snapshots stay rejected"
    );
    assert!(!event(&store, 2, StreamEvent::Category("21465".into()), 50).await);
    assert!(
        !store
            .apply_stream_event(999, "ghost".into(), StreamEvent::Online, 100)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn version_one_database_upgrades_transactionally_without_losing_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .unwrap();
        connection.execute("INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json,raw_json) VALUES(10,'osu','avatar',20,'CHANNEL','twitch-avatar',1,?1,?2)", params![json!({"enable":true,"echo":false}).to_string(), json!({"legacy":true}).to_string()]).unwrap();
        connection
            .execute(
                "INSERT INTO exclusions(twitch_id,login) VALUES(20,'VIEWER')",
                [],
            )
            .unwrap();
        connection.execute("INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,timestamp) VALUES('viewer',30,'channel',1700000000)", []).unwrap();
        connection.execute("INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json) VALUES(30,31,1700000000,'{\"id\":30}')", []).unwrap();
        connection.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('m',10,'pm','sending',0)", []).unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
    }
    let store = Store::open(&path).await.unwrap();
    let version: i64 = store
        .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(version, 4);
    let user = store.user_by_twitch(20).await.unwrap().unwrap();
    assert!(!user.is_live, "legacy live activity is not consent");
    assert_eq!(user.osu_username, "osu");
    assert_eq!(user.twitch_avatar_url, "twitch-avatar");
    assert!(!user.settings.echo);
    assert_eq!(user.excluded_users, vec!["viewer"]);
    let raw: Value = store
        .call(|c| {
            Ok(serde_json::from_str(&c.query_row(
                "SELECT raw_json FROM users",
                [],
                |r| r.get::<_, String>(0),
            )?)?)
        })
        .await
        .unwrap();
    assert_eq!(raw["legacy"], true);
    let counts: (i64, i64, i64, i64, String) = store
        .call(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM statistics", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM beatmaps", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM twitch_channel_grants", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT COUNT(*) FROM channel_stream_state", [], |r| {
                    r.get(0)
                })?,
                c.query_row("SELECT status FROM deliveries", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (1, 1, 0, 0, "uncertain".into()));
    assert_eq!(store.enabled_twitch_ids().await.unwrap(), vec![20]);
    assert!(store.granted_twitch_ids().await.unwrap().is_empty());

    let future = directory.path().join("future.sqlite");
    {
        let connection = rusqlite::Connection::open(&future).unwrap();
        connection.pragma_update(None, "user_version", 5).unwrap();
    }
    assert!(Store::open(&future).await.is_err());
}

#[tokio::test]
async fn concurrent_snapshot_and_offline_event_converge_on_offline() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    grant(&store, 2).await;
    assert_eq!(
        store.snapshot_versions(vec![2]).await.unwrap(),
        vec![(2, 0)]
    );
    let snapshot =
        store.apply_stream_snapshot(vec![snapshot(2, 0, true, "21465", Some("CHANNEL"))]);
    let event = store.apply_stream_event(2, "channel".into(), StreamEvent::Offline, 100);
    let (snapshot, event) = tokio::join!(snapshot, event);
    snapshot.unwrap();
    event.unwrap();
    let online: bool = store
        .call(|c| {
            Ok(c.query_row(
                "SELECT online FROM channel_stream_state WHERE twitch_id=2",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        !online,
        "a captured snapshot cannot resurrect a channel after a newer offline event"
    );
    assert!(!is_live(&store, 2).await);
}

#[tokio::test]
async fn modern_singletons_are_enforced_and_grants_cascade_with_accounts() {
    let store = memory().await;
    store.save_user(user()).await.unwrap();
    let auth_count: i64 = store.call(|c| {
        c.execute("INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at) VALUES(1,'1','bot',x'00',0,'[]',0)", [])?;
        assert!(c.execute("INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at) VALUES(1,'2','other',x'00',0,'[]',0)", []).is_err());
        Ok(c.query_row("SELECT COUNT(*) FROM twitch_bot_auth", [], |r| r.get(0))?)
    }).await.unwrap();
    assert_eq!(auth_count, 1);
    store.call(|c| {
        c.execute("INSERT INTO twitch_conduit(singleton,client_id,conduit_id,updated_at) VALUES(1,'client','conduit',0)", [])?;
        assert!(c.execute("INSERT INTO twitch_conduit(singleton,client_id,conduit_id,updated_at) VALUES(1,'client','second',0)", []).is_err());
        Ok(())
    }).await.unwrap();
    grant(&store, 2).await;
    store
        .apply_stream_event(2, "channel".into(), StreamEvent::Online, 1)
        .await
        .unwrap();
    store.delete_user(2).await.unwrap();
    let grants: i64 = scalar(&store, "SELECT COUNT(*) FROM twitch_channel_grants").await;
    let states: i64 = scalar(&store, "SELECT COUNT(*) FROM channel_stream_state").await;
    assert_eq!((grants, states), (0, 0));
}

#[tokio::test]
async fn grant_activation_waits_for_active_at_but_public_consent_is_unchanged() {
    let store = memory().await;
    store.save_user(account(10, 20, "alpha")).await.unwrap();
    let now = chrono::Utc::now().timestamp();
    store.call(move |c| {
        c.execute("INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(20,0,?1,'granted')", [now + 3600])?;
        Ok(())
    }).await.unwrap();
    assert!(store.granted_twitch_ids().await.unwrap().is_empty());
    exec(&store, "UPDATE twitch_channel_grants SET active_at=0").await;
    assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![20]);
}

#[tokio::test]
async fn legacy_channels_exclude_disabled_and_any_grant_and_page_by_twitch_id() {
    let store = memory().await;
    for (osu, twitch, login) in [
        (10, 20, "alpha"),
        (11, 21, "beta"),
        (12, 22, "disabled"),
        (13, 23, "gamma"),
    ] {
        store.save_user(account(osu, twitch, login)).await.unwrap();
    }
    disable(&store, 22).await;
    // Revoked grants are still proof of an attempted migration.
    set_grant(&store, 20, "revoked").await;
    assert_eq!(legacy_ids(&store, 0, 100).await, vec![21, 23]);
    assert_eq!(legacy_ids(&store, 20, 1).await, vec![21]);
    assert_eq!(legacy_ids(&store, 21, 100).await, vec![23]);
    assert!(store.legacy_channels(0, 0).await.is_err());
    assert_eq!(
        store
            .legacy_user_by_login("BETA")
            .await
            .unwrap()
            .unwrap()
            .osu_id,
        11
    );
    assert!(store.legacy_user_by_login("alpha").await.unwrap().is_none());
    assert!(store.legacy_user_by_twitch(20).await.unwrap().is_none());
    assert_eq!(
        store
            .legacy_user_by_twitch(23)
            .await
            .unwrap()
            .unwrap()
            .twitch_username,
        "gamma"
    );
}

#[tokio::test]
async fn legacy_claims_reject_duplicates_mismatches_and_migrated_channels() {
    let store = memory().await;
    store.save_user(account(10, 20, "alpha")).await.unwrap();
    assert_eq!(
        store
            .claim_legacy_delivery("m1".into(), 20, 999, "!request 1".into())
            .await
            .unwrap(),
        ClaimOutcome::Rejected("target does not match channel")
    );
    assert_eq!(
        store
            .claim_legacy_delivery("m1".into(), 20, 10, "!request 1".into())
            .await
            .unwrap(),
        ClaimOutcome::Claimed
    );
    assert_eq!(
        store
            .claim_legacy_delivery("m1".into(), 20, 10, "!request 1".into())
            .await
            .unwrap(),
        ClaimOutcome::Rejected("delivery already claimed")
    );
    exec(&store, "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-message',10,'pm','pending',0,'new')").await;
    assert_eq!(
        store
            .claim_legacy_delivery("new-message".into(), 20, 10, "pm".into())
            .await
            .unwrap(),
        ClaimOutcome::Rejected("delivery already claimed")
    );
    grant(&store, 20).await;
    assert_eq!(
        store
            .claim_legacy_delivery("m2".into(), 20, 10, "!request 2".into())
            .await
            .unwrap(),
        ClaimOutcome::Rejected("channel migrated")
    );
    disable(&store, 20).await;
    assert_eq!(
        store
            .claim_legacy_delivery("m3".into(), 20, 10, "!request 3".into())
            .await
            .unwrap(),
        ClaimOutcome::Rejected("channel disabled")
    );
    let pending: i64 = scalar(
        &store,
        "SELECT COUNT(*) FROM deliveries WHERE source='legacy'",
    )
    .await;
    assert_eq!(pending, 1);
}

#[tokio::test]
async fn legacy_delivery_reports_are_idempotent_for_statistics() {
    let store = memory().await;
    store.save_user(account(10, 20, "alpha")).await.unwrap();
    store
        .claim_legacy_delivery("m1".into(), 20, 10, "!request 1".into())
        .await
        .unwrap();
    let statistics = RequestEvent {
        requester_channel_name: "viewer".into(),
        requested_beatmap_id: 42,
        requested_channel_name: "alpha".into(),
        mods: Some("HD".into()),
        timestamp: 1_700_000_000,
    };
    assert_eq!(
        store
            .complete_legacy_delivery("m1".into(), true, None, Some(statistics.clone()))
            .await
            .unwrap(),
        DeliveryOutcome::Recorded
    );
    let mut changed = statistics.clone();
    changed.requester_channel_name = "replay".into();
    assert_eq!(
        store
            .complete_legacy_delivery("m1".into(), true, None, Some(changed))
            .await
            .unwrap(),
        DeliveryOutcome::Recorded
    );
    let status: String = scalar(
        &store,
        "SELECT status FROM deliveries WHERE twitch_message_id='m1'",
    )
    .await;
    let source: String = scalar(
        &store,
        "SELECT source_id FROM statistics WHERE source_id='twitch:m1'",
    )
    .await;
    let rows: i64 = scalar(&store, "SELECT COUNT(*) FROM statistics").await;
    let requester: String = scalar(
        &store,
        "SELECT requester_channel_name FROM statistics WHERE source_id='twitch:m1'",
    )
    .await;
    assert_eq!(
        (status.as_str(), source.as_str(), rows, requester.as_str()),
        ("sent", "twitch:m1", 1, "viewer")
    );

    store
        .claim_legacy_delivery("m2".into(), 20, 10, "!request 2".into())
        .await
        .unwrap();
    assert_eq!(
        store
            .complete_legacy_delivery("m2".into(), false, Some("osu PM failed".into()), None)
            .await
            .unwrap(),
        DeliveryOutcome::Recorded
    );
    let status: String = scalar(
        &store,
        "SELECT status FROM deliveries WHERE twitch_message_id='m2'",
    )
    .await;
    let error: Option<String> = store
        .call(|c| {
            Ok(c.query_row(
                "SELECT error FROM deliveries WHERE twitch_message_id='m2'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        (status.as_str(), error.as_deref()),
        ("failed", Some("osu PM failed"))
    );
    assert_eq!(
        store
            .complete_legacy_delivery("missing".into(), false, None, None)
            .await
            .unwrap(),
        DeliveryOutcome::NotFound
    );
    exec(&store, "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-message',10,'pm','pending',0,'new')").await;
    assert_eq!(
        store
            .complete_legacy_delivery("new-message".into(), false, None, None)
            .await
            .unwrap(),
        DeliveryOutcome::NotFound
    );
}

#[tokio::test]
async fn legacy_live_reports_refresh_names_and_public_union_paginates() {
    let store = memory().await;
    for (osu, twitch, login) in [(10, 20, "alpha"), (11, 21, "beta"), (12, 22, "new-live")] {
        store.save_user(account(osu, twitch, login)).await.unwrap();
    }
    exec(&store, "UPDATE users SET is_live=1 WHERE twitch_id=22").await;
    assert_eq!(
        store
            .replace_legacy_live(vec![
                (20, "Alpha_New".into()),
                (21, "BETA".into()),
                (999, "ghost".into())
            ])
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .user_by_twitch(20)
            .await
            .unwrap()
            .unwrap()
            .twitch_username,
        "alpha_new"
    );
    assert_eq!(
        store.live_users(100, 0).await.unwrap(),
        vec!["alpha_new", "beta", "new-live"]
    );
    assert_eq!(
        store.live_channels().await.unwrap(),
        vec![(22, "new-live".into())]
    );
    assert_eq!(
        store.live_users(2, 1).await.unwrap(),
        vec!["beta", "new-live"]
    );
    exec(
        &store,
        "UPDATE legacy_live SET updated_at=unixepoch()-181 WHERE twitch_id=20",
    )
    .await;
    assert_eq!(
        store.live_users(100, 0).await.unwrap(),
        vec!["beta", "new-live"]
    );
}

#[tokio::test]
async fn legacy_live_reports_skip_ineligible_accounts_and_remove_rows() {
    let store = memory().await;
    for (osu, twitch, login) in [(10, 20, "alpha"), (11, 21, "beta"), (12, 22, "gamma")] {
        store.save_user(account(osu, twitch, login)).await.unwrap();
    }
    assert_eq!(
        store
            .replace_legacy_live(vec![(20, "alpha".into()), (21, "beta".into())])
            .await
            .unwrap(),
        2
    );
    disable(&store, 20).await;
    grant(&store, 21).await;
    assert_eq!(
        store
            .replace_legacy_live(vec![
                (20, "alpha".into()),
                (21, "beta".into()),
                (22, "gamma".into()),
                (23, "missing".into())
            ])
            .await
            .unwrap(),
        1
    );
    let logins: Vec<String> = store
        .call(|c| {
            let logins: Vec<String> = c
                .prepare("SELECT login FROM legacy_live ORDER BY twitch_id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(logins)
        })
        .await
        .unwrap();
    assert_eq!(logins, vec!["gamma"]);
    assert_eq!(
        store.live_users(100, 0).await.unwrap(),
        vec!["gamma"],
        "disabled and granted accounts never enter the public union"
    );
    assert!(
        store
            .replace_legacy_live(vec![(22, "gamma".into()), (22, "again".into())])
            .await
            .is_err()
    );
    assert!(
        store
            .replace_legacy_live(vec![(22, "  ".into())])
            .await
            .is_err()
    );
    store.delete_user(22).await.unwrap();
    assert_eq!(
        store.live_users(100, 0).await.unwrap(),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn migration_three_preserves_existing_grants_and_deliveries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("version-two.sqlite");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../../migrations/002_modern_twitch.sql"))
            .unwrap();
        connection.execute("INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(10,'osu',20,'alpha',?1)", [json!({"enable":true}).to_string()]).unwrap();
        connection.execute("INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(20,0,'granted')", []).unwrap();
        connection.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('old',10,'pm','pending',0)", []).unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
    }
    let store = Store::open(&path).await.unwrap();
    let version: i64 = store
        .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(version, 4);
    let active_at: i64 = scalar(
        &store,
        "SELECT active_at FROM twitch_channel_grants WHERE twitch_id=20",
    )
    .await;
    let source: String = scalar(
        &store,
        "SELECT source FROM deliveries WHERE twitch_message_id='old'",
    )
    .await;
    let status: String = scalar(
        &store,
        "SELECT status FROM deliveries WHERE twitch_message_id='old'",
    )
    .await;
    assert_eq!(
        (active_at, source.as_str(), status.as_str()),
        (0, "new", "pending")
    );
    assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![20]);
    let reopened = Store::open(&path).await.unwrap();
    let version: i64 = reopened
        .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(version, 4);
}

#[tokio::test]
async fn migration_four_adds_legacy_live_without_losing_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("version-three.sqlite");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(include_str!("../../migrations/001_initial.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../../migrations/002_modern_twitch.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../../migrations/003_coexistence.sql"))
            .unwrap();
        connection.execute("INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(10,'osu',20,'alpha',?1)", [json!({"enable":true}).to_string()]).unwrap();
        connection.pragma_update(None, "user_version", 3).unwrap();
    }
    let store = Store::open(&path).await.unwrap();
    let version: i64 = store
        .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap();
    let legacy_count: i64 = scalar(&store, "SELECT COUNT(*) FROM legacy_live").await;
    let user_count: i64 = scalar(&store, "SELECT COUNT(*) FROM users").await;
    assert_eq!((version, legacy_count, user_count), (4, 0, 1));
    assert_eq!(
        store
            .replace_legacy_live(vec![(20, "alpha".into())])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store.live_users(100, 0).await.unwrap(),
        vec!["alpha"],
        "fresh legacy presence is public after migration"
    );
    let reopened = Store::open(&path).await.unwrap();
    let version: i64 = reopened
        .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(reopened.live_users(100, 0).await.unwrap(), vec!["alpha"]);
}
