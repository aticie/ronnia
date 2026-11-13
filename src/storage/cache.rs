//! Beatmap and beatmapset JSON cache: a one-day freshness window over rows that
//! are retained past expiry for historical joins but never served as fresh.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::Value;

use super::{Store, sql};

const FRESHNESS: i64 = 86_400;

fn cutoff() -> i64 {
    chrono::Utc::now().timestamp() - FRESHNESS
}

fn decode(raw: Option<String>) -> Result<Option<Value>> {
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

impl Store {
    pub async fn cache_get(&self, id: i64) -> Result<Option<Value>> {
        self.call(move |c| {
            decode(sql::optional(
                c,
                "SELECT raw_json FROM beatmaps WHERE id=?1 AND fetched_at>?2",
                params![id, cutoff()],
            )?)
        })
        .await
    }
    pub async fn cache_put(&self, value: Value) -> Result<()> {
        self.call(move |c| {
            put(c, &value, chrono::Utc::now().timestamp())?;
            Ok(())
        })
        .await
    }
    pub async fn cache_get_set(&self, set_id: i64) -> Result<Option<Value>> {
        self.call(move |c| decode(sql::optional(c, "SELECT b.raw_json FROM mapset_cache s JOIN beatmaps b ON b.id=s.beatmap_id AND b.beatmapset_id=s.set_id WHERE s.set_id=?1 AND s.fetched_at>?2 AND b.fetched_at>?2", params![set_id, cutoff()])?)).await
    }
    pub async fn cache_put_set(&self, set_id: i64, value: Value) -> Result<()> {
        ensure!(set_id > 0, "beatmapset ID must be positive");
        self.write(move |tx| {
            let fetched_at = chrono::Utc::now().timestamp();
            let (beatmap_id, actual_set_id) = put(tx, &value, fetched_at)?;
            ensure!(actual_set_id == set_id, "selected beatmap does not belong to requested set");
            tx.execute("INSERT INTO mapset_cache(set_id,beatmap_id,fetched_at) VALUES(?1,?2,?3) ON CONFLICT(set_id) DO UPDATE SET beatmap_id=excluded.beatmap_id,fetched_at=excluded.fetched_at", params![set_id, beatmap_id, fetched_at])?;
            Ok(())
        }).await
    }
}

fn put(connection: &Connection, value: &Value, fetched_at: i64) -> Result<(i64, i64)> {
    let id = value
        .get("id")
        .and_then(Value::as_i64)
        .context("beatmap id must be an integer")?;
    let set = value
        .get("beatmapset_id")
        .and_then(Value::as_i64)
        .context("beatmapset_id must be an integer")?;
    ensure!(id > 0 && set > 0, "beatmap IDs must be positive");
    connection.execute("INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET beatmapset_id=excluded.beatmapset_id,fetched_at=excluded.fetched_at,raw_json=excluded.raw_json", params![id, set, fetched_at, value.to_string()])?;
    Ok((id, set))
}
