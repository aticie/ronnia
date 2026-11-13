use crate::domain::{RequestEvent, Settings, StreamEvent, StreamSnapshot, User};
use anyhow::{Context, Result, anyhow, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimOutcome {
    Claimed,
    Rejected(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Recorded,
    NotFound,
}

/// A bounded mailbox to a dedicated SQLite thread. No SQLite work runs on Tokio workers.
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    channel_revision: watch::Sender<u64>,
}

impl Store {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (sender, mut receiver) = mpsc::channel::<Job>(128);
        let (ready_tx, ready_rx) = oneshot::channel();
        std::thread::Builder::new().name("ronnia-sqlite".into()).spawn(move || {
            let initialized = (|| -> Result<Connection> {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)?;
                }
                let mut connection = Connection::open(&path).context("opening SQLite database")?;
                connection.busy_timeout(Duration::from_secs(5))?;
                connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA temp_store=FILE;")?;
                initialize(&mut connection)?;
                Ok(connection)
            })();
            match initialized {
                Ok(mut connection) => {
                    if ready_tx.send(Ok(())).is_err() { return; }
                    while let Some(job) = receiver.blocking_recv() { job(&mut connection); }
                }
                Err(error) => { let _ = ready_tx.send(Err(error)); }
            }
        }).context("starting SQLite actor")?;
        ready_rx
            .await
            .context("SQLite actor stopped during startup")??;
        let (channel_revision, _) = watch::channel(0);
        Ok(Self {
            sender,
            channel_revision,
        })
    }

    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(Box::new(move |connection| {
                let _ = tx.send(f(connection));
            }))
            .await
            .map_err(|_| anyhow!("SQLite actor is unavailable"))?;
        rx.await.context("SQLite actor stopped before replying")?
    }

    /// A bounded latest-value notification, shared by every clone of this Store.
    pub fn channel_changes(&self) -> watch::Receiver<u64> {
        self.channel_revision.subscribe()
    }

    pub fn notify_channels_changed(&self) {
        self.channel_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    pub async fn user_by_twitch(&self, id: i64) -> Result<Option<User>> {
        self.call(move |c| find_user(c, "twitch_id = ?1", rusqlite::types::Value::Integer(id)))
            .await
    }
    pub async fn user_by_osu(&self, id: i64) -> Result<Option<User>> {
        self.call(move |c| find_user(c, "osu_id = ?1", rusqlite::types::Value::Integer(id)))
            .await
    }
    pub async fn user_by_login(&self, login: &str) -> Result<Option<User>> {
        let login = login.to_owned();
        self.call(move |c| {
            find_user(
                c,
                "twitch_username = ?1 COLLATE NOCASE ORDER BY is_live DESC,twitch_id DESC LIMIT 1",
                rusqlite::types::Value::Text(login),
            )
        })
        .await
    }
    pub async fn enabled_twitch_ids(&self) -> Result<Vec<i64>> {
        self.call(|c| {
            let mut statement = c.prepare("SELECT twitch_id FROM users WHERE json_extract(settings_json,'$.enable') = 1 ORDER BY twitch_id")?;
            Ok(statement.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    /// Enabled channels with verified broadcaster consent that is no longer in
    /// the migration grace window. Public authorization is independent of this.
    pub async fn granted_twitch_ids(&self) -> Result<Vec<i64>> {
        self.call(|c| {
            let mut statement = c.prepare("SELECT u.twitch_id FROM users u JOIN twitch_channel_grants g ON g.twitch_id=u.twitch_id WHERE json_extract(u.settings_json,'$.enable')=1 AND g.status='granted' AND g.active_at<=unixepoch() ORDER BY u.twitch_id")?;
            Ok(statement.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }

    /// Enabled channels that have never completed Twitch channel:bot consent.
    /// Any grant row, including a revoked one, permanently removes eligibility.
    pub async fn legacy_channels(&self, after: i64, limit: i64) -> Result<Vec<User>> {
        ensure!(
            (1..=100).contains(&limit),
            "limit must be between 1 and 100"
        );
        self.call(move |c| {
            let ids: Vec<i64> = {
                let mut statement = c.prepare(
                    "SELECT u.twitch_id FROM users u \
                     WHERE u.twitch_id>?1 \
                       AND json_extract(u.settings_json,'$.enable')=1 \
                       AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=u.twitch_id) \
                     ORDER BY u.twitch_id LIMIT ?2",
                )?;
                statement
                    .query_map(params![after, limit], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            ids.into_iter()
                .map(|id| {
                    find_legacy_user(c, "u.twitch_id=?1", rusqlite::types::Value::Integer(id))?
                        .context("legacy channel disappeared during read")
                })
                .collect()
        })
        .await
    }

    pub async fn legacy_user_by_login(&self, login: &str) -> Result<Option<User>> {
        let login = login.to_owned();
        self.call(move |c| {
            find_legacy_user(
                c,
                "u.twitch_username=?1 COLLATE NOCASE",
                rusqlite::types::Value::Text(login),
            )
        })
        .await
    }

    pub async fn legacy_user_by_twitch(&self, id: i64) -> Result<Option<User>> {
        self.call(move |c| {
            find_legacy_user(c, "u.twitch_id=?1", rusqlite::types::Value::Integer(id))
        })
        .await
    }

    /// Atomically claim one legacy IRC message for a channel that has not
    /// migrated. The unique delivery row is also the new bot's duplicate guard.
    pub async fn claim_legacy_delivery(
        &self,
        twitch_message_id: String,
        twitch_id: i64,
        target_osu_id: i64,
        message: String,
    ) -> Result<ClaimOutcome> {
        ensure!(
            !twitch_message_id.trim().is_empty(),
            "message ID cannot be empty"
        );
        ensure!(!message.trim().is_empty(), "message cannot be empty");
        ensure!(
            twitch_id > 0 && target_osu_id > 0,
            "channel and target IDs must be positive"
        );
        self.call(move |c| {
            let tx = c.transaction()?;
            let user = tx
                .query_row(
                    "SELECT osu_id,COALESCE(json_extract(settings_json,'$.enable'),0) FROM users WHERE twitch_id=?1",
                    [twitch_id],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
                )
                .optional()?;
            let outcome = match user {
                None => ClaimOutcome::Rejected("unknown channel"),
                Some((_, false)) => ClaimOutcome::Rejected("channel disabled"),
                Some((osu_id, true)) => {
                    let migrated: bool = tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM twitch_channel_grants WHERE twitch_id=?1)",
                        [twitch_id],
                        |r| r.get(0),
                    )?;
                    if migrated {
                        ClaimOutcome::Rejected("channel migrated")
                    } else if osu_id != target_osu_id {
                        ClaimOutcome::Rejected("target does not match channel")
                    } else {
                        let inserted = tx.execute(
                            "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) \
                             VALUES(?1,?2,?3,'pending',?4,'legacy') ON CONFLICT DO NOTHING",
                            params![
                                twitch_message_id,
                                target_osu_id,
                                message,
                                chrono::Utc::now().timestamp()
                            ],
                        )?;
                        if inserted == 1 {
                            ClaimOutcome::Claimed
                        } else {
                            ClaimOutcome::Rejected("delivery already claimed")
                        }
                    }
                }
            };
            tx.commit()?;
            Ok(outcome)
        })
        .await
    }

    /// Finish a legacy delivery exactly once. Replaying a terminal report is a
    /// no-op so statistics can never be counted twice.
    pub async fn complete_legacy_delivery(
        &self,
        twitch_message_id: String,
        sent: bool,
        error: Option<String>,
        statistics: Option<RequestEvent>,
    ) -> Result<DeliveryOutcome> {
        ensure!(
            !twitch_message_id.trim().is_empty(),
            "message ID cannot be empty"
        );
        ensure!(!sent || statistics.is_some(), "statistics required");
        if let Some(event) = &statistics {
            event.validate()?;
        }
        self.call(move |c| {
            let tx = c.transaction()?;
            let delivery = tx
                .query_row(
                    "SELECT status,source FROM deliveries WHERE twitch_message_id=?1",
                    [&twitch_message_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?;
            let Some((status, source)) = delivery else {
                return Ok(DeliveryOutcome::NotFound);
            };
            if source != "legacy" {
                return Ok(DeliveryOutcome::NotFound);
            }
            let terminal = matches!(status.as_str(), "sent" | "failed" | "uncertain");
            if !terminal {
                if sent {
                    tx.execute(
                        "UPDATE deliveries SET status='sent',error=NULL WHERE twitch_message_id=?1",
                        [&twitch_message_id],
                    )?;
                    if let Some(event) = statistics {
                        tx.execute(
                            "INSERT INTO statistics(source_id,requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp) \
                             VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id) DO NOTHING",
                            params![
                                format!("twitch:{twitch_message_id}"),
                                event.requester_channel_name,
                                event.requested_beatmap_id,
                                event.requested_channel_name,
                                event.mods,
                                event.timestamp
                            ],
                        )?;
                    }
                } else {
                    tx.execute(
                        "UPDATE deliveries SET status='failed',error=?2 WHERE twitch_message_id=?1",
                        params![twitch_message_id, error],
                    )?;
                }
            }
            tx.commit()?;
            Ok(DeliveryOutcome::Recorded)
        })
        .await
    }
    pub async fn save_user(&self, mut user: User) -> Result<()> {
        user.validate()?;
        self.call(move |c| {
            let transaction = c.transaction()?;
            let existing = transaction.query_row("SELECT twitch_id FROM users WHERE osu_id=?1", [user.osu_id], |r| r.get::<_,i64>(0)).optional()?;
            if let Some(twitch_id) = existing {
                ensure!(twitch_id == user.twitch_id, "osu account is already linked to another Twitch account");
            }
            transaction.execute("INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(osu_id) DO UPDATE SET osu_username=excluded.osu_username,osu_avatar_url=excluded.osu_avatar_url,twitch_username=excluded.twitch_username,twitch_avatar_url=excluded.twitch_avatar_url",
                params![user.osu_id,user.osu_username,user.osu_avatar_url,user.twitch_id,user.twitch_username,user.twitch_avatar_url,false,serde_json::to_string(&user.settings)?])?;
            if existing.is_none() {
                for login in user.excluded_users { transaction.execute("INSERT INTO exclusions(twitch_id,login) VALUES(?1,?2)",params![user.twitch_id,login])?; }
            }
            refresh_eligibility(&transaction, user.twitch_id)?;
            transaction.commit()?; Ok(())
        }).await?;
        self.notify_channels_changed();
        Ok(())
    }
    pub async fn delete_user(&self, twitch_id: i64) -> Result<()> {
        self.call(move |c| {
            c.execute("DELETE FROM users WHERE twitch_id=?1", [twitch_id])?;
            Ok(())
        })
        .await?;
        self.notify_channels_changed();
        Ok(())
    }
    pub async fn set_settings(&self, twitch_id: i64, mut settings: Settings) -> Result<()> {
        settings.validate()?;
        self.call(move |c| {
            let tx = c.transaction()?;
            let current: String = tx
                .query_row(
                    "SELECT settings_json FROM users WHERE twitch_id=?1",
                    [twitch_id],
                    |r| r.get(0),
                )
                .optional()?
                .context("user not found")?;
            let mut merged: Value = serde_json::from_str(&current)?;
            let object = merged
                .as_object_mut()
                .context("stored settings are not an object")?;
            object.extend(
                serde_json::to_value(settings)?
                    .as_object()
                    .context("settings serialization failed")?
                    .clone(),
            );
            tx.execute(
                "UPDATE users SET settings_json=?1 WHERE twitch_id=?2",
                params![merged.to_string(), twitch_id],
            )?;
            refresh_eligibility(&tx, twitch_id)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
        self.notify_channels_changed();
        Ok(())
    }
    pub async fn set_excluded(&self, twitch_id: i64, login: String, excluded: bool) -> Result<()> {
        let login = login.to_lowercase();
        ensure!(!login.trim().is_empty(), "excluded login cannot be empty");
        self.call(move |c| {
            let exists: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE twitch_id=?1)",
                [twitch_id],
                |r| r.get(0),
            )?;
            ensure!(exists, "user not found");
            if excluded {
                c.execute(
                    "INSERT INTO exclusions(twitch_id,login) VALUES(?1,?2) ON CONFLICT DO NOTHING",
                    params![twitch_id, login],
                )?;
            } else {
                c.execute(
                    "DELETE FROM exclusions WHERE twitch_id=?1 AND login=?2",
                    params![twitch_id, login],
                )?;
            }
            Ok(())
        })
        .await
    }
    pub async fn snapshot_versions(&self, ids: Vec<i64>) -> Result<Vec<(i64, i64)>> {
        self.call(move |c| {
            let tx = c.transaction()?;
            let mut versions = Vec::with_capacity(ids.len());
            {
                let mut initialize = tx.prepare("INSERT OR IGNORE INTO channel_stream_state(twitch_id) SELECT twitch_id FROM users WHERE twitch_id=?1")?;
                let mut version = tx.prepare("SELECT revision FROM channel_stream_state WHERE twitch_id=?1")?;
                for id in ids {
                    initialize.execute([id])?;
                    if let Some(revision) = version.query_row([id], |r| r.get(0)).optional()? {
                        versions.push((id, revision));
                    }
                }
            }
            tx.commit()?;
            Ok(versions)
        }).await
    }

    pub async fn apply_stream_snapshot(&self, snapshots: Vec<StreamSnapshot>) -> Result<()> {
        let changed = self.call(move |c| {
            let tx = c.transaction()?;
            let mut changed = false;
            for snapshot in snapshots {
                tx.execute("INSERT OR IGNORE INTO channel_stream_state(twitch_id) SELECT twitch_id FROM users WHERE twitch_id=?1", [snapshot.twitch_id])?;
                let applied = tx.execute(
                    "UPDATE channel_stream_state SET online=?1,category_id=?2,revision=revision+1 WHERE twitch_id=?3 AND revision=?4",
                    params![snapshot.online, snapshot.category_id, snapshot.twitch_id, snapshot.expected_revision],
                )? > 0;
                if !applied { continue; }
                if let Some(login) = snapshot.login.filter(|login| !login.is_empty()) {
                    tx.execute("UPDATE users SET twitch_username=?1 WHERE twitch_id=?2", params![login.to_ascii_lowercase(), snapshot.twitch_id])?;
                }
                refresh_eligibility(&tx, snapshot.twitch_id)?;
                changed = true;
            }
            tx.commit()?;
            Ok(changed)
        }).await?;
        if changed {
            self.notify_channels_changed();
        }
        Ok(())
    }

    /// Apply only a newer event in its timestamp family. Event times are epoch nanoseconds.
    pub async fn apply_stream_event(
        &self,
        twitch_id: i64,
        login: String,
        event: StreamEvent,
        at: i64,
    ) -> Result<bool> {
        let applied = self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("INSERT OR IGNORE INTO channel_stream_state(twitch_id) SELECT twitch_id FROM users WHERE twitch_id=?1", [twitch_id])?;
            let applied = match event {
                StreamEvent::Online => tx.execute(
                    "UPDATE channel_stream_state SET online=1,category_id=CASE WHEN category_event_at<=?1 THEN '' ELSE category_id END,online_event_at=?1,revision=revision+1 WHERE twitch_id=?2 AND online_event_at<?1",
                    params![at, twitch_id],
                )?,
                StreamEvent::Offline => tx.execute(
                    "UPDATE channel_stream_state SET online=0,online_event_at=?1,revision=revision+1 WHERE twitch_id=?2 AND online_event_at<?1",
                    params![at, twitch_id],
                )?,
                StreamEvent::Category(category) => tx.execute(
                    "UPDATE channel_stream_state SET category_id=?1,category_event_at=?2,revision=revision+1 WHERE twitch_id=?3 AND category_event_at<?2 AND (online=0 OR online_event_at<?2)",
                    params![category, at, twitch_id],
                )?,
            } > 0;
            if applied {
                if !login.is_empty() {
                    tx.execute("UPDATE users SET twitch_username=?1 WHERE twitch_id=?2", params![login.to_ascii_lowercase(), twitch_id])?;
                }
                refresh_eligibility(&tx, twitch_id)?;
            }
            tx.commit()?;
            Ok(applied)
        }).await?;
        if applied {
            self.notify_channels_changed();
        }
        Ok(applied)
    }

    pub async fn live_channels(&self) -> Result<Vec<(i64, String)>> {
        self.call(|c| {
            let mut statement = c.prepare(
                "SELECT twitch_id,twitch_username FROM users WHERE is_live=1 ORDER BY twitch_id",
            )?;
            Ok(statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Replace the legacy relay's live snapshot. Only enabled accounts without
    /// any grant row are recorded; malformed or duplicate entries are rejected
    /// before the transaction so one bad poll cannot wipe presence.
    pub async fn replace_legacy_live(&self, channels: Vec<(i64, String)>) -> Result<usize> {
        ensure!(channels.len() <= 10_000, "too many live channels");
        let mut seen = std::collections::HashSet::with_capacity(channels.len());
        let mut normalized = Vec::with_capacity(channels.len());
        for (twitch_id, login) in channels {
            let login = login.trim().to_ascii_lowercase();
            ensure!(twitch_id > 0, "live channel IDs must be positive");
            ensure!(!login.is_empty(), "live channel logins cannot be empty");
            ensure!(seen.insert(twitch_id), "duplicate live channel {twitch_id}");
            normalized.push((twitch_id, login));
        }
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("DELETE FROM legacy_live", [])?;
            let mut updated = 0usize;
            for (twitch_id, login) in normalized {
                let eligible: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM users u WHERE u.twitch_id=?1 AND json_extract(u.settings_json,'$.enable')=1 AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=u.twitch_id))",
                    [twitch_id],
                    |r| r.get(0),
                )?;
                if !eligible {
                    continue;
                }
                tx.execute(
                    "UPDATE users SET twitch_username=?1 WHERE twitch_id=?2",
                    params![login, twitch_id],
                )?;
                tx.execute(
                    "INSERT INTO legacy_live(twitch_id,login,updated_at) VALUES(?1,?2,unixepoch())",
                    params![twitch_id, login],
                )?;
                updated += 1;
            }
            tx.commit()?;
            Ok(updated)
        })
        .await
    }

    pub async fn revoke_twitch_channel(&self, twitch_id: i64) -> Result<()> {
        self.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("UPDATE twitch_channel_grants SET status='revoked' WHERE twitch_id=?1 AND status='granted'", [twitch_id])?;
            refresh_eligibility(&tx, twitch_id)?;
            tx.commit()?;
            Ok(())
        }).await?;
        self.notify_channels_changed();
        Ok(())
    }
    pub async fn live_users(&self, limit: i64, offset: i64) -> Result<Vec<String>> {
        pagination(limit, offset)?;
        self.call(move |c| {
            let mut s=c.prepare("SELECT login FROM (SELECT twitch_username AS login FROM users WHERE is_live=1 UNION SELECT l.login AS login FROM legacy_live l JOIN users u ON u.twitch_id=l.twitch_id WHERE l.updated_at>=unixepoch()-180 AND json_extract(u.settings_json,'$.enable')=1 AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=l.twitch_id)) ORDER BY login LIMIT ?1 OFFSET ?2")?;
            Ok(s.query_map(params![limit,offset],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }
    pub async fn record_request(&self, event: RequestEvent) -> Result<()> {
        event.validate()?;
        self.call(move |c| {
            c.execute("INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp) VALUES(?1,?2,?3,?4,?5)",params![event.requester_channel_name,event.requested_beatmap_id,event.requested_channel_name,event.mods,event.timestamp])?;
            Ok(())
        }).await
    }
    pub async fn leaderboard(&self, since: i64, limit: i64, offset: i64) -> Result<Vec<Value>> {
        pagination(limit, offset)?;
        self.call(move |c| {
            let mut s=c.prepare("SELECT s.requested_beatmap_id,COUNT(*) AS total,b.raw_json FROM statistics s LEFT JOIN beatmaps b ON b.id=s.requested_beatmap_id WHERE s.timestamp>=?1 GROUP BY s.requested_beatmap_id ORDER BY total DESC,s.requested_beatmap_id ASC LIMIT ?2 OFFSET ?3")?;
            let rows=s.query_map(params![since,limit,offset],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?)))?;
            rows.map(|row| {
                let (id,count,raw)=row?;
                let mut value=match raw {Some(raw)=>serde_json::from_str::<Value>(&raw)?,None=>json!({})};
                let obj=value.as_object_mut().context("beatmap metadata must be an object")?;
                obj.remove("_id"); obj.insert("_id".into(),json!(id)); obj.insert("count".into(),json!(count));
                Ok(value)
            }).collect()
        }).await
    }
    pub async fn cache_get(&self, id: i64) -> Result<Option<Value>> {
        self.call(move |c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT raw_json FROM beatmaps WHERE id=?1 AND fetched_at>?2",
                    params![id, chrono::Utc::now().timestamp() - 86_400],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|s| serde_json::from_str(&s).map_err(Into::into))
                .transpose()
        })
        .await
    }
    pub async fn cache_put(&self, value: Value) -> Result<()> {
        self.call(move |c| {
            put_cached_map(c, &value, chrono::Utc::now().timestamp())?;
            Ok(())
        })
        .await
    }
    pub async fn cache_get_set(&self, set_id: i64) -> Result<Option<Value>> {
        self.call(move |c| {
            let raw: Option<String> = c.query_row(
                "SELECT b.raw_json FROM mapset_cache s JOIN beatmaps b ON b.id=s.beatmap_id AND b.beatmapset_id=s.set_id WHERE s.set_id=?1 AND s.fetched_at>?2 AND b.fetched_at>?2",
                params![set_id,chrono::Utc::now().timestamp()-86_400],
                |row|row.get(0),
            ).optional()?;
            raw.map(|raw|serde_json::from_str(&raw).map_err(Into::into)).transpose()
        }).await
    }
    /// Persist only an actual beatmapset API selection, never an arbitrary cached map.
    pub async fn cache_put_set(&self, set_id: i64, value: Value) -> Result<()> {
        ensure!(set_id > 0, "beatmapset ID must be positive");
        self.call(move |c| {
            let tx=c.transaction()?;
            let fetched_at=chrono::Utc::now().timestamp();
            let (beatmap_id,actual_set_id)=put_cached_map(&tx,&value,fetched_at)?;
            ensure!(actual_set_id==set_id,"selected beatmap does not belong to requested set");
            tx.execute("INSERT INTO mapset_cache(set_id,beatmap_id,fetched_at) VALUES(?1,?2,?3) ON CONFLICT(set_id) DO UPDATE SET beatmap_id=excluded.beatmap_id,fetched_at=excluded.fetched_at",params![set_id,beatmap_id,fetched_at])?;
            tx.commit()?;
            Ok(())
        }).await
    }
    pub async fn missing_beatmap_ids(&self, after: i64, limit: i64) -> Result<Vec<i64>> {
        ensure!(
            (1..=1000).contains(&limit),
            "limit must be between 1 and 1000"
        );
        self.call(move |c| {
            let mut s=c.prepare("SELECT DISTINCT requested_beatmap_id FROM statistics s WHERE requested_beatmap_id>?1 AND NOT EXISTS(SELECT 1 FROM beatmaps b WHERE b.id=s.requested_beatmap_id) ORDER BY requested_beatmap_id LIMIT ?2")?;
            Ok(s.query_map(params![after,limit],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }).await
    }
    pub async fn settings_metadata(&self) -> Result<Vec<Value>> {
        self.call(|c| {
            let mut s=c.prepare("SELECT name,value_json,description,type,raw_json FROM settings_metadata ORDER BY rowid")?;
            let rows=s.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?;
            rows.map(|row| {
                let (name,value,description,kind,raw)=row?;
                let mut document:Value=serde_json::from_str(&raw)?;
                let obj=document.as_object_mut().context("invalid settings metadata")?;
                obj.remove("_id");obj.insert("name".into(),json!(name));obj.insert("value".into(),serde_json::from_str(&value)?);obj.insert("description".into(),json!(description));obj.insert("type".into(),json!(kind));
                Ok(document)
            }).collect()
        }).await
    }
}

fn put_cached_map(connection: &Connection, value: &Value, fetched_at: i64) -> Result<(i64, i64)> {
    let id = value
        .get("id")
        .and_then(Value::as_i64)
        .context("beatmap id must be an integer")?;
    let set = value
        .get("beatmapset_id")
        .and_then(Value::as_i64)
        .context("beatmapset_id must be an integer")?;
    ensure!(id > 0 && set > 0, "beatmap IDs must be positive");
    connection.execute("INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET beatmapset_id=excluded.beatmapset_id,fetched_at=excluded.fetched_at,raw_json=excluded.raw_json",params![id,set,fetched_at,value.to_string()])?;
    Ok((id, set))
}

fn pagination(limit: i64, offset: i64) -> Result<()> {
    ensure!(
        (1..=100).contains(&limit) && offset >= 0,
        "invalid pagination"
    );
    Ok(())
}

fn find_user(c: &Connection, filter: &str, key: rusqlite::types::Value) -> Result<Option<User>> {
    let sql = format!(
        "SELECT osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json FROM users WHERE {filter}"
    );
    let row = c
        .query_row(&sql, [key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, String>(7)?,
            ))
        })
        .optional()?;
    let Some((
        osu_id,
        osu_username,
        osu_avatar_url,
        twitch_id,
        twitch_username,
        twitch_avatar_url,
        is_live,
        settings,
    )) = row
    else {
        return Ok(None);
    };
    let mut statement =
        c.prepare("SELECT login FROM exclusions WHERE twitch_id=?1 ORDER BY login")?;
    let excluded_users = statement
        .query_map([twitch_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(User {
        osu_id,
        osu_username,
        osu_avatar_url,
        twitch_id,
        twitch_username,
        twitch_avatar_url,
        excluded_users,
        is_live,
        settings: serde_json::from_str(&settings)?,
    }))
}

fn find_legacy_user(
    c: &Connection,
    filter: &str,
    key: rusqlite::types::Value,
) -> Result<Option<User>> {
    let sql = format!(
        "SELECT u.osu_id,u.osu_username,u.osu_avatar_url,u.twitch_id,u.twitch_username,u.twitch_avatar_url,u.is_live,u.settings_json \
         FROM users u WHERE json_extract(u.settings_json,'$.enable')=1 \
           AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=u.twitch_id) \
           AND {filter} LIMIT 1"
    );
    let row = c
        .query_row(&sql, [key], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, String>(7)?,
            ))
        })
        .optional()?;
    let Some((
        osu_id,
        osu_username,
        osu_avatar_url,
        twitch_id,
        twitch_username,
        twitch_avatar_url,
        is_live,
        settings,
    )) = row
    else {
        return Ok(None);
    };
    let mut statement =
        c.prepare("SELECT login FROM exclusions WHERE twitch_id=?1 ORDER BY login")?;
    let excluded_users = statement
        .query_map([twitch_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(User {
        osu_id,
        osu_username,
        osu_avatar_url,
        twitch_id,
        twitch_username,
        twitch_avatar_url,
        excluded_users,
        is_live,
        settings: serde_json::from_str(&settings)?,
    }))
}

/// Twitch categories that count as an osu! stream: the classic `osu!` game and
/// the separate `Osu!Lazer` client category.
pub(crate) const OSU_CATEGORIES: [&str; 2] = ["21465", "295941293"];

/// Keep public activity and bot membership derived from current state and consent.
/// Call within the transaction that changes a user, setting, grant, or stream state.
pub(crate) fn refresh_eligibility(conn: &Connection, twitch_id: i64) -> Result<()> {
    // Destructuring keeps the statement's placeholders in step with the list.
    let [osu, lazer] = OSU_CATEGORIES;
    conn.execute(
        "UPDATE users SET is_live=(COALESCE(json_extract(settings_json,'$.enable'),0)=1 AND EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=users.twitch_id AND g.status='granted') AND EXISTS(SELECT 1 FROM channel_stream_state s WHERE s.twitch_id=users.twitch_id AND s.online=1 AND s.category_id IN (?2,?3))) WHERE twitch_id=?1",
        params![twitch_id, osu, lazer],
    )?;
    Ok(())
}

fn initialize(c: &mut Connection) -> Result<()> {
    let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version <= 4,
        "database schema version {version} is newer than this application"
    );
    let tx = c.transaction()?;
    if version == 0 {
        tx.execute_batch(include_str!("../migrations/001_initial.sql"))?;
    }
    if version < 2 {
        tx.execute_batch(include_str!("../migrations/002_modern_twitch.sql"))?;
        tx.pragma_update(None, "user_version", 2)?;
    }
    if version < 3 {
        tx.execute_batch(include_str!("../migrations/003_coexistence.sql"))?;
        tx.pragma_update(None, "user_version", 3)?;
    }
    if version < 4 {
        tx.execute_batch(include_str!("../migrations/004_legacy_live.sql"))?;
        tx.pragma_update(None, "user_version", 4)?;
    }
    for document in default_metadata() {
        tx.execute("INSERT INTO settings_metadata(name,value_json,description,type,raw_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(name) DO NOTHING",params![document["name"].as_str().unwrap(),document["value"].to_string(),document["description"].as_str().unwrap(),document["type"].as_str().unwrap(),document.to_string()])?;
    }
    tx.execute("UPDATE deliveries SET status='uncertain',error=COALESCE(error,'process stopped during delivery; not replayed') WHERE status='sending'",[])?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn default_metadata() -> Vec<Value> {
    vec![
        json!({"name":"enable","value":true,"description":"Enables the bot.","type":"toggle"}),
        json!({"name":"echo","value":true,"description":"Enables Twitch chat acknowledge message.","type":"toggle"}),
        json!({"name":"sub-only","value":false,"description":"Subscribers only request mode.","type":"toggle"}),
        json!({"name":"points-only","value":false,"description":"Channel Points only request mode.","type":"toggle"}),
        json!({"name":"test","value":false,"description":"Enables test mode. (Removes all restrictions.)","type":"toggle"}),
        json!({"name":"cooldown","value":30,"description":"Cooldown for requests.","type":"value"}),
        json!({"name":"sr","value":[0,-1],"description":"Star rating limit for requests.","type":"range"}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
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
    async fn set_grant(store: &Store, id: i64, status: &str) {
        let status = status.to_owned();
        store
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(?1,0,?2) ON CONFLICT(twitch_id) DO UPDATE SET status=excluded.status",
                    params![id, status],
                )?;
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
    #[tokio::test]
    async fn linking_preserves_preferences_and_enforces_identity() {
        let store = Store::open(":memory:").await.unwrap();
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
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        let mut new_owner = user();
        new_owner.osu_id = 3;
        new_owner.twitch_id = 1;
        new_owner.osu_username = "new owner".into();
        store.save_user(new_owner.clone()).await.unwrap();
        grant(&store, 1).await;
        let versions = store.snapshot_versions(vec![1]).await.unwrap();
        assert_eq!(versions, vec![(1, 0)]);
        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 1,
                expected_revision: 0,
                online: true,
                category_id: "21465".into(),
                login: Some("CHANNEL".into()),
            }])
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
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        store
            .call(|c| {
                c.execute("INSERT INTO sessions VALUES('hash',2,999999)", [])?;
                Ok(())
            })
            .await
            .unwrap();
        store.delete_user(2).await.unwrap();
        let count: i64 = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT (SELECT COUNT(*) FROM sessions)+(SELECT COUNT(*) FROM exclusions)",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    #[tokio::test]
    async fn history_joins_raw_metadata_and_lists_missing_ids() {
        let store = Store::open(":memory:").await.unwrap();
        store.cache_put(json!({"id":11,"beatmapset_id":12,"version":"Hard","beatmapset":{"artist":"Artist"}})).await.unwrap();
        for id in [11, 11, 13] {
            store
                .record_request(RequestEvent {
                    requester_channel_name: "a".into(),
                    requested_beatmap_id: id,
                    requested_channel_name: "b".into(),
                    mods: None,
                    timestamp: 100,
                })
                .await
                .unwrap();
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
        let store = Store::open(":memory:").await.unwrap();
        let selected = json!({"id":2,"beatmapset_id":10,"version":"selected"});
        store.cache_put_set(10, selected.clone()).await.unwrap();
        store
            .call(|c| {
                c.execute(
                    "UPDATE mapset_cache SET fetched_at=?1",
                    [chrono::Utc::now().timestamp() - 86_400],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(store.cache_get_set(10).await.unwrap().is_none());
        assert_eq!(store.cache_get(2).await.unwrap(), Some(selected.clone()));
        store.cache_put_set(10, selected.clone()).await.unwrap();
        store
            .call(|c| {
                c.execute(
                    "UPDATE beatmaps SET fetched_at=?1",
                    [chrono::Utc::now().timestamp() - 86_400],
                )?;
                Ok(())
            })
            .await
            .unwrap();
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
        let store = Store::open(":memory:").await.unwrap();
        store
            .cache_put(json!({"id":11,"beatmapset_id":12}))
            .await
            .unwrap();
        store
            .call(|c| {
                c.execute("UPDATE beatmaps SET fetched_at=0", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(store.cache_get(11).await.unwrap().is_none());
        let count: i64 = store
            .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM beatmaps", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
    #[tokio::test]
    async fn restart_marks_sending_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.sqlite");
        let store = Store::open(&path).await.unwrap();
        store.call(|c|{c.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('m',1,'pm','sending',0)",[])?;Ok(())}).await.unwrap();
        let second = Store::open(&path).await.unwrap();
        let status: String = second
            .call(|c| Ok(c.query_row("SELECT status FROM deliveries", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(status, "uncertain");
    }
    #[tokio::test]
    async fn actor_serializes_parallel_calls_off_runtime_threads() {
        let store = Store::open(":memory:").await.unwrap();
        let mut jobs = Vec::new();
        for _ in 0..256 {
            let store = store.clone();
            jobs.push(tokio::spawn(async move {
                store.call(|c| {
                    assert_eq!(std::thread::current().name(),Some("ronnia-sqlite"));
                    c.execute("INSERT INTO statistics(requester_channel_name,requested_beatmap_id,requested_channel_name,timestamp) VALUES('viewer',1,'channel',0)",[])?;
                    Ok(())
                }).await
            }));
        }
        for job in jobs {
            job.await.unwrap().unwrap();
        }
        let count: i64 = store
            .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM statistics", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(count, 256);
    }
    #[tokio::test]
    async fn grants_and_independent_stream_families_drive_eligibility() {
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        assert_eq!(store.enabled_twitch_ids().await.unwrap(), vec![2]);
        assert!(store.granted_twitch_ids().await.unwrap().is_empty());
        assert!(store.live_channels().await.unwrap().is_empty());
        grant(&store, 2).await;
        assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![2]);

        let mut changes = store.channel_changes();
        let observed = *changes.borrow();
        assert!(
            store
                .apply_stream_event(2, "CHANNEL".into(), StreamEvent::Online, 100)
                .await
                .unwrap()
        );
        tokio::time::timeout(Duration::from_secs(1), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(*changes.borrow(), observed);
        let online = store.user_by_twitch(2).await.unwrap().unwrap();
        assert!(
            !online.is_live,
            "an online notice alone is not osu eligibility"
        );

        assert!(
            store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("21465".into()),
                    200
                )
                .await
                .unwrap()
        );
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);
        assert_eq!(
            store.live_channels().await.unwrap(),
            vec![(2, "channel".into())]
        );

        let stable_revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
        assert!(
            !store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("21465".into()),
                    200
                )
                .await
                .unwrap()
        );
        assert!(
            !store
                .apply_stream_event(2, "channel".into(), StreamEvent::Category("1".into()), 150)
                .await
                .unwrap()
        );
        assert_eq!(
            store.snapshot_versions(vec![2]).await.unwrap()[0].1,
            stable_revision
        );
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);

        assert!(
            store
                .apply_stream_event(2, "channel".into(), StreamEvent::Online, 300)
                .await
                .unwrap()
        );
        assert!(
            !store.user_by_twitch(2).await.unwrap().unwrap().is_live,
            "online clears the stale category"
        );
        assert!(
            !store
                .apply_stream_event(2, "channel".into(), StreamEvent::Offline, 250)
                .await
                .unwrap(),
            "late offline cannot undo a newer online"
        );
        assert!(
            !store
                .apply_stream_event(2, "channel".into(), StreamEvent::Category("1".into()), 250)
                .await
                .unwrap(),
            "a category older than the online notice cannot reinstate it"
        );
        assert!(!store.user_by_twitch(2).await.unwrap().unwrap().is_live);
        assert!(
            store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("21465".into()),
                    400
                )
                .await
                .unwrap()
        );
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);
        assert!(
            !store
                .apply_stream_event(2, "channel".into(), StreamEvent::Category("1".into()), 350)
                .await
                .unwrap()
        );
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);

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
            store.user_by_twitch(2).await.unwrap().unwrap().is_live,
            "regrant uses the known stream state"
        );

        let disabled = Settings {
            enable: false,
            ..Settings::default()
        };
        store.set_settings(2, disabled).await.unwrap();
        assert!(store.enabled_twitch_ids().await.unwrap().is_empty());
        assert!(store.granted_twitch_ids().await.unwrap().is_empty());
        assert!(!store.user_by_twitch(2).await.unwrap().unwrap().is_live);
        store.set_settings(2, Settings::default()).await.unwrap();
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);
    }
    #[tokio::test]
    async fn osu_lazer_counts_as_osu_but_other_categories_do_not() {
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        grant(&store, 2).await;

        store
            .apply_stream_event(2, "channel".into(), StreamEvent::Online, 100)
            .await
            .unwrap();
        assert!(
            store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category(OSU_CATEGORIES[1].into()),
                    200,
                )
                .await
                .unwrap()
        );
        assert!(
            store.user_by_twitch(2).await.unwrap().unwrap().is_live,
            "Osu!Lazer is an osu! stream"
        );
        assert_eq!(
            store.live_channels().await.unwrap(),
            vec![(2, "channel".into())]
        );

        // A discovery snapshot carrying Osu!Lazer keeps the channel eligible.
        let revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 2,
                expected_revision: revision,
                online: true,
                category_id: OSU_CATEGORIES[1].into(),
                login: Some("channel".into()),
            }])
            .await
            .unwrap();
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);

        // Any other category, such as Just Chatting, is not an osu! stream.
        assert!(
            store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("509659".into()),
                    300,
                )
                .await
                .unwrap()
        );
        assert!(!store.user_by_twitch(2).await.unwrap().unwrap().is_live);
        assert!(store.live_channels().await.unwrap().is_empty());

        // The classic osu! category remains eligible.
        let revision = store.snapshot_versions(vec![2]).await.unwrap()[0].1;
        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 2,
                expected_revision: revision,
                online: true,
                category_id: OSU_CATEGORIES[0].into(),
                login: Some("channel".into()),
            }])
            .await
            .unwrap();
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);
    }
    #[tokio::test]
    async fn stale_snapshots_cannot_overwrite_newer_events_or_unknown_rows() {
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        grant(&store, 2).await;
        let versions = store.snapshot_versions(vec![2, 999]).await.unwrap();
        assert_eq!(versions, vec![(2, 0)]);
        assert!(
            !store.user_by_twitch(2).await.unwrap().unwrap().is_live,
            "capturing versions never implies live"
        );
        assert!(
            store
                .apply_stream_event(2, "channel".into(), StreamEvent::Online, 100)
                .await
                .unwrap()
        );
        assert!(
            store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("21465".into()),
                    200
                )
                .await
                .unwrap()
        );
        assert!(store.user_by_twitch(2).await.unwrap().unwrap().is_live);

        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 2,
                expected_revision: 0,
                online: false,
                category_id: String::new(),
                login: Some("stale".into()),
            }])
            .await
            .unwrap();
        let user_now = store.user_by_twitch(2).await.unwrap().unwrap();
        assert!(
            user_now.is_live,
            "a slow HTTP snapshot cannot undo newer events"
        );
        assert_eq!(user_now.twitch_username, "channel");

        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 2,
                expected_revision: 2,
                online: false,
                category_id: String::new(),
                login: Some("Renamed".into()),
            }])
            .await
            .unwrap();
        let user_now = store.user_by_twitch(2).await.unwrap().unwrap();
        assert!(!user_now.is_live);
        assert_eq!(user_now.twitch_username, "renamed");
        assert!(store.live_channels().await.unwrap().is_empty());

        store
            .apply_stream_snapshot(vec![StreamSnapshot {
                twitch_id: 2,
                expected_revision: 2,
                online: true,
                category_id: "21465".into(),
                login: None,
            }])
            .await
            .unwrap();
        assert!(
            !store.user_by_twitch(2).await.unwrap().unwrap().is_live,
            "replayed snapshots stay rejected"
        );
        assert!(
            !store
                .apply_stream_event(
                    2,
                    "channel".into(),
                    StreamEvent::Category("21465".into()),
                    50
                )
                .await
                .unwrap()
        );
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
                .execute_batch(include_str!("../migrations/001_initial.sql"))
                .unwrap();
            connection.execute(
                "INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json,raw_json) VALUES(10,'osu','avatar',20,'CHANNEL','twitch-avatar',1,?1,?2)",
                params![json!({"enable":true,"echo":false}).to_string(), json!({"legacy":true}).to_string()],
            ).unwrap();
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
        assert!(store.enabled_twitch_ids().await.unwrap() == vec![20]);
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
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(user()).await.unwrap();
        grant(&store, 2).await;
        assert_eq!(
            store.snapshot_versions(vec![2]).await.unwrap(),
            vec![(2, 0)]
        );
        let snapshot = store.apply_stream_snapshot(vec![StreamSnapshot {
            twitch_id: 2,
            expected_revision: 0,
            online: true,
            category_id: "21465".into(),
            login: Some("CHANNEL".into()),
        }]);
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
        assert!(!store.user_by_twitch(2).await.unwrap().unwrap().is_live);
    }
    #[tokio::test]
    async fn modern_singletons_are_enforced_and_grants_cascade_with_accounts() {
        let store = Store::open(":memory:").await.unwrap();
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
        let remaining: (i64, i64) = store
            .call(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM twitch_channel_grants", [], |r| {
                        r.get(0)
                    })?,
                    c.query_row("SELECT COUNT(*) FROM channel_stream_state", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(remaining, (0, 0));
    }

    #[tokio::test]
    async fn grant_activation_waits_for_active_at_but_public_consent_is_unchanged() {
        let store = Store::open(":memory:").await.unwrap();
        store.save_user(account(10, 20, "alpha")).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        store
            .call(move |c| {
                c.execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(20,0,?1,'granted')",
                    [now + 3600],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(store.granted_twitch_ids().await.unwrap().is_empty());
        store
            .call(|c| {
                c.execute("UPDATE twitch_channel_grants SET active_at=0", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![20]);
    }

    #[tokio::test]
    async fn legacy_channels_exclude_disabled_and_any_grant_and_page_by_twitch_id() {
        let store = Store::open(":memory:").await.unwrap();
        for (osu, twitch, login) in [
            (10, 20, "alpha"),
            (11, 21, "beta"),
            (12, 22, "disabled"),
            (13, 23, "gamma"),
        ] {
            store.save_user(account(osu, twitch, login)).await.unwrap();
        }
        store
            .set_settings(
                22,
                Settings {
                    enable: false,
                    ..Settings::default()
                },
            )
            .await
            .unwrap();
        // Revoked grants are still proof of an attempted migration.
        set_grant(&store, 20, "revoked").await;
        let all = store.legacy_channels(0, 100).await.unwrap();
        assert_eq!(
            all.iter().map(|u| u.twitch_id).collect::<Vec<_>>(),
            vec![21, 23]
        );
        assert_eq!(
            store
                .legacy_channels(20, 1)
                .await
                .unwrap()
                .iter()
                .map(|u| u.twitch_id)
                .collect::<Vec<_>>(),
            vec![21]
        );
        assert_eq!(
            store
                .legacy_channels(21, 100)
                .await
                .unwrap()
                .iter()
                .map(|u| u.twitch_id)
                .collect::<Vec<_>>(),
            vec![23]
        );
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
        let store = Store::open(":memory:").await.unwrap();
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
        store
            .call(|c| {
                c.execute(
                    "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-message',10,'pm','pending',0,'new')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
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
        store
            .set_settings(
                20,
                Settings {
                    enable: false,
                    ..Settings::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .claim_legacy_delivery("m3".into(), 20, 10, "!request 3".into())
                .await
                .unwrap(),
            ClaimOutcome::Rejected("channel disabled")
        );
        let pending: i64 = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM deliveries WHERE source='legacy'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn legacy_delivery_reports_are_idempotent_for_statistics() {
        let store = Store::open(":memory:").await.unwrap();
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
        let (status, source, rows, requester): (String, String, i64, String) = store
            .call(|c| {
                Ok((
                    c.query_row(
                        "SELECT status FROM deliveries WHERE twitch_message_id='m1'",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row(
                        "SELECT source_id FROM statistics WHERE source_id='twitch:m1'",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row("SELECT COUNT(*) FROM statistics", [], |r| r.get(0))?,
                    c.query_row(
                        "SELECT requester_channel_name FROM statistics WHERE source_id='twitch:m1'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(status, "sent");
        assert_eq!(source, "twitch:m1");
        assert_eq!(rows, 1);
        assert_eq!(requester, "viewer");

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
        let (status, error): (String, Option<String>) = store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT status,error FROM deliveries WHERE twitch_message_id='m2'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("osu PM failed"));
        assert_eq!(
            store
                .complete_legacy_delivery("missing".into(), false, None, None)
                .await
                .unwrap(),
            DeliveryOutcome::NotFound
        );
        store
            .call(|c| {
                c.execute(
                    "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-message',10,'pm','pending',0,'new')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
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
        let store = Store::open(":memory:").await.unwrap();
        for (osu, twitch, login) in [(10, 20, "alpha"), (11, 21, "beta"), (12, 22, "new-live")] {
            store.save_user(account(osu, twitch, login)).await.unwrap();
        }
        store
            .call(|c| {
                c.execute("UPDATE users SET is_live=1 WHERE twitch_id=22", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .replace_legacy_live(vec![
                    (20, "Alpha_New".into()),
                    (21, "BETA".into()),
                    (999, "ghost".into()),
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

        store
            .call(|c| {
                c.execute(
                    "UPDATE legacy_live SET updated_at=unixepoch()-181 WHERE twitch_id=20",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store.live_users(100, 0).await.unwrap(),
            vec!["beta", "new-live"]
        );
    }

    #[tokio::test]
    async fn legacy_live_reports_skip_ineligible_accounts_and_remove_rows() {
        let store = Store::open(":memory:").await.unwrap();
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
        store
            .set_settings(
                20,
                Settings {
                    enable: false,
                    ..Settings::default()
                },
            )
            .await
            .unwrap();
        grant(&store, 21).await;
        assert_eq!(
            store
                .replace_legacy_live(vec![
                    (20, "alpha".into()),
                    (21, "beta".into()),
                    (22, "gamma".into()),
                    (23, "missing".into()),
                ])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .call(|c| {
                    let logins: Vec<String> = c
                        .prepare("SELECT login FROM legacy_live ORDER BY twitch_id")?
                        .query_map([], |r| r.get(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(logins)
                })
                .await
                .unwrap(),
            vec!["gamma"]
        );
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
                .execute_batch(include_str!("../migrations/001_initial.sql"))
                .unwrap();
            connection
                .execute_batch(include_str!("../migrations/002_modern_twitch.sql"))
                .unwrap();
            connection
                .execute(
                    "INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(10,'osu',20,'alpha',?1)",
                    [json!({"enable":true}).to_string()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(20,0,'granted')",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('old',10,'pm','pending',0)",
                    [],
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 2).unwrap();
        }
        let store = Store::open(&path).await.unwrap();
        let version: i64 = store
            .call(|c| Ok(c.pragma_query_value(None, "user_version", |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(version, 4);
        let (active_at, source, status): (i64, String, String) = store
            .call(|c| {
                let active_at = c.query_row(
                    "SELECT active_at FROM twitch_channel_grants WHERE twitch_id=20",
                    [],
                    |r| r.get(0),
                )?;
                let source = c.query_row(
                    "SELECT source FROM deliveries WHERE twitch_message_id='old'",
                    [],
                    |r| r.get(0),
                )?;
                let status = c.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='old'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((active_at, source, status))
            })
            .await
            .unwrap();
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
                .execute_batch(include_str!("../migrations/001_initial.sql"))
                .unwrap();
            connection
                .execute_batch(include_str!("../migrations/002_modern_twitch.sql"))
                .unwrap();
            connection
                .execute_batch(include_str!("../migrations/003_coexistence.sql"))
                .unwrap();
            connection
                .execute(
                    "INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(10,'osu',20,'alpha',?1)",
                    [json!({"enable":true}).to_string()],
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 3).unwrap();
        }
        let store = Store::open(&path).await.unwrap();
        let (version, legacy_count, user_count): (i64, i64, i64) = store
            .call(|c| {
                Ok((
                    c.pragma_query_value(None, "user_version", |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM legacy_live", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
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
}
