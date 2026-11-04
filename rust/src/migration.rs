//! Offline MongoDB export import. Every collection is streamed into a temporary SQLite
//! staging table; validation errors leave the persistent database entirely unchanged.
use crate::{
    domain::{RequestEvent, Settings, User},
    storage::{Store, default_metadata},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeSeed, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

const COLLECTIONS: [&str; 4] = ["Users", "Settings", "Statistics", "Beatmaps"];
const MAX_ISSUES: usize = 1000;
const MAX_BSON_DOCUMENT: usize = 16 * 1024 * 1024;

#[derive(Debug, Serialize, Default)]
pub struct CollectionReport {
    pub seen: u64,
    pub valid: u64,
    pub invalid: u64,
}
#[derive(Debug, Serialize)]
pub struct ImportIssue {
    pub collection: String,
    pub record: Option<u64>,
    pub message: String,
}
#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub committed: bool,
    pub dry_run: bool,
    pub collections: BTreeMap<String, CollectionReport>,
    pub issue_count: u64,
    pub issues: Vec<ImportIssue>,
    pub issues_truncated: bool,
    pub notes: Vec<String>,
}
impl ImportReport {
    fn new(dry_run: bool) -> Self {
        Self {committed:false,dry_run,collections:COLLECTIONS.into_iter().map(|name|(name.to_owned(),CollectionReport::default())).collect(),issue_count:0,issues:Vec::new(),issues_truncated:false,
            notes:vec!["Twitch logins and exclusions are normalized to lowercase; duplicate exclusions are collapsed. Original documents remain in raw_json.".into(),
            "Star upper bounds >=10 become -1 (unlimited), matching the existing API; source settings and unknown customization fields remain in raw_json.".into(),
            "Timestamps are stored as UTC epoch seconds; source subsecond precision remains in raw_json. Statistics without Mongo _id retain every occurrence and receive export SHA256+ordinal identities to reject replay of the same ordered export.".into()]}
    }
    fn issue(&mut self, collection: &str, record: Option<u64>, message: impl Into<String>) {
        self.issue_count += 1;
        if self.issues.len() < MAX_ISSUES {
            self.issues.push(ImportIssue {
                collection: collection.into(),
                record,
                message: message.into(),
            });
        } else {
            self.issues_truncated = true;
        }
    }
}

pub async fn import_exports(store: Store, directory: &Path) -> Result<ImportReport> {
    let directory = directory.to_path_buf();
    let report = store
        .call(move |connection| import(connection, &directory, false))
        .await?;
    if report.committed {
        store.notify_channels_changed();
    }
    Ok(report)
}
pub async fn validate_exports(store: Store, directory: &Path) -> Result<ImportReport> {
    let directory = directory.to_path_buf();
    store
        .call(move |connection| import(connection, &directory, true))
        .await
}

