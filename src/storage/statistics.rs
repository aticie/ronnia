//! Request statistics: recording delivered requests, the beatmap leaderboard, and
//! the backlog of beatmaps those statistics reference but never cached.
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

use super::{Store, sql};
use crate::domain::RequestEvent;

impl Store {
    pub async fn record_request(&self, event: RequestEvent) -> Result<()> {
        event.validate()?;
        self.call(move |c| insert(c, &event, None)).await
    }
    pub async fn leaderboard(&self, since: i64, limit: i64, offset: i64) -> Result<Vec<Value>> {
        sql::pagination(limit, offset)?;
        self.call(move |c| {
            let rows = sql::all(c, "SELECT s.requested_beatmap_id,COUNT(*) AS total,b.raw_json FROM statistics s LEFT JOIN beatmaps b ON b.id=s.requested_beatmap_id WHERE s.timestamp>=?1 GROUP BY s.requested_beatmap_id ORDER BY total DESC,s.requested_beatmap_id ASC LIMIT ?2 OFFSET ?3", params![since, limit, offset], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?)))?;
            rows.into_iter().map(|(id, count, raw)| {
                let mut object = sql::document(raw.as_deref())?;
                object.insert("_id".into(), json!(id));
                object.insert("count".into(), json!(count));
                Ok(Value::Object(object))
            }).collect()
        }).await
    }
    pub async fn missing_beatmap_ids(&self, after: i64, limit: i64) -> Result<Vec<i64>> {
        ensure!(
            (1..=1000).contains(&limit),
            "limit must be between 1 and 1000"
        );
        self.call(move |c| sql::column(c, "SELECT DISTINCT requested_beatmap_id FROM statistics s WHERE requested_beatmap_id>?1 AND NOT EXISTS(SELECT 1 FROM beatmaps b WHERE b.id=s.requested_beatmap_id) ORDER BY requested_beatmap_id LIMIT ?2", params![after, limit])).await
    }
}

/// Insert one delivered request. A `source` makes the row idempotent, which is
/// what keeps legacy reports from counting the same request twice; NULL sources
/// never conflict, so plain recordings stay unconstrained.
pub(super) fn insert(
    conn: &Connection,
    event: &RequestEvent,
    source: Option<String>,
) -> Result<()> {
    conn.execute("INSERT INTO statistics(source_id,requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(source_id) DO NOTHING", params![source, event.requester_channel_name, event.requested_beatmap_id, event.requested_channel_name, event.mods, event.timestamp])?;
    Ok(())
}
