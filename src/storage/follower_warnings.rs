//! Durable, at-most-once follower-mode warnings, keyed by Twitch's stream identity.
use anyhow::{Result, ensure};
use rusqlite::{OptionalExtension, params};

use super::Store;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FollowerWarning {
    pub osu_id: i64,
    pub stream_id: String,
}

impl Store {
    /// Claim before sending externally. Every outcome, including an interrupted send,
    /// consumes the warning for this stream; only a different stream may claim again.
    pub async fn claim_follower_warning(&self, twitch_id: i64) -> Result<Option<FollowerWarning>> {
        self.write(move |tx| {
            let warning = tx
                .query_row(
                    "SELECT u.osu_id,s.stream_id FROM users u JOIN twitch_channel_grants g ON g.twitch_id=u.twitch_id JOIN channel_stream_state s ON s.twitch_id=u.twitch_id WHERE u.twitch_id=?1 AND json_extract(u.settings_json,'$.enable')=1 AND g.status='granted' AND g.active_at<=unixepoch() AND s.online=1 AND length(trim(s.stream_id))>0 AND NOT EXISTS(SELECT 1 FROM follower_warnings w WHERE w.twitch_id=u.twitch_id AND w.stream_id=s.stream_id)",
                    [twitch_id],
                    |row| {
                        Ok(FollowerWarning {
                            osu_id: row.get(0)?,
                            stream_id: row.get(1)?,
                        })
                    },
                )
                .optional()?;
            let Some(warning) = warning else {
                return Ok(None);
            };
            let claimed = tx.execute(
                "INSERT INTO follower_warnings(twitch_id,stream_id,status,error,attempted_at) VALUES(?1,?2,'sending',NULL,unixepoch()) ON CONFLICT(twitch_id) DO UPDATE SET stream_id=excluded.stream_id,status='sending',error=NULL,attempted_at=excluded.attempted_at WHERE follower_warnings.stream_id<>excluded.stream_id",
                params![twitch_id, warning.stream_id],
            )?;
            Ok((claimed == 1).then_some(warning))
        })
        .await
    }