fn import(connection: &mut Connection, directory: &Path, dry_run: bool) -> Result<ImportReport> {
    let mut report = ImportReport::new(dry_run);
    let files = discover(directory, &mut report)?;
    let transaction = connection.transaction()?;
    transaction.execute_batch("DROP TABLE IF EXISTS temp.import_records;
        CREATE TEMP TABLE import_records(collection TEXT NOT NULL,ordinal INTEGER NOT NULL,source_id TEXT,key1 TEXT,key2 TEXT,canonical TEXT NOT NULL,raw_json TEXT NOT NULL,PRIMARY KEY(collection,ordinal));
        CREATE UNIQUE INDEX temp.import_source ON import_records(collection,source_id) WHERE source_id IS NOT NULL;
        CREATE UNIQUE INDEX temp.import_key1 ON import_records(collection,key1) WHERE key1 IS NOT NULL;
        CREATE UNIQUE INDEX temp.import_key2 ON import_records(collection,key2) WHERE key2 IS NOT NULL;")?;
    for collection in COLLECTIONS {
        if let Some(path) = files.get(collection) {
            let mut ordinal = 0u64;
            let outcome = stream_file(path, |raw| {
                ordinal += 1;
                report.collections.get_mut(collection).unwrap().seen += 1;
                match stage(&transaction, collection, ordinal, raw) {
                    Ok(()) => report.collections.get_mut(collection).unwrap().valid += 1,
                    Err(error) => {
                        report.collections.get_mut(collection).unwrap().invalid += 1;
                        report.issue(collection, Some(ordinal), format!("{error:#}"));
                    }
                }
            });
            if let Err(error) = outcome {
                let counts = report.collections.get_mut(collection).unwrap();
                counts.seen += 1;
                counts.invalid += 1;
                report.issue(
                    collection,
                    Some(ordinal + 1),
                    format!(
                        "{}: {error:#}; parsing stopped, remaining records were not imported",
                        path.display()
                    ),
                );
            }
        }
    }
    assign_statistic_source_ids(&transaction)?;
    validate_existing(&transaction, &mut report)?;
    if report.issue_count == 0 && !dry_run {
        commit_staged(&transaction)?;
        transaction.execute_batch("DROP TABLE temp.import_records;")?;
        transaction.commit()?;
        report.committed = true;
    }
    // Dropping the transaction rolls back staging and every data mutation on errors/dry run.
    Ok(report)
}
/// An export-level digest plus ordinal keeps legitimately identical events distinct,
/// while making replay of the same ordered ID-less export a persistent conflict.
fn assign_statistic_source_ids(connection: &Connection) -> Result<()> {
    let mut digest = Sha256::new();
    {
        let mut statement=connection.prepare("SELECT raw_json FROM temp.import_records WHERE collection='Statistics' ORDER BY ordinal")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            digest.update((raw.len() as u64).to_le_bytes());
            digest.update(raw.as_bytes());
        }
    }
    let prefix = format!("ronnia:idless:{}", hex::encode(digest.finalize()));
    connection.execute("UPDATE temp.import_records SET source_id=?1||':'||ordinal WHERE collection='Statistics' AND source_id IS NULL",[prefix])?;
    Ok(())
}

fn discover(directory: &Path, report: &mut ImportReport) -> Result<BTreeMap<String, PathBuf>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(directory)
        .with_context(|| format!("reading export directory {}", directory.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // mongodump metadata contains indexes/options, not collection records.
        if name.ends_with(".metadata.json") {
            continue;
        }
        let Some((stem, extension)) = name.rsplit_once('.') else {
            continue;
        };
        if !["json", "jsonl", "ndjson", "bson"].contains(&extension) {
            if name.ends_with(".gz") {
                report.issue(
                    "exports",
                    None,
                    format!("compressed export {name} must be decompressed before import"),
                );
            }
            continue;
        }
        if !COLLECTIONS.contains(&stem) {
            report.issue("exports",None,format!("unknown collection export {name}; expected exact names Users, Settings, Statistics, Beatmaps"));
            continue;
        }
        if files.insert(stem.to_owned(), path.clone()).is_some() {
            report.issue(
                stem,
                None,
                format!("multiple export files for {stem}; keep exactly one format"),
            );
        }
    }
    for name in COLLECTIONS {
        if !files.contains_key(name) {
            report.issue(name,None,format!("missing {name}.json/.jsonl/.ndjson/.bson export (an empty collection must have an empty export)"));
        }
    }
    Ok(files)
}

