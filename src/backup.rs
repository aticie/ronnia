//! Consistent, verified SQLite snapshots uploaded to S3-compatible storage.

use std::{
    fs::File,
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use aws_sdk_s3::{Client, primitives::ByteStream};
use chrono::Utc;
use flate2::{Compression, write::GzEncoder};
use rusqlite::{Connection, OpenFlags};
use tokio_util::sync::CancellationToken;

use crate::{config::BackupConfig, storage::Store};

pub async fn create_snapshot(store: &Store, destination: PathBuf) -> Result<()> {
    let display = destination.display().to_string();
    if destination.exists() {
        bail!("Backup destination already exists: {display}");
    }
    let path = destination
        .to_str()
        .context("Backup path must be valid UTF-8")?
        .to_owned();
    store
        .call(move |conn| {
            conn.execute("VACUUM INTO ?1", [path])?;
            Ok(())
        })
        .await
}

pub async fn run(config: BackupConfig, store: Store, shutdown: CancellationToken) -> Result<()> {
    let sdk_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let client = Client::from_conf(
        aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(config.force_path_style)
            .build(),
    );
    let mut interval = tokio::time::interval(config.interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tracing::info!(
        bucket = %config.bucket,
        prefix = %config.prefix,
        interval_seconds = config.interval.as_secs(),
        retention_days = config.retention_days,
        "SQLite backup worker enabled"
    );
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {
                match backup_once(&client, &config, &store).await {
                    Ok(report) => tracing::info!(
                        key = %report.key,
                        source_bytes = report.source_bytes,
                        uploaded_bytes = report.uploaded_bytes,
                        expired = report.expired,
                        "SQLite backup uploaded"
                    ),
                    Err(error) => tracing::error!(
                        error = format!("{error:#}"),
                        "SQLite backup failed"
                    ),
                }
            }
        }
    }
}

struct BackupReport {
    key: String,
    source_bytes: u64,
    uploaded_bytes: u64,
    expired: usize,
}

async fn backup_once(
    client: &Client,
    config: &BackupConfig,
    store: &Store,
) -> Result<BackupReport> {
    let directory = tempfile::tempdir().context("Creating temporary backup directory")?;
    let snapshot = directory.path().join("ronnia.sqlite3");
    create_snapshot(store, snapshot.clone()).await?;
    let compressed = directory.path().join("ronnia.sqlite3.gz");
    let snapshot_for_preparation = snapshot.clone();
    let compressed_for_preparation = compressed.clone();
    let (source_bytes, uploaded_bytes) = tokio::task::spawn_blocking(move || {
        prepare_upload(&snapshot_for_preparation, &compressed_for_preparation)
    })
    .await
    .context("Joining backup preparation")??;

    let key = format!(
        "{}/ronnia-{}.sqlite3.gz",
        config.prefix,
        Utc::now().format("%Y%m%dT%H%M%S%.3fZ")
    );
    let body = ByteStream::from_path(&compressed)
        .await
        .context("Reading compressed SQLite backup")?;
    client
        .put_object()
        .bucket(&config.bucket)
        .key(&key)
        .content_type("application/gzip")
        .body(body)
        .send()
        .await
        .context("Uploading SQLite backup")?;
    let expired = delete_expired(client, config).await?;
    Ok(BackupReport {
        key,
        source_bytes,
        uploaded_bytes,
        expired,
    })
}

fn prepare_upload(snapshot: &Path, compressed: &Path) -> Result<(u64, u64)> {
    verify_snapshot(snapshot)?;
    let source_bytes = snapshot.metadata()?.len();
    let mut input = BufReader::new(File::open(snapshot)?);
    let output = BufWriter::new(File::create(compressed)?);
    let mut encoder = GzEncoder::new(output, Compression::default());
    std::io::copy(&mut input, &mut encoder).context("Compressing SQLite backup")?;
    encoder.finish()?.flush()?;
    Ok((source_bytes, compressed.metadata()?.len()))
}

fn verify_snapshot(path: &Path) -> Result<()> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("Opening SQLite backup for verification")?;
    let integrity = connection
        .prepare("PRAGMA integrity_check")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let foreign_keys: i64 =
        connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if integrity != ["ok"] || foreign_keys != 0 {
        bail!(
            "SQLite backup verification failed: integrity={integrity:?}, foreign key violations={foreign_keys}"
        );
    }
    Ok(())
}

fn is_generated_backup_key(key: &str) -> bool {
    key.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("ronnia-") && name.ends_with(".sqlite3.gz"))
}

async fn delete_expired(client: &Client, config: &BackupConfig) -> Result<usize> {
    let cutoff = Utc::now() - chrono::Duration::days(i64::from(config.retention_days));
    let mut continuation_token = None;
    let mut expired = Vec::new();
    loop {
        let page = client
            .list_objects_v2()
            .bucket(&config.bucket)
            .prefix(format!("{}/", config.prefix))
            .set_continuation_token(continuation_token)
            .send()
            .await
            .context("Listing SQLite backups")?;
        for object in page.contents() {
            let (Some(key), Some(modified)) = (object.key(), object.last_modified()) else {
                continue;
            };
            if is_generated_backup_key(key) && modified.secs() < cutoff.timestamp() {
                expired.push(key.to_owned());
            }
        }
        continuation_token = page.next_continuation_token().map(str::to_owned);
        if continuation_token.is_none() {
            break;
        }
    }
    for key in &expired {
        client
            .delete_object()
            .bucket(&config.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("Deleting expired SQLite backup {key}"))?;
    }
    Ok(expired.len())
}

#[cfg(test)]
mod tests {
    use flate2::read::GzDecoder;

    use super::*;

    #[tokio::test]
    async fn snapshot_survives_compression_and_restore() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.sqlite3");
        let store = Store::open(&source).await.unwrap();
        store
            .call(|connection| {
                connection.execute_batch(
                    "CREATE TABLE backup_probe (value TEXT); INSERT INTO backup_probe VALUES ('preserved');",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let snapshot = directory.path().join("snapshot.sqlite3");
        create_snapshot(&store, snapshot.clone()).await.unwrap();
        let compressed = directory.path().join("snapshot.sqlite3.gz");
        let (source_bytes, uploaded_bytes) = prepare_upload(&snapshot, &compressed).unwrap();
        assert!(source_bytes > 0);
        assert!(uploaded_bytes > 0);

        let restored = directory.path().join("restored.sqlite3");
        let mut decoder = GzDecoder::new(File::open(&compressed).unwrap());
        let mut output = File::create(&restored).unwrap();
        std::io::copy(&mut decoder, &mut output).unwrap();
        output.flush().unwrap();
        verify_snapshot(&restored).unwrap();
        let connection = Connection::open(&restored).unwrap();
        let value: String = connection
            .query_row("SELECT value FROM backup_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, "preserved");
    }

    #[test]
    fn retention_ignores_unrelated_objects() {
        assert!(is_generated_backup_key(
            "production/ronnia/ronnia-20261114T123000.000Z.sqlite3.gz"
        ));
        assert!(!is_generated_backup_key("production/ronnia/unrelated.gz"));
        assert!(!is_generated_backup_key(
            "production/ronnia/ronnia.sqlite3.gz"
        ));
    }

    #[test]
    fn invalid_snapshot_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("invalid.sqlite3");
        std::fs::write(&path, b"not a SQLite database").unwrap();
        assert!(verify_snapshot(&path).is_err());
    }
}
