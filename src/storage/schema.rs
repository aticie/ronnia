//! Schema bootstrap: connection pragmas, forward-only migrations, the settings
//! metadata catalog, and recovery of deliveries interrupted by a process stop.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

use super::{Store, sql};

const MIGRATIONS: &[(i64, &str)] = &[
    (2, include_str!("../../migrations/002_modern_twitch.sql")),
    (3, include_str!("../../migrations/003_coexistence.sql")),
    (4, include_str!("../../migrations/004_legacy_live.sql")),
];
const SCHEMA_VERSION: i64 = 4;

pub(super) fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut connection = Connection::open(path).context("opening SQLite database")?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA temp_store=FILE;")?;
    initialize(&mut connection)?;
    Ok(connection)
}

fn initialize(c: &mut Connection) -> Result<()> {
    let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        version <= SCHEMA_VERSION,
        "database schema version {version} is newer than this application"
    );
    let tx = c.transaction()?;
    if version == 0 {
        tx.execute_batch(include_str!("../../migrations/001_initial.sql"))?;
    }
    for (target, migration) in MIGRATIONS.iter().filter(|(target, _)| version < *target) {
        tx.execute_batch(migration)?;
        tx.pragma_update(None, "user_version", *target)?;
    }
    for document in default_metadata() {
        tx.execute("INSERT INTO settings_metadata(name,value_json,description,type,raw_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(name) DO NOTHING", params![document["name"].as_str().unwrap(), document["value"].to_string(), document["description"].as_str().unwrap(), document["type"].as_str().unwrap(), document.to_string()])?;
    }
    tx.execute("UPDATE deliveries SET status='uncertain',error=COALESCE(error,'process stopped during delivery; not replayed') WHERE status='sending'", [])?;
    tx.commit()?;
    Ok(())
}

impl Store {
    pub async fn settings_metadata(&self) -> Result<Vec<Value>> {
        self.call(|c| {
            let rows = sql::all(c, "SELECT name,value_json,description,type,raw_json FROM settings_metadata ORDER BY rowid", [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?;
            rows.into_iter().map(|(name, value, description, kind, raw)| {
                let mut object = sql::document(Some(&raw))?;
                object.insert("name".into(), json!(name));
                object.insert("value".into(), serde_json::from_str(&value)?);
                object.insert("description".into(), json!(description));
                object.insert("type".into(), json!(kind));
                Ok(Value::Object(object))
            }).collect()
        }).await
    }
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