/// Reject duplicate keys before Value/Document can collapse them.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON without duplicate object keys")
            }
            fn visit_bool<E: serde::de::Error>(
                self,
                v: bool,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|v| StrictValue(Value::Number(v)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.into())))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(v) = seq.next_element::<StrictValue>()? {
                    values.push(v.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut object = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if object.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object key {key}"
                        )));
                    }
                    object.insert(key, map.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(object)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

fn validate_raw_bson(document: &bson::raw::RawDocument, depth: usize) -> Result<()> {
    ensure!(depth <= 128, "BSON nesting exceeds 128 levels");
    let mut names = std::collections::HashSet::new();
    for element in document.iter() {
        let (name, value) = element?;
        ensure!(names.insert(name), "duplicate BSON field {name}");
        validate_raw_bson_value(value, depth + 1)?;
    }
    Ok(())
}
fn validate_raw_bson_value(value: bson::raw::RawBsonRef<'_>, depth: usize) -> Result<()> {
    ensure!(depth <= 128, "BSON nesting exceeds 128 levels");
    match value {
        bson::raw::RawBsonRef::Document(document) => validate_raw_bson(document, depth)?,
        bson::raw::RawBsonRef::Array(array) => {
            for value in array {
                validate_raw_bson_value(value?, depth + 1)?;
            }
        }
        bson::raw::RawBsonRef::JavaScriptCodeWithScope(code) => {
            validate_raw_bson(code.scope, depth)?
        }
        _ => {}
    }
    Ok(())
}

struct ArraySeed<'a, F>(&'a mut F);
impl<'de, F: FnMut(Value)> DeserializeSeed<'de> for ArraySeed<'_, F> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<(), D::Error> {
        struct ArrayVisitor<'a, F>(&'a mut F);
        impl<'de, F: FnMut(Value)> Visitor<'de> for ArrayVisitor<'_, F> {
            type Value = ();
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of Mongo documents")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
                while let Some(value) = seq.next_element::<StrictValue>()? {
                    (self.0)(value.0);
                }
                Ok(())
            }
        }
        deserializer.deserialize_seq(ArrayVisitor(self.0))
    }
}
fn stream_file(path: &Path, mut consume: impl FnMut(Value)) -> Result<()> {
    let mut reader = BufReader::new(File::open(path)?);
    if path.extension().and_then(|s| s.to_str()) == Some("bson") {
        loop {
            if reader.fill_buf()?.is_empty() {
                break;
            }
            let mut header = [0u8; 4];
            reader
                .read_exact(&mut header)
                .context("truncated BSON header")?;
            let length = i32::from_le_bytes(header);
            ensure!(
                (5..=MAX_BSON_DOCUMENT as i32).contains(&length),
                "BSON document length {length} outside 5..16MiB"
            );
            let mut bytes = vec![0u8; length as usize];
            bytes[..4].copy_from_slice(&header);
            reader
                .read_exact(&mut bytes[4..])
                .context("truncated BSON document")?;
            validate_raw_bson(bson::raw::RawDocument::from_bytes(&bytes)?, 0)?;
            let document = bson::Document::from_reader(&mut std::io::Cursor::new(bytes))
                .context("invalid BSON document")?;
            consume(bson::Bson::Document(document).into_relaxed_extjson());
        }
        return Ok(());
    }
    loop {
        let buffer = reader.fill_buf()?;
        let count = buffer
            .iter()
            .take_while(|b| b.is_ascii_whitespace())
            .count();
        if count == 0 {
            break;
        }
        reader.consume(count);
    }
    if reader.fill_buf()?.first() == Some(&b'[') {
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        ArraySeed(&mut consume).deserialize(&mut deserializer)?;
        deserializer.end()?;
    } else {
        for value in serde_json::Deserializer::from_reader(reader).into_iter::<StrictValue>() {
            consume(value?.0);
        }
    }
    Ok(())
}