    /// A late completion cannot overwrite the receipt for a newer stream or an
    /// already-terminal receipt. Startup recovery likewise remains terminal.
    pub async fn finish_follower_warning(
        &self,
        twitch_id: i64,
        stream_id: String,
        status: &'static str,
        error: Option<String>,
    ) -> Result<()> {
        ensure!(
            matches!(status, "sent" | "failed" | "uncertain"),
            "follower warning completion status must be terminal"
        );
        self.write(move |tx| {
            tx.execute(
                "UPDATE follower_warnings SET status=?3,error=?4 WHERE twitch_id=?1 AND stream_id=?2 AND status='sending'",
                params![twitch_id, stream_id, status, error],
            )?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Settings, User};

    fn account() -> User {
        User {
            osu_id: 1,
            osu_username: "player".into(),
            osu_avatar_url: String::new(),
            twitch_id: 2,
            twitch_username: "channel".into(),
            twitch_avatar_url: String::new(),
            excluded_users: Vec::new(),
            is_live: false,
            settings: Settings::default(),
        }
    }

    async fn exec(store: &Store, sql: &'static str) {
        store
            .write(move |tx| {
                tx.execute(sql, [])?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn stream(store: &Store, online: bool, stream_id: Option<&str>) {
        let stream_id = stream_id.map(str::to_owned);
        store
            .write(move |tx| {
                tx.execute(
                    "INSERT INTO channel_stream_state(twitch_id,online,category_id,stream_id) VALUES(2,?1,'21465',?2) ON CONFLICT(twitch_id) DO UPDATE SET online=excluded.online,stream_id=excluded.stream_id",
                    params![online, stream_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn eligible(store: &Store) {
        store.save_user(account()).await.unwrap();
        exec(
            store,
            "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(2,0,'granted')",
        )
        .await;
        stream(store, true, Some("stream-a")).await;
    }

    async fn memory() -> Store {
        let store = Store::open(":memory:").await.unwrap();
        eligible(&store).await;
        store
    }

    async fn receipt(store: &Store) -> (String, String, Option<String>, i64) {
        store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT stream_id,status,error,attempted_at FROM follower_warnings WHERE twitch_id=2",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn concurrent_claims_have_one_winner() {
        let store = memory().await;
        let other = store.clone();
        let (first, second) = tokio::join!(
            store.claim_follower_warning(2),
            other.claim_follower_warning(2)
        );
        let claims = [first.unwrap(), second.unwrap()];
        assert_eq!(claims.iter().flatten().count(), 1);
        assert_eq!(
            claims.into_iter().flatten().next().unwrap(),
            FollowerWarning {
                osu_id: 1,
                stream_id: "stream-a".into(),
            }
        );
        let (_, status, error, attempted_at) = receipt(&store).await;
        assert_eq!(status, "sending");
        assert_eq!(error, None);
        assert!(attempted_at > 0);
    }

    #[tokio::test]
    async fn every_receipt_outcome_survives_reopen_without_replay() {
        for status in ["sending", "sent", "failed", "uncertain"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("warnings.sqlite");
            let store = Store::open(&path).await.unwrap();
            eligible(&store).await;
            assert!(store.claim_follower_warning(2).await.unwrap().is_some());
            if status != "sending" {
                store
                    .finish_follower_warning(2, "stream-a".into(), status, None)
                    .await
                    .unwrap();
            }
            let attempted_at = receipt(&store).await.3;
            drop(store);

            let reopened = Store::open(&path).await.unwrap();
            assert!(reopened.claim_follower_warning(2).await.unwrap().is_none());
            let (stream_id, recovered_status, error, recovered_attempted_at) =
                receipt(&reopened).await;
            assert_eq!(stream_id, "stream-a");
            assert_eq!(recovered_attempted_at, attempted_at);
            assert_eq!(
                recovered_status,
                if status == "sending" {
                    "uncertain"
                } else {
                    status
                }
            );
            if status == "sending" {
                assert!(error.unwrap().contains("not replayed"));
                reopened
                    .finish_follower_warning(2, "stream-a".into(), "sent", None)
                    .await
                    .unwrap();
                assert_eq!(receipt(&reopened).await.1, "uncertain");
            }
        }
    }

    #[tokio::test]
    async fn only_a_different_stream_identity_allows_another_warning() {
        let store = memory().await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_some());
        stream(&store, false, Some("stream-a")).await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        stream(&store, true, Some("stream-a")).await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        stream(&store, true, Some("stream-b")).await;
        let warning = store.claim_follower_warning(2).await.unwrap().unwrap();
        assert_eq!(warning.stream_id, "stream-b");
        let count: i64 = store
            .call(|c| Ok(c.query_row("SELECT count(*) FROM follower_warnings", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn claims_require_current_account_consent_and_known_online_stream() {
        let store = memory().await;
        assert!(store.claim_follower_warning(999).await.unwrap().is_none());
        store
            .set_settings(
                2,
                Settings {
                    enable: false,
                    ..Settings::default()
                },
            )
            .await
            .unwrap();
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        store.set_settings(2, Settings::default()).await.unwrap();

        exec(
            &store,
            "DELETE FROM twitch_channel_grants WHERE twitch_id=2",
        )
        .await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        exec(
            &store,
            "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(2,0,'revoked')",
        )
        .await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        exec(&store, "UPDATE twitch_channel_grants SET status='granted',active_at=9223372036854775807 WHERE twitch_id=2").await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        exec(
            &store,
            "UPDATE twitch_channel_grants SET active_at=0 WHERE twitch_id=2",
        )
        .await;

        exec(&store, "DELETE FROM channel_stream_state WHERE twitch_id=2").await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        stream(&store, false, Some("stream-a")).await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        for stream_id in [None, Some(""), Some("   ")] {
            stream(&store, true, stream_id).await;
            assert!(store.claim_follower_warning(2).await.unwrap().is_none());
        }
        stream(&store, true, Some("stream-a")).await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn late_or_repeated_finish_cannot_overwrite_a_receipt() {
        let store = memory().await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_some());
        stream(&store, true, Some("stream-b")).await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_some());
        store
            .finish_follower_warning(2, "stream-a".into(), "failed", Some("old failure".into()))
            .await
            .unwrap();
        let (stream_id, status, error, _) = receipt(&store).await;
        assert_eq!(stream_id, "stream-b");
        assert_eq!(status, "sending");
        assert_eq!(error, None);
        store
            .finish_follower_warning(2, "stream-b".into(), "sent", None)
            .await
            .unwrap();
        store
            .finish_follower_warning(2, "stream-b".into(), "uncertain", Some("late".into()))
            .await
            .unwrap();
        let (_, status, error, _) = receipt(&store).await;
        assert_eq!(status, "sent");
        assert_eq!(error, None);
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn account_deletion_cascades_warning_receipts() {
        let store = memory().await;
        assert!(store.claim_follower_warning(2).await.unwrap().is_some());
        store.delete_user(2).await.unwrap();
        let count: i64 = store
            .call(|c| Ok(c.query_row("SELECT count(*) FROM follower_warnings", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert!(store.claim_follower_warning(2).await.unwrap().is_none());
    }
}
