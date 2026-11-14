//! User accounts: identity lookups, sign-up persistence, settings and exclusion
//! edits, legacy channel selection, and the derived eligibility flag.
use anyhow::{Context, Result, ensure};
use rusqlite::types::Value::{self, Integer, Text};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value as Json;

use super::{Store, sql};
use crate::domain::{Settings, User};

/// One active channel grant and whether chat creation is waiting for an unban probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelGrant {
    pub twitch_id: i64,
    /// Persisted ban shown by the API, including while an unban probe is due.
    pub chat_banned: bool,
    /// Whether creation is currently waiting for the next hourly probe.
    pub chat_blocked: bool,
}

/// Columns every user read decodes, in `decode` order.
const COLUMNS: &str = "osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json";
/// Enabled accounts with no grant row at all: the legacy relay may still serve them.
const LEGACY: &str = "json_extract(u.settings_json,'$.enable')=1 AND NOT EXISTS(SELECT 1 FROM twitch_channel_grants g WHERE g.twitch_id=u.twitch_id)";
const BY_LOGIN: &str =
    "twitch_username=?1 COLLATE NOCASE ORDER BY is_live DESC,twitch_id DESC LIMIT 1";
const USER_EXISTS: &str = "SELECT EXISTS(SELECT 1 FROM users WHERE twitch_id=?1)";

fn decode(row: &Row<'_>) -> rusqlite::Result<User> {
    let settings: String = row.get(7)?;
    Ok(User {
        osu_id: row.get(0)?,
        osu_username: row.get(1)?,
        osu_avatar_url: row.get(2)?,
        twitch_id: row.get(3)?,
        twitch_username: row.get(4)?,
        twitch_avatar_url: row.get(5)?,
        excluded_users: Vec::new(),
        is_live: row.get(6)?,
        settings: serde_json::from_str(&settings).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, error.into())
        })?,
    })
}

/// Read the first user matching `predicate` (`?1` is the bound key).
fn load(conn: &Connection, predicate: &str, key: Value) -> Result<Option<User>> {
    let statement = format!("SELECT {COLUMNS} FROM users u WHERE {predicate}");
    let row = conn.query_row(&statement, [key], decode).optional()?;
    let Some(mut user) = row else { return Ok(None) };
    user.excluded_users = sql::column(
        conn,
        "SELECT login FROM exclusions WHERE twitch_id=?1 ORDER BY login",
        [user.twitch_id],
    )?;
    Ok(Some(user))
}

/// Same projection, restricted to accounts the legacy relay may still serve.
fn load_legacy(conn: &Connection, predicate: &str, key: Value) -> Result<Option<User>> {
    load(conn, &format!("{LEGACY} AND {predicate} LIMIT 1"), key)
}

/// Merge a settings patch over the stored document, keeping unknown keys.
fn merged_settings(current: &str, settings: Settings) -> Result<String> {
    let mut merged: Json = serde_json::from_str(current)?;
    let incoming = serde_json::to_value(settings)?;
    merged
        .as_object_mut()
        .context("stored settings are not an object")?
        .extend(
            incoming
                .as_object()
                .context("settings serialization failed")?
                .clone(),
        );
    Ok(merged.to_string())
}