struct Staged {
    key1: Option<String>,
    key2: Option<String>,
    canonical: Value,
}
fn stage(connection: &Connection, collection: &str, ordinal: u64, raw: Value) -> Result<()> {
    ensure!(raw.is_object(), "record must be a document");
    let source_id = raw.get("_id").map(source_identity).transpose()?;
    let raw_json = raw.to_string();
    let normalized = normalize_extended(raw)?;
    let staged = match collection {
        "Users" => {
            let mut user_value = normalized;
            normalize_id_alias(&mut user_value, &["osuId", "osu_id"], "osuId")?;
            normalize_id_alias(&mut user_value, &["twitchId", "twitch_id"], "twitchId")?;
            let mut user = User::deserialize(&user_value).context("invalid user")?;
            user.validate()?;
            let mut canonical = serde_json::to_value(&user)?;
            // Unknown user-setting keys are retained, not silently discarded by typed deserialization.
            if let Some(custom) = user_value.get("settings").and_then(Value::as_object) {
                let settings = canonical["settings"].as_object_mut().unwrap();
                for (key, value) in custom {
                    if !["sub_only", "subOnly", "points_only", "pointsOnly"].contains(&key.as_str())
                    {
                        settings.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            Staged {
                key1: Some(user.osu_id.to_string()),
                key2: Some(user.twitch_id.to_string()),
                canonical,
            }
        }
        "Settings" => {
            let object = normalized.as_object().unwrap();
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .context("setting name must be a string")?;
            ensure!(!name.is_empty(), "setting name cannot be empty");
            let value = object
                .get("value")
                .or_else(|| object.get("default_value"))
                .or_else(|| object.get("defaultValue"))
                .context("setting value missing")?;
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .context("setting type must be a string")?;
            let description = object
                .get("description")
                .map(|v| v.as_str().context("setting description must be a string"))
                .transpose()?
                .unwrap_or("");
            // Metadata can include custom setting names/types, but known defaults must be executable.
            if [
                "echo",
                "enable",
                "sub-only",
                "points-only",
                "test",
                "cooldown",
                "sr",
            ]
            .contains(&name)
            {
                let mut candidate = json!({});
                candidate[name] = value.clone();
                let mut settings: Settings =
                    serde_json::from_value(candidate).context("invalid setting default")?;
                settings.validate()?;
            }
            Staged {
                key1: Some(name.to_owned()),
                key2: None,
                canonical: json!({"name":name,"value":value,"description":description,"type":kind}),
            }
        }
        "Statistics" => {
            let mut value = normalized;
            normalize_id_alias(
                &mut value,
                &["requested_beatmap_id", "requestedBeatmapId"],
                "requested_beatmap_id",
            )?;
            value["timestamp"] = json!(date_seconds(
                value
                    .get("timestamp")
                    .context("statistics timestamp missing")?
            )?);
            let event: RequestEvent = serde_json::from_value(value).context("invalid statistic")?;
            event.validate()?;
            Staged {
                key1: None,
                key2: None,
                canonical: serde_json::to_value(event)?,
            }
        }
        "Beatmaps" => {
            let id = integer(normalized.get("id").context("beatmap id missing")?)?;
            let set = integer(
                normalized
                    .get("beatmapset_id")
                    .or_else(|| normalized.get("beatmapsetId"))
                    .or_else(|| normalized.get("beatmapset").and_then(|v| v.get("id")))
                    .context("beatmapset_id missing")?,
            )?;
            ensure!(id > 0 && set > 0, "beatmap IDs must be positive");
            let fetched_at=date_seconds(normalized.get("ronnia_updated_at").or_else(||normalized.get("fetched_at")).or_else(||normalized.get("fetchedAt")).context("beatmap ronnia_updated_at/fetched_at missing; refusing to invent freshness")?)?;
            let mut metadata = normalized;
            let object = metadata.as_object_mut().unwrap();
            object.remove("_id");
            object.insert("id".into(), json!(id));
            object.insert("beatmapset_id".into(), json!(set));
            Staged {
                key1: Some(id.to_string()),
                key2: None,
                canonical: json!({"id":id,"beatmapset_id":set,"fetched_at":fetched_at,"metadata":metadata}),
            }
        }
        _ => bail!("unknown collection"),
    };
    connection.execute("INSERT INTO temp.import_records(collection,ordinal,source_id,key1,key2,canonical,raw_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![collection,ordinal as i64,source_id,staged.key1,staged.key2,staged.canonical.to_string(),raw_json]).context("duplicate Mongo _id, account identity, setting name, or beatmap ID in export")?;
    Ok(())
}

fn normalize_id_alias(value: &mut Value, aliases: &[&str], canonical: &str) -> Result<()> {
    let object = value.as_object_mut().context("expected object")?;
    let mut result = None;
    for alias in aliases {
        if let Some(value) = object.get(*alias) {
            let id = integer(value)?;
            if let Some(previous) = result {
                ensure!(previous == id, "conflicting aliases for {canonical}");
            }
            result = Some(id);
        }
    }
    let id = result.with_context(|| format!("{canonical} missing"))?;
    for alias in aliases {
        object.remove(*alias);
    }
    object.insert(canonical.into(), json!(id));
    Ok(())
}
fn integer(value: &Value) -> Result<i64> {
    if let Some(n) = value.as_i64() {
        return Ok(n);
    }
    if let Some(s) = value.as_str() {
        return s
            .parse()
            .context("integer string outside signed 64-bit range");
    }
    if let Some(object) = value.as_object()
        && let Some(inner) = object
            .get("$numberLong")
            .or_else(|| object.get("$numberInt"))
    {
        return integer(inner);
    }
    if let Some(n) = value.as_f64() {
        ensure!(
            n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n < (i64::MAX as f64),
            "ID must be an exact signed 64-bit integer"
        );
        return Ok(n as i64);
    }
    bail!("expected integer or decimal integer string")
}
fn source_identity(value: &Value) -> Result<String> {
    // Canonical JSON preserves Mongo identity type (string 1 is not number 1).
    match value {
        Value::Null => bail!("Mongo _id cannot be null"),
        Value::Object(object) if object.contains_key("$oid") => {
            let oid = object["$oid"].as_str().context("$oid must be a string")?;
            ensure!(
                oid.len() == 24 && oid.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid ObjectId"
            );
            Ok(json!({"$oid":oid.to_lowercase()}).to_string())
        }
        _ => Ok(normalize_extended(value.clone())?.to_string()),
    }
}
fn normalize_extended(value: Value) -> Result<Value> {
    match value {
        Value::Array(array) => Ok(Value::Array(
            array
                .into_iter()
                .map(normalize_extended)
                .collect::<Result<_>>()?,
        )),
        Value::Object(mut object) => {
            if object.len() == 1 {
                if let Some(n) = object
                    .get("$numberLong")
                    .or_else(|| object.get("$numberInt"))
                {
                    return Ok(json!(integer(n)?));
                }
                if let Some(n) = object.get("$numberDouble") {
                    let n = n
                        .as_str()
                        .context("$numberDouble must be a string")?
                        .parse::<f64>()?;
                    ensure!(
                        n.is_finite(),
                        "non-finite BSON numeric value cannot be represented by JSON"
                    );
                    return Ok(json!(n));
                }
                // Date, ObjectId, binary, decimal128 and other source types stay lossless Extended JSON.
                if object.keys().next().is_some_and(|key| key.starts_with('$')) {
                    return Ok(Value::Object(object));
                }
            }
            for value in object.values_mut() {
                *value = normalize_extended(std::mem::take(value))?;
            }
            Ok(Value::Object(object))
        }
        other => Ok(other),
    }
}
fn date_seconds(value: &Value) -> Result<i64> {
    let seconds = if let Some(date) = value.as_object().and_then(|object| object.get("$date")) {
        if let Some(string) = date.as_str() {
            parse_date_string(string)?
        } else {
            integer(date)?.div_euclid(1000)
        }
    } else if let Some(string) = value.as_str() {
        if let Ok(seconds) = string.parse::<i64>() {
            seconds
        } else {
            parse_date_string(string)?
        }
    } else {
        integer(value)?
    };
    ensure!(
        chrono::DateTime::from_timestamp(seconds, 0).is_some(),
        "timestamp outside supported UTC range"
    );
    Ok(seconds)
}
fn parse_date_string(value: &str) -> Result<i64> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)
        .context("date must be RFC3339 with timezone")?
        .timestamp())
}

fn validate_existing(connection: &Connection, report: &mut ImportReport) -> Result<()> {
    let mut statement=connection.prepare("SELECT collection,ordinal,key1,key2,source_id,raw_json FROM temp.import_records ORDER BY collection,ordinal")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let collection: String = row.get(0)?;
        let ordinal: i64 = row.get(1)?;
        let key1: Option<String> = row.get(2)?;
        let key2: Option<String> = row.get(3)?;
        let source: Option<String> = row.get(4)?;
        let conflict = match collection.as_str() {
            "Users" => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE osu_id=?1 OR twitch_id=?2)",
                params![key1, key2],
                |r| r.get::<_, bool>(0),
            )?,
            "Beatmaps" => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM beatmaps WHERE id=?1)",
                [key1],
                |r| r.get(0),
            )?,
            "Statistics" => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM statistics WHERE source_id=?1)",
                [source],
                |r| r.get(0),
            )?,
            "Settings" => {
                let current: Option<String> = connection
                    .query_row(
                        "SELECT raw_json FROM settings_metadata WHERE name=?1",
                        [key1],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(current) = current {
                    let existing: Value = serde_json::from_str(&current)?;
                    let raw: Value = serde_json::from_str(&row.get::<_, String>(5)?)?;
                    !default_metadata().contains(&existing) && existing != raw
                } else {
                    false
                }
            }
            _ => false,
        };
        if conflict {
            let counts = report.collections.get_mut(&collection).unwrap();
            counts.valid -= 1;
            counts.invalid += 1;
            report.issue(&collection,Some(ordinal as u64),"conflicts with an existing persistent record; import does not overwrite account/history/cache/custom metadata");
        }
    }
    Ok(())
}

