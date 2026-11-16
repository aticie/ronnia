//! Read-only business snapshots for the periodic metrics exporter. Cohorts use
//! preserved import documents; request totals use confirmed deliveries, not the
//! optional beatmap statistics recorded alongside them.
use anyhow::Result;

use super::Store;

#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct BusinessMetrics {
    pub(crate) accounts_legacy: i64,
    pub(crate) accounts_new: i64,
    pub(crate) accounts_enabled: i64,
    pub(crate) migration_pending: i64,
    pub(crate) migration_authorized: i64,
    pub(crate) migration_revoked: i64,
    pub(crate) live_rust: i64,
    pub(crate) live_legacy: i64,
    pub(crate) online_rust: i64,
    pub(crate) requests_rust: i64,
    pub(crate) requests_legacy: i64,
    pub(crate) active_channels_24h: i64,
    pub(crate) active_channels_7d: i64,
    pub(crate) authorization_pending: i64,
    pub(crate) authorization_authorized: i64,
    pub(crate) authorization_revoked: i64,
    pub(crate) chat_banned: i64,
}

impl Store {
    /// One SQLite statement keeps every aggregate on the same read snapshot,
    /// with a single clock value and explicit zero counts for an empty database.
    pub(crate) async fn business_metrics(&self) -> Result<BusinessMetrics> {
        self.call(|c| {
            Ok(c.query_row(
                r#"
                WITH clock AS MATERIALIZED (SELECT unixepoch() AS now),
                accounts AS (
                    SELECT
                        COUNT(*) FILTER (WHERE u.raw_json IS NOT NULL) AS legacy,
                        COUNT(*) FILTER (WHERE u.raw_json IS NULL) AS new,
                        COUNT(*) FILTER (WHERE json_extract(u.settings_json,'$.enable')=1) AS enabled,
                        COUNT(*) FILTER (WHERE u.raw_json IS NOT NULL AND g.twitch_id IS NULL) AS migration_pending,
                        COUNT(*) FILTER (WHERE u.raw_json IS NOT NULL AND g.status='granted') AS migration_authorized,
                        COUNT(*) FILTER (WHERE u.raw_json IS NOT NULL AND g.status='revoked') AS migration_revoked,
                        COUNT(*) FILTER (WHERE u.is_live=1) AS live,
                        COUNT(*) FILTER (WHERE s.online=1 AND json_extract(u.settings_json,'$.enable')=1
                            AND g.status='granted' AND g.active_at<=clock.now) AS online,
                        COUNT(*) FILTER (WHERE g.twitch_id IS NULL) AS authorization_pending,
                        COUNT(*) FILTER (WHERE g.status='granted') AS authorization_authorized,
                        COUNT(*) FILTER (WHERE g.status='revoked') AS authorization_revoked,
                        COUNT(*) FILTER (WHERE g.status='granted' AND g.chat_banned_at IS NOT NULL) AS chat_banned
                    FROM users u
                    LEFT JOIN twitch_channel_grants g ON g.twitch_id=u.twitch_id
                    LEFT JOIN channel_stream_state s ON s.twitch_id=u.twitch_id
                    CROSS JOIN clock
                ),
                legacy AS (
                    SELECT COUNT(*) AS live
                    FROM legacy_live l JOIN users u ON u.twitch_id=l.twitch_id
                    CROSS JOIN clock
                    WHERE l.updated_at>=clock.now-180
                        AND json_extract(u.settings_json,'$.enable')=1
                        AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=l.twitch_id)
                ),
                requests AS (
                    SELECT
                        COUNT(*) FILTER (WHERE d.source='new') AS rust,
                        COUNT(*) FILTER (WHERE d.source='legacy') AS legacy,
                        COUNT(DISTINCT CASE WHEN d.created_at BETWEEN clock.now-86400 AND clock.now
                            THEN d.target_osu_id END) AS active_24h,
                        COUNT(DISTINCT CASE WHEN d.created_at BETWEEN clock.now-604800 AND clock.now
                            THEN d.target_osu_id END) AS active_7d
                    FROM deliveries d CROSS JOIN clock
                    WHERE d.status='sent'
                )
                SELECT a.legacy,a.new,a.enabled,
                    a.migration_pending,a.migration_authorized,a.migration_revoked,
                    a.live,l.live,a.online,r.rust,r.legacy,r.active_24h,r.active_7d,
                    a.authorization_pending,a.authorization_authorized,a.authorization_revoked,a.chat_banned
                FROM accounts a CROSS JOIN legacy l CROSS JOIN requests r
                "#,
                [],
                |row| {
                    Ok(BusinessMetrics {
                        accounts_legacy: row.get(0)?,
                        accounts_new: row.get(1)?,
                        accounts_enabled: row.get(2)?,
                        migration_pending: row.get(3)?,
                        migration_authorized: row.get(4)?,
                        migration_revoked: row.get(5)?,
                        live_rust: row.get(6)?,
                        live_legacy: row.get(7)?,
                        online_rust: row.get(8)?,
                        requests_rust: row.get(9)?,
                        requests_legacy: row.get(10)?,
                        active_channels_24h: row.get(11)?,
                        active_channels_7d: row.get(12)?,
                        authorization_pending: row.get(13)?,
                        authorization_authorized: row.get(14)?,
                        authorization_revoked: row.get(15)?,
                        chat_banned: row.get(16)?,
                    })
                },
            )?)
        })
        .await
    }

    /// Immutable historical baseline, loaded once by the exporter rather than
    /// rescanning imported statistics with every periodic snapshot.
    pub(crate) async fn imported_request_total(&self) -> Result<i64> {
        self.call(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*) FROM statistics WHERE raw_json IS NOT NULL",
                [],
                |row| row.get(0),
            )?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::{BusinessMetrics, Store};
    use crate::{
        domain::{RequestEvent, Settings, User},
        storage::refresh_eligibility,
    };
    use rusqlite::params;

    async fn account(store: &Store, id: i64, imported: bool) {
        store
            .save_user(User {
                osu_id: id,
                osu_username: format!("player{id}"),
                osu_avatar_url: String::new(),
                twitch_id: id,
                twitch_username: format!("channel{id}"),
                twitch_avatar_url: String::new(),
                excluded_users: Vec::new(),
                is_live: false,
                settings: Settings::default(),
            })
            .await
            .unwrap();
        if imported {
            store
                .call(move |c| {
                    c.execute("UPDATE users SET raw_json='{}' WHERE twitch_id=?1", [id])?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    }

    async fn grant(store: &Store, id: i64, status: &'static str) {
        store
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(?1,0,?2)",
                    params![id, status],
                )?;
                refresh_eligibility(&tx, id)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn empty_snapshot_emits_every_count_as_zero() {
        let store = Store::open(":memory:").await.unwrap();
        assert_eq!(
            store.business_metrics().await.unwrap(),
            BusinessMetrics::default()
        );
        assert_eq!(store.imported_request_total().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn migration_states_only_include_retained_imported_accounts() {
        let store = Store::open(":memory:").await.unwrap();
        for id in 1..=6 {
            account(&store, id, id <= 3).await;
        }
        grant(&store, 2, "granted").await;
        grant(&store, 3, "revoked").await;
        grant(&store, 5, "granted").await;
        grant(&store, 6, "revoked").await;
        store
            .call(|c| {
                c.execute("INSERT INTO signup_attempts(token_hash,provider,identity_json,expires_at) VALUES('pending','twitch','{}',unixepoch()+300)", [])?;
                c.execute("INSERT INTO oauth_attempts(state_hash,provider,return_to,expires_at) VALUES('pending','twitch','/',unixepoch()+300)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store.business_metrics().await.unwrap(),
            BusinessMetrics {
                accounts_legacy: 3,
                accounts_new: 3,
                accounts_enabled: 6,
                migration_pending: 1,
                migration_authorized: 1,
                migration_revoked: 1,
                authorization_pending: 2,
                authorization_authorized: 2,
                authorization_revoked: 2,
                ..BusinessMetrics::default()
            }
        );
        store.delete_user(3).await.unwrap();
        let metrics = store.business_metrics().await.unwrap();
        assert_eq!(metrics.accounts_legacy, 2);
        assert_eq!(metrics.accounts_new, 3);
        assert_eq!(metrics.migration_revoked, 0);
        assert_eq!(metrics.authorization_revoked, 1);
    }

    #[tokio::test]
    async fn live_and_online_counts_follow_freshness_membership_and_settings() {
        let store = Store::open(":memory:").await.unwrap();
        for id in 1..=11 {
            account(&store, id, id <= 5).await;
        }
        for id in [5, 6, 7, 10, 11] {
            grant(&store, id, "granted").await;
        }
        for id in [4, 8] {
            grant(&store, id, "revoked").await;
        }
        for id in [3, 7] {
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
        store
            .call(|c| {
                let tx = c.transaction()?;
                for id in 1..=5 {
                    tx.execute("INSERT INTO legacy_live(twitch_id,login,updated_at) VALUES(?1,?2,unixepoch()-?3)", params![id, format!("channel{id}"), if id == 2 { 3600 } else { 0 }])?;
                }
                // Preserve stale online rows for disabled, revoked and ungranted
                // accounts, plus a consent grant still in its grace window.
                for id in 5..=11 {
                    tx.execute("INSERT INTO channel_stream_state(twitch_id,online,category_id) VALUES(?1,?2,?3)", params![id, id != 11, if [6, 10].contains(&id) { "509658" } else { "21465" }])?;
                    refresh_eligibility(&tx, id)?;
                }
                tx.execute("UPDATE twitch_channel_grants SET active_at=unixepoch()+3600 WHERE twitch_id=10", [])?;
                tx.execute("UPDATE twitch_channel_grants SET chat_banned_at=unixepoch() WHERE twitch_id IN (4,5,7)", [])?;
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        let metrics = store.business_metrics().await.unwrap();
        assert_eq!(metrics.accounts_enabled, 9);
        assert_eq!(metrics.live_legacy, 1);
        assert_eq!(metrics.live_rust, 1);
        assert_eq!(metrics.online_rust, 2);
        assert_eq!(metrics.chat_banned, 2);

        store
            .set_settings(
                5,
                Settings {
                    enable: false,
                    ..Settings::default()
                },
            )
            .await
            .unwrap();
        let disabled = store.business_metrics().await.unwrap();
        assert_eq!(disabled.accounts_enabled, 8);
        assert_eq!(disabled.live_rust, 0);
        assert_eq!(disabled.online_rust, 1);
        assert_eq!(disabled.chat_banned, 2);
        store.set_settings(5, Settings::default()).await.unwrap();
        store.revoke_twitch_channel(6).await.unwrap();
        store.clear_twitch_chat_ban(5).await.unwrap();
        let restored = store.business_metrics().await.unwrap();
        assert_eq!(restored.accounts_enabled, 9);
        assert_eq!(restored.live_rust, 1);
        assert_eq!(restored.online_rust, 1);
        assert_eq!(restored.chat_banned, 1);
    }

    #[tokio::test]
    async fn only_confirmed_deliveries_count_requests_and_distinct_active_targets() {
        let store = Store::open(":memory:").await.unwrap();
        store
            .call(|c| {
                let tx = c.transaction()?;
                let deliveries = [
                    ("new", "sent", 1, 60),
                    ("new", "sent", 1, 120),
                    ("legacy", "sent", 1, 180),
                    ("legacy", "sent", 2, 2 * 86400),
                    ("new", "sent", 3, 8 * 86400),
                    ("new", "failed", 4, 60),
                    ("legacy", "uncertain", 5, 60),
                    ("new", "pending", 6, 60),
                    ("legacy", "sending", 7, 60),
                    ("legacy", "sent", 8, 8 * 86400),
                    ("new", "sent", 9, 23 * 3600),
                    ("legacy", "sent", 10, 25 * 3600),
                ];
                for (index, (source, status, target, age)) in deliveries.into_iter().enumerate() {
                    tx.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES(?1,?2,'request',?3,unixepoch()-?4,?5)", params![format!("delivery{index}"), target, status, age, source])?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        // No statistics rows exist: successful mapset-link fallbacks must still
        // count, and the same target used by both bots is one active channel.
        let metrics = store.business_metrics().await.unwrap();
        assert_eq!(metrics.requests_rust, 4);
        assert_eq!(metrics.requests_legacy, 4);
        assert_eq!(metrics.active_channels_24h, 2);
        assert_eq!(metrics.active_channels_7d, 4);
        assert_eq!(metrics.accounts_new, 0);
    }

    #[tokio::test]
    async fn imported_baseline_excludes_runtime_statistics_and_deliveries() {
        let store = Store::open(":memory:").await.unwrap();
        store
            .call(|c| {
                c.execute("INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,timestamp,raw_json) VALUES('oldviewer',1,'oldchannel',0,'{}'),('oldviewer',2,'oldchannel',0,'{}')", [])?;
                c.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('runtime',1,'request','sent',unixepoch(),'legacy')", [])?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .record_request(RequestEvent {
                requester_channel_name: "viewer".into(),
                requested_beatmap_id: 3,
                requested_channel_name: "channel".into(),
                mods: None,
                timestamp: 1,
            })
            .await
            .unwrap();
        assert_eq!(store.imported_request_total().await.unwrap(), 2);
        let metrics = store.business_metrics().await.unwrap();
        assert_eq!(metrics.requests_rust, 0);
        assert_eq!(metrics.requests_legacy, 1);
    }
}
