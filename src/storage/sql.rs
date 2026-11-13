//! Shared SQLite plumbing: result collection, optional scalars, JSON document
//! decoding, and the pagination bounds every list endpoint enforces.
use anyhow::{Result, anyhow, ensure};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Params, Row};
use serde_json::{Map, Value};

pub(super) fn all<T, F>(c: &Connection, sql: &str, p: impl Params, map: F) -> Result<Vec<T>>
where
    F: FnMut(&Row<'_>) -> rusqlite::Result<T>,
{
    let mut statement = c.prepare(sql)?;
    Ok(statement
        .query_map(p, map)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(super) fn column<T: FromSql>(c: &Connection, sql: &str, p: impl Params) -> Result<Vec<T>> {
    all(c, sql, p, |row| row.get(0))
}

pub(super) fn optional<T: FromSql>(c: &Connection, sql: &str, p: impl Params) -> Result<Option<T>> {
    Ok(c.query_row(sql, p, |row| row.get(0)).optional()?)
}

pub(super) fn document(raw: Option<&str>) -> Result<Map<String, Value>> {
    let value: Value = match raw {
        Some(raw) => serde_json::from_str(raw)?,
        None => Value::Object(Map::new()),
    };
    let Value::Object(mut object) = value else {
        return Err(anyhow!("stored document is not an object"));
    };
    object.remove("_id");
    Ok(object)
}

pub(super) fn pagination(limit: i64, offset: i64) -> Result<()> {
    ensure!(
        (1..=100).contains(&limit) && offset >= 0,
        "invalid pagination"
    );
    Ok(())
}