fn commit_staged(connection: &Connection) -> Result<()> {
    // Validation has completed. All persistent inserts are in this one transaction.
    let mut statement=connection.prepare("SELECT collection,source_id,canonical,raw_json FROM temp.import_records ORDER BY collection,ordinal")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let collection: String = row.get(0)?;
        let source: Option<String> = row.get(1)?;
        let canonical: Value = serde_json::from_str(&row.get::<_, String>(2)?)?;
        let raw: String = row.get(3)?;
        match collection.as_str() {
            "Users" => {
                let user = User::deserialize(&canonical)?;
                // Source live flags are provenance only; effective eligibility needs consent and state.
                connection.execute("INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,is_live,settings_json,raw_json) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8)",params![user.osu_id,user.osu_username,user.osu_avatar_url,user.twitch_id,user.twitch_username,user.twitch_avatar_url,canonical["settings"].to_string(),raw])?;
                for login in user.excluded_users {
                    connection.execute(
                        "INSERT INTO exclusions(twitch_id,login) VALUES(?1,?2)",
                        params![user.twitch_id, login],
                    )?;
                }
                crate::storage::refresh_eligibility(connection, user.twitch_id)?;
            }
            "Settings" => {
                connection.execute("INSERT INTO settings_metadata(name,value_json,description,type,raw_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(name) DO UPDATE SET value_json=excluded.value_json,description=excluded.description,type=excluded.type,raw_json=excluded.raw_json",params![canonical["name"].as_str(),canonical["value"].to_string(),canonical["description"].as_str(),canonical["type"].as_str(),raw])?;
            }
            "Statistics" => {
                let event: RequestEvent = serde_json::from_value(canonical)?;
                connection.execute("INSERT INTO statistics(source_id,requester_channel_name,requested_beatmap_id,requested_channel_name,mods,timestamp,raw_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![source,event.requester_channel_name,event.requested_beatmap_id,event.requested_channel_name,event.mods,event.timestamp,raw])?;
            }
            "Beatmaps" => {
                connection.execute("INSERT INTO beatmaps(id,beatmapset_id,fetched_at,raw_json,source_json) VALUES(?1,?2,?3,?4,?5)",params![canonical["id"].as_i64(),canonical["beatmapset_id"].as_i64(),canonical["fetched_at"].as_i64(),canonical["metadata"].to_string(),raw])?;
            }
            _ => bail!("unexpected staged collection"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures(dir: &Path) {
        for name in COLLECTIONS {
            std::fs::write(dir.join(format!("{name}.json")), "[]").unwrap();
        }
    }
    fn write(dir: &Path, name: &str, value: Value) {
        std::fs::write(dir.join(format!("{name}.json")), value.to_string()).unwrap();
    }
    fn sample_user() -> Value {
        json!({"_id":{"$oid":"000000000000000000000001"},"osu_id":{"$numberLong":"10"},"osu_username":"osu","twitch_id":"20","twitch_username":"CHANNEL","is_live":true,"excluded_users":["VIEWER"],"settings":{"echo":false,"cooldown":15,"custom":{"color":"blue"}}})
    }
    #[test]
    fn parses_extended_dates_and_rejects_ambiguous_dates() {
        assert_eq!(
            date_seconds(&json!({"$date":{"$numberLong":"1700000000123"}})).unwrap(),
            1700000000
        );
        assert_eq!(
            date_seconds(&json!({"$date":"2023-11-14T22:13:20Z"})).unwrap(),
            1700000000
        );
        assert_eq!(date_seconds(&json!({"$date":-1})).unwrap(), -1);
        assert!(date_seconds(&json!("2023-11-14 22:13:20")).is_err());
    }
    #[tokio::test]
    async fn atomic_import_retains_settings_metadata_history_and_raw_maps() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        write(dir.path(), "Users", json!([sample_user()]));
        write(
            dir.path(),
            "Settings",
            json!([{"name":"echo","value":false,"description":"Customized","type":"toggle","customField":7}]),
        );
        write(
            dir.path(),
            "Statistics",
            json!([{"_id":{"$oid":"000000000000000000000002"},"requester_channel_name":"viewer","requested_beatmap_id":{"$numberInt":"30"},"requested_channel_name":"channel","timestamp":{"$date":{"$numberLong":"1700000000000"}}}]),
        );
        write(
            dir.path(),
            "Beatmaps",
            json!([{"id":30,"beatmapset_id":31,"version":"Hard","upstream_marker":"retained","ronnia_updated_at":{"$date":"2023-11-14T22:13:20Z"}}]),
        );
        let store = Store::open(":memory:").await.unwrap();
        let mut changes = store.channel_changes();
        let observed = *changes.borrow();
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.committed, "{report:?}");
        tokio::time::timeout(std::time::Duration::from_secs(1), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_ne!(*changes.borrow(), observed);
        let user = store.user_by_twitch(20).await.unwrap().unwrap();
        assert_eq!(user.excluded_users, vec!["viewer"]);
        assert!(!user.settings.echo);
        assert!(!user.is_live, "imported live state is not bot permission");
        assert_eq!(store.enabled_twitch_ids().await.unwrap(), vec![20]);
        assert!(store.granted_twitch_ids().await.unwrap().is_empty());
        let source_live: bool = store
            .call(|c| {
                Ok(serde_json::from_str::<Value>(&c.query_row(
                    "SELECT raw_json FROM users",
                    [],
                    |r| r.get::<_, String>(0),
                )?)
                .unwrap()["is_live"]
                    .as_bool()
                    .unwrap())
            })
            .await
            .unwrap();
        assert!(source_live, "source provenance is retained unchanged");
        let metadata = store.settings_metadata().await.unwrap();
        let echo = metadata.iter().find(|v| v["name"] == "echo").unwrap();
        assert_eq!(echo["description"], "Customized");
        assert_eq!(echo["customField"], 7);
        let raw: String = store
            .call(|c| Ok(c.query_row("SELECT settings_json FROM users", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["custom"]["color"],
            "blue"
        );
        let top = store.leaderboard(0, 10, 0).await.unwrap();
        assert_eq!(top[0]["upstream_marker"], "retained");
        assert_eq!(top[0]["count"], 1);
        store.set_settings(20, Settings::default()).await.unwrap();
        let raw: String = store
            .call(|c| Ok(c.query_row("SELECT settings_json FROM users", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["custom"]["color"],
            "blue"
        );
        let source: String = store
            .call(|c| Ok(c.query_row("SELECT source_json FROM beatmaps", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&source).unwrap()["ronnia_updated_at"]["$date"],
            "2023-11-14T22:13:20Z"
        );
    }
    #[tokio::test]
    async fn duplicate_identity_rolls_back_every_collection() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        let user = sample_user();
        let mut duplicate = user.clone();
        duplicate["_id"] = json!({"$oid":"000000000000000000000003"});
        write(dir.path(), "Users", json!([user, duplicate]));
        let store = Store::open(":memory:").await.unwrap();
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(!report.committed);
        assert!(report.issue_count > 0);
        assert!(store.user_by_twitch(20).await.unwrap().is_none());
        assert_eq!(report.collections["Users"].seen, 2);
    }
    #[tokio::test]
    async fn invalid_record_and_existing_conflicts_are_reported_before_commit() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        write(dir.path(), "Users", json!([sample_user()]));
        let store = Store::open(":memory:").await.unwrap();
        assert!(
            import_exports(store.clone(), dir.path())
                .await
                .unwrap()
                .committed
        );
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(!report.committed);
        assert!(report.issues.iter().any(|i| i.message.contains("existing")));
        write(dir.path(), "Users", json!([]));
        write(
            dir.path(),
            "Statistics",
            json!([{"requested_beatmap_id":4,"requester_channel_name":"x","requested_channel_name":"y"}]),
        );
        let report = import_exports(store, dir.path()).await.unwrap();
        assert!(!report.committed);
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.message.contains("timestamp"))
        );
    }
    #[tokio::test]
    async fn dry_run_and_malformed_arrays_do_not_mutate_database() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        write(dir.path(), "Users", json!([sample_user()]));
        let store = Store::open(":memory:").await.unwrap();
        let report = validate_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.dry_run);
        assert_eq!(report.issue_count, 0);
        assert!(!report.committed);
        assert!(store.user_by_twitch(20).await.unwrap().is_none());
        std::fs::write(
            dir.path().join("Users.json"),
            format!("[{},", sample_user()),
        )
        .unwrap();
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.issue_count > 0);
        assert!(store.user_by_twitch(20).await.unwrap().is_none());
    }
    #[test]
    fn streams_bson_and_json_arrays_without_collecting_exports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Statistics.bson");
        let mut bytes = Vec::new();
        bson::doc! {"id":1_i64,"timestamp":bson::DateTime::from_millis(1700000000000)}
            .to_writer(&mut bytes)
            .unwrap();
        bson::doc! {"id":2_i64}.to_writer(&mut bytes).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mut values = Vec::new();
        stream_file(&path, |v| values.push(v)).unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(date_seconds(&values[0]["timestamp"]).unwrap(), 1700000000);
        let array = dir.path().join("Users.json");
        std::fs::write(&array, "[{\"id\":1},{\"id\":2}]").unwrap();
        let mut count = 0;
        stream_file(&array, |_| count += 1).unwrap();
        assert_eq!(count, 2);
    }
    #[test]
    fn rejects_duplicate_aliases_and_nonfinite_numeric_wrappers() {
        let mut value = json!({"osuId":1,"osu_id":2});
        assert!(normalize_id_alias(&mut value, &["osuId", "osu_id"], "osuId").is_err());
        assert!(normalize_extended(json!({"$numberDouble":"NaN"})).is_err());
    }
    #[test]
    fn rejects_duplicate_json_and_bson_fields_instead_of_collapsing_them() {
        assert!(
            serde_json::from_str::<StrictValue>("{\"settings\":{\"echo\":true,\"echo\":false}}")
                .is_err()
        );
        let bytes = [
            19, 0, 0, 0, 0x10, b'x', 0, 1, 0, 0, 0, 0x10, b'x', 0, 2, 0, 0, 0, 0,
        ];
        assert!(validate_raw_bson(bson::raw::RawDocument::from_bytes(&bytes).unwrap(), 0).is_err());
    }
    #[tokio::test]
    async fn preserves_identical_history_without_ids_but_rejects_duplicate_source_ids() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        let event = json!({"requester_channel_name":"viewer","requested_beatmap_id":30,"requested_channel_name":"channel","timestamp":1700000000});
        write(dir.path(), "Statistics", json!([event, event]));
        let store = Store::open(":memory:").await.unwrap();
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.committed);
        assert_eq!(store.leaderboard(0, 10, 0).await.unwrap()[0]["count"], 2);
        let sources: Vec<(String, String)> = store
            .call(|c| {
                let mut statement =
                    c.prepare("SELECT source_id,raw_json FROM statistics ORDER BY id")?;
                Ok(statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap();
        assert_ne!(sources[0].0, sources[1].0);
        assert!(sources.iter().all(|(id, raw)| {
            id.starts_with("ronnia:idless:")
                && serde_json::from_str::<Value>(raw)
                    .unwrap()
                    .get("_id")
                    .is_none()
        }));
        std::fs::write(
            dir.path().join("Statistics.json"),
            serde_json::to_string_pretty(&json!([event, event])).unwrap(),
        )
        .unwrap();
        let replay = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(!replay.committed);
        assert_eq!(replay.collections["Statistics"].invalid, 2);
        assert!(
            replay
                .issues
                .iter()
                .all(|issue| issue.message.contains("existing"))
        );
        assert_eq!(store.leaderboard(0, 10, 0).await.unwrap()[0]["count"], 2);
        let mut event = event;
        event["_id"] = json!({"$oid":"000000000000000000000001"});
        write(dir.path(), "Statistics", json!([event, event]));
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(!report.committed);
        assert_eq!(report.collections["Statistics"].invalid, 1);
        assert_eq!(store.leaderboard(0, 10, 0).await.unwrap()[0]["count"], 2);
    }
    #[tokio::test]
    async fn requires_exact_collection_names_and_complete_exports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("users.json"), "[]").unwrap();
        let store = Store::open(":memory:").await.unwrap();
        let report = validate_exports(store, dir.path()).await.unwrap();
        assert_eq!(report.issue_count, 5);
        assert!(!report.committed);
    }
    #[tokio::test]
    async fn recycled_logins_are_not_export_identity_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        fixtures(dir.path());
        let first = sample_user();
        let mut second = sample_user();
        second["_id"] = json!({"$oid":"000000000000000000000002"});
        second["osu_id"] = json!(11);
        second["twitch_id"] = json!(21);
        write(dir.path(), "Users", json!([first, second]));
        let store = Store::open(":memory:").await.unwrap();
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.committed, "{report:?}");
        let mut third = sample_user();
        third["_id"] = json!({"$oid":"000000000000000000000003"});
        third["osu_id"] = json!(12);
        third["twitch_id"] = json!(22);
        write(dir.path(), "Users", json!([third]));
        let report = import_exports(store.clone(), dir.path()).await.unwrap();
        assert!(report.committed, "{report:?}");
        for id in [20, 21, 22] {
            assert!(store.user_by_twitch(id).await.unwrap().is_some());
        }
    }
}