fn require_user(tx: &Connection, twitch_id: i64) -> Result<()> {
    let exists: bool = sql::optional(tx, USER_EXISTS, [twitch_id])?.unwrap_or(false);
    ensure!(exists, "user not found");
    Ok(())
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

impl Store {
    async fn read(&self, predicate: &'static str, key: Value) -> Result<Option<User>> {
        self.call(move |c| load(c, predicate, key)).await
    }
    async fn read_legacy(&self, predicate: &'static str, key: Value) -> Result<Option<User>> {
        self.call(move |c| load_legacy(c, predicate, key)).await
    }
    pub async fn user_by_twitch(&self, id: i64) -> Result<Option<User>> {
        self.read("twitch_id=?1", Integer(id)).await
    }
    pub async fn user_by_osu(&self, id: i64) -> Result<Option<User>> {
        self.read("osu_id=?1", Integer(id)).await
    }
    pub async fn user_by_login(&self, login: &str) -> Result<Option<User>> {
        self.read(BY_LOGIN, Text(login.to_owned())).await
    }
    pub async fn enabled_twitch_ids(&self) -> Result<Vec<i64>> {
        self.call(|c| sql::column(c, "SELECT twitch_id FROM users WHERE json_extract(settings_json,'$.enable') = 1 ORDER BY twitch_id", [])).await
    }
    /// Enabled channels with verified broadcaster consent, past the migration grace window.
    pub async fn granted_twitch_ids(&self) -> Result<Vec<i64>> {
        Ok(self
            .granted_twitch_channels()
            .await?
            .into_iter()
            .map(|grant| grant.twitch_id)
            .collect())
    }
    /// The same grant snapshot with chat-ban probes included. State subscriptions
    /// remain desired while a channel's chat subscription waits for an unban.
    pub async fn granted_twitch_channels(&self) -> Result<Vec<ChannelGrant>> {
        self.call(|c| sql::all(c, "SELECT u.twitch_id,g.chat_banned_at IS NOT NULL,g.chat_banned_at IS NOT NULL AND g.chat_retry_at>unixepoch() FROM users u JOIN twitch_channel_grants g ON g.twitch_id=u.twitch_id WHERE json_extract(u.settings_json,'$.enable')=1 AND g.status='granted' AND g.active_at<=unixepoch() ORDER BY u.twitch_id", [], |row| Ok(ChannelGrant { twitch_id: row.get(0)?, chat_banned: row.get(1)?, chat_blocked: row.get(2)? }))).await
    }
    /// Enabled channels that never completed Twitch channel:bot consent.
    pub async fn legacy_channels(&self, after: i64, limit: i64) -> Result<Vec<User>> {
        ensure!(
            (1..=100).contains(&limit),
            "limit must be between 1 and 100"
        );
        self.call(move |c| {
            let ids: Vec<i64> = sql::column(c, &format!("SELECT u.twitch_id FROM users u WHERE u.twitch_id>?1 AND {LEGACY} ORDER BY u.twitch_id LIMIT ?2"), params![after, limit])?;
            ids.into_iter().map(|id| load_legacy(c, "u.twitch_id=?1", Integer(id))?.context("legacy channel disappeared during read")).collect()
        }).await
    }
    pub async fn legacy_user_by_login(&self, login: &str) -> Result<Option<User>> {
        self.read_legacy(
            "u.twitch_username=?1 COLLATE NOCASE",
            Text(login.to_owned()),
        )
        .await
    }
    pub async fn legacy_user_by_twitch(&self, id: i64) -> Result<Option<User>> {
        self.read_legacy("u.twitch_id=?1", Integer(id)).await
    }
    pub async fn save_user(&self, mut user: User) -> Result<()> {
        user.validate()?;
        self.write_notifying(move |tx| {
            let existing: Option<i64> = sql::optional(tx, "SELECT twitch_id FROM users WHERE osu_id=?1", [user.osu_id])?;
            ensure!(existing.is_none_or(|twitch_id| twitch_id == user.twitch_id), "osu account is already linked to another Twitch account");
            tx.execute("INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(osu_id) DO UPDATE SET osu_username=excluded.osu_username,osu_avatar_url=excluded.osu_avatar_url,twitch_username=excluded.twitch_username,twitch_avatar_url=excluded.twitch_avatar_url",
                params![user.osu_id, user.osu_username, user.osu_avatar_url, user.twitch_id, user.twitch_username, user.twitch_avatar_url, false, serde_json::to_string(&user.settings)?])?;
            if existing.is_none() {
                for login in user.excluded_users { tx.execute("INSERT INTO exclusions(twitch_id,login) VALUES(?1,?2)", params![user.twitch_id, login])?; }
            }
            refresh_eligibility(tx, user.twitch_id)
        }).await
    }
    pub async fn delete_user(&self, twitch_id: i64) -> Result<()> {
        self.write_notifying(move |tx| {
            tx.execute("DELETE FROM users WHERE twitch_id=?1", [twitch_id])?;
            Ok(())
        })
        .await
    }
    pub async fn set_settings(&self, twitch_id: i64, mut settings: Settings) -> Result<()> {
        settings.validate()?;
        self.write_notifying(move |tx| {
            let current: String = sql::optional(
                tx,
                "SELECT settings_json FROM users WHERE twitch_id=?1",
                [twitch_id],
            )?
            .context("user not found")?;
            tx.execute(
                "UPDATE users SET settings_json=?1 WHERE twitch_id=?2",
                params![merged_settings(&current, settings)?, twitch_id],
            )?;
            refresh_eligibility(tx, twitch_id)
        })
        .await
    }
    pub async fn set_excluded(&self, twitch_id: i64, login: String, excluded: bool) -> Result<()> {
        let login = login.to_lowercase();
        ensure!(!login.trim().is_empty(), "excluded login cannot be empty");
        self.write(move |tx| {
            require_user(tx, twitch_id)?;
            let statement = if excluded {
                "INSERT INTO exclusions(twitch_id,login) VALUES(?1,?2) ON CONFLICT DO NOTHING"
            } else {
                "DELETE FROM exclusions WHERE twitch_id=?1 AND login=?2"
            };
            tx.execute(statement, params![twitch_id, login])?;
            Ok(())
        })
        .await
    }
}
