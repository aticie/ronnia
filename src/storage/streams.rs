//! Stream presence: revisioned snapshots, timestamped events, legacy relay
//! presence, and the public live-channel union.
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::collections::HashSet;

use super::{Store, sql, users};
use crate::domain::{StreamEvent, StreamSnapshot};

fn ensure_row(conn: &Connection, twitch_id: i64) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO channel_stream_state(twitch_id) SELECT twitch_id FROM users WHERE twitch_id=?1", [twitch_id])?;
    Ok(())
}

fn refresh(conn: &Connection, twitch_id: i64, login: Option<&str>) -> Result<()> {
    if let Some(login) = login.filter(|login| !login.is_empty()) {
        conn.execute(
            "UPDATE users SET twitch_username=?1 WHERE twitch_id=?2",
            params![login.to_ascii_lowercase(), twitch_id],
        )?;
    }
    users::refresh_eligibility(conn, twitch_id)
}

impl Store {
    pub async fn snapshot_versions(&self, ids: Vec<i64>) -> Result<Vec<(i64, i64)>> {
        self.write(move |tx| {
            let mut versions = Vec::with_capacity(ids.len());
            for id in ids {
                ensure_row(tx, id)?;
                if let Some(revision) = sql::optional(
                    tx,
                    "SELECT revision FROM channel_stream_state WHERE twitch_id=?1",
                    [id],
                )? {
                    versions.push((id, revision));
                }
            }
            Ok(versions)
        })
        .await
    }
    pub async fn apply_stream_snapshot(&self, snapshots: Vec<StreamSnapshot>) -> Result<()> {
        let changed = self.write(move |tx| {
            let mut changed = false;
            for snapshot in snapshots {
                ensure_row(tx, snapshot.twitch_id)?;
                let applied = tx.execute("UPDATE channel_stream_state SET online=?1,category_id=?2,revision=revision+1 WHERE twitch_id=?3 AND revision=?4", params![snapshot.online, snapshot.category_id, snapshot.twitch_id, snapshot.expected_revision])? > 0;
                if applied { refresh(tx, snapshot.twitch_id, snapshot.login.as_deref())?; changed = true; }
            }
            Ok(changed)
        }).await?;
        if changed {
            self.notify_channels_changed();
        }
        Ok(())
    }
    pub async fn apply_stream_event(
        &self,
        twitch_id: i64,
        login: String,
        event: StreamEvent,
        at: i64,
    ) -> Result<bool> {
        let applied = self.write(move |tx| {
            ensure_row(tx, twitch_id)?;
            let applied = match event {
                StreamEvent::Online => tx.execute("UPDATE channel_stream_state SET online=1,category_id=CASE WHEN category_event_at<=?1 THEN '' ELSE category_id END,online_event_at=?1,revision=revision+1 WHERE twitch_id=?2 AND online_event_at<?1", params![at, twitch_id])?,
                StreamEvent::Offline => tx.execute("UPDATE channel_stream_state SET online=0,online_event_at=?1,revision=revision+1 WHERE twitch_id=?2 AND online_event_at<?1", params![at, twitch_id])?,
                StreamEvent::Category(category) => tx.execute("UPDATE channel_stream_state SET category_id=?1,category_event_at=?2,revision=revision+1 WHERE twitch_id=?3 AND category_event_at<?2 AND (online=0 OR online_event_at<?2)", params![category, at, twitch_id])?,
            } > 0;
            if applied { refresh(tx, twitch_id, Some(&login))?; }
            Ok(applied)
        }).await?;
        if applied {
            self.notify_channels_changed();
        }
        Ok(applied)
    }
    pub async fn live_channels(&self) -> Result<Vec<(i64, String)>> {
        self.call(|c| {
            sql::all(
                c,
                "SELECT twitch_id,twitch_username FROM users WHERE is_live=1 ORDER BY twitch_id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
    }
    pub async fn replace_legacy_live(&self, channels: Vec<(i64, String)>) -> Result<usize> {
        ensure!(channels.len() <= 10_000, "too many live channels");
        let mut seen = HashSet::with_capacity(channels.len());
        let mut normalized = Vec::with_capacity(channels.len());
        for (twitch_id, login) in channels {
            let login = login.trim().to_ascii_lowercase();
            ensure!(twitch_id > 0, "live channel IDs must be positive");
            ensure!(!login.is_empty(), "live channel logins cannot be empty");
            ensure!(seen.insert(twitch_id), "duplicate live channel {twitch_id}");
            normalized.push((twitch_id, login));
        }
        self.write(move |tx| {
            tx.execute("DELETE FROM legacy_live", [])?;
            let mut updated = 0usize;
            for (twitch_id, login) in normalized {
                let eligible: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM users u WHERE u.twitch_id=?1 AND json_extract(u.settings_json,'$.enable')=1 AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=u.twitch_id))", [twitch_id], |r| r.get(0))?;
                if !eligible { continue; }
                tx.execute("UPDATE users SET twitch_username=?1 WHERE twitch_id=?2", params![login, twitch_id])?;
                tx.execute("INSERT INTO legacy_live(twitch_id,login,updated_at) VALUES(?1,?2,unixepoch())", params![twitch_id, login])?;
                updated += 1;
            }
            Ok(updated)
        }).await
    }
    pub async fn revoke_twitch_channel(&self, twitch_id: i64) -> Result<()> {
        self.write_notifying(move |tx| {
            tx.execute("UPDATE twitch_channel_grants SET status='revoked',revision=revision+1 WHERE twitch_id=?1 AND status='granted'", [twitch_id])?;
            users::refresh_eligibility(tx, twitch_id)
        }).await
    }
    pub(crate) async fn revoke_twitch_channel_if_current(
        &self,
        twitch_id: i64,
        revision: i64,
    ) -> Result<bool> {
        self.write_notifying(move |tx| {
            let changed = tx.execute(
                "UPDATE twitch_channel_grants SET status='revoked',revision=revision+1 WHERE twitch_id=?1 AND revision=?2 AND status='granted'",
                params![twitch_id, revision],
            )? == 1;
            if changed {
                users::refresh_eligibility(tx, twitch_id)?;
            }
            Ok(changed)
        }).await
    }
    pub async fn live_users(&self, limit: i64, offset: i64) -> Result<Vec<String>> {
        sql::pagination(limit, offset)?;
        self.call(move |c| sql::column(c, "SELECT login FROM (SELECT twitch_username AS login FROM users WHERE is_live=1 UNION SELECT l.login AS login FROM legacy_live l JOIN users u ON u.twitch_id=l.twitch_id WHERE l.updated_at>=unixepoch()-180 AND json_extract(u.settings_json,'$.enable')=1 AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=l.twitch_id)) ORDER BY login LIMIT ?1 OFFSET ?2", params![limit, offset])).await
    }
}
