use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ronnia::{api, bot, config::Config, storage::Store};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, prelude::*};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// SQLite database file, overrides DATABASE_PATH.
    #[arg(long, global = true)]
    database: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the API and supervised Twitch relay (the default command).
    Serve {
        /// Run only the account API, without Twitch chat or live polling.
        #[arg(long)]
        api_only: bool,
    },
    /// Fetch missing beatmap metadata for all historical request IDs.
    Backfill,
    /// Run SQLite integrity and foreign-key checks.
    Check,
    /// Create a consistent SQLite snapshot; destination must not exist.
    Backup {
        #[arg(long)]
        destination: PathBuf,
    },
    /// Inspect failed/uncertain messages; these are never replayed automatically.
    Deliveries {
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=1000))]
        limit: u16,
    },
    /// Authorize the bot account in a browser using Twitch's device-code flow.
    AuthorizeBot {
        /// Expected Twitch login of the bot account.
        #[arg(long)]
        login: String,
        /// Replace credentials for a different bot account.
        #[arg(long)]
        replace: bool,
    },
    /// Generate a random key for encrypting stored Twitch credentials.
    GenerateTokenKey,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new(std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".into()))
        }))
        .with_writer(std::io::stderr)
        .finish()
        .with(sentry_tracing::layer())
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _sentry = std::env::var("SENTRY_DSN")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|dsn| {
            sentry::init((
                dsn,
                sentry::ClientOptions {
                    release: sentry::release_name!(),
                    send_default_pii: false,
                    traces_sample_rate: 0.0,
                    ..Default::default()
                },
            ))
        });
    let mut config = Config::from_env()?;
    if let Some(path) = cli.database {
        config.database_path = path;
    }
    let command = cli.command.unwrap_or(Command::Serve { api_only: false });
    if matches!(command, Command::GenerateTokenKey) {
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::rng().fill_bytes(&mut key);
        println!("{}", hex::encode(key));
        eprintln!(
            "Store this as TWITCH_TOKEN_KEY in the service's secret environment. Discarding it makes stored bot credentials unrecoverable."
        );
        return Ok(());
    }
    if matches!(command, Command::Serve { api_only: true }) {
        config.bot_enabled = false;
    }
    if matches!(command, Command::Serve { .. }) {
        config.validate_serve()?;
    }
    let store = Store::open(&config.database_path).await?;
    match command {
        Command::Serve { .. } => serve(Arc::new(config), store).await,
        Command::Backfill => bot::backfill(Arc::new(config), store).await,
        Command::AuthorizeBot { login, replace } => {
            if config.twitch_client_id.is_empty()
                || config.twitch_client_secret.is_empty()
                || config.twitch_token_key.is_none()
            {
                bail!(
                    "Authorize-bot requires TWITCH_CLIENT_ID, TWITCH_CLIENT_SECRET and TWITCH_TOKEN_KEY"
                );
            }
            let shutdown = CancellationToken::new();
            let cancel = shutdown.clone();
            let interrupt = tokio::spawn(async move {
                if shutdown_signal().await.is_ok() {
                    cancel.cancel();
                }
            });
            let identity = ronnia::twitch_auth::authorize_bot(
                Arc::new(config),
                store,
                &login,
                replace,
                shutdown,
            )
            .await;
            interrupt.abort();
            let identity = identity?;
            println!("{}", serde_json::to_string_pretty(&identity)?);
            Ok(())
        }
        Command::GenerateTokenKey => unreachable!("handled before database initialization"),
        Command::Check => {
            let (integrity, foreign_keys) = store
                .call(|conn| {
                    let integrity = conn
                        .prepare("PRAGMA integrity_check")?
                        .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    let foreign_keys: i64 = conn.query_row(
                        "SELECT count(*) FROM pragma_foreign_key_check",
                        [],
                        |row| row.get(0),
                    )?;
                    Ok((integrity, foreign_keys))
                })
                .await?;
            println!(
                "{}",
                serde_json::json!({"integrity":integrity,"foreignKeyViolations":foreign_keys})
            );
            if integrity != ["ok"] || foreign_keys != 0 {
                bail!("Database integrity check failed");
            }
            Ok(())
        }
        Command::Backup { destination } => {
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
                .await?;
            println!("Created consistent backup: {display}");
            Ok(())
        }
        Command::Deliveries { limit } => {
            let rows = store.call(move |conn| {
                let mut statement = conn.prepare("SELECT id,twitch_message_id,target_osu_id,status,error,created_at FROM deliveries WHERE status IN ('uncertain','failed') ORDER BY id DESC LIMIT ?1")?;
                let rows = statement.query_map([i64::from(limit)], |row| {
                    Ok(serde_json::json!({
                        "id":row.get::<_,i64>(0)?,"twitchMessageId":row.get::<_,String>(1)?,
                        "targetOsuId":row.get::<_,i64>(2)?,"status":row.get::<_,String>(3)?,
                        "error":row.get::<_,Option<String>>(4)?,"createdAt":row.get::<_,i64>(5)?
                    }))
                })?.collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            }).await?;
            println!("{}", serde_json::to_string_pretty(&rows)?);
            Ok(())
        }
    }
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn serve(config: Arc<Config>, store: Store) -> Result<()> {
    let listener = tokio::net::TcpListener::bind((config.bind, config.bind_port))
        .await
        .context("Binding API listener")?;
    let address = listener.local_addr()?;
    let shutdown = CancellationToken::new();
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    let app = api::router(config.clone(), store.clone());
    let api_shutdown = shutdown.clone();
    tasks.spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(api_shutdown.cancelled_owned())
            .await?;
        Ok(())
    });
    if config.bot_enabled {
        let bot_shutdown = shutdown.clone();
        tasks.spawn(bot::run(config, store, bot_shutdown));
    }
    tracing::info!(%address, "API listening");
    let result = tokio::select! {
        signal = shutdown_signal() => signal,
        completed = tasks.join_next() => match completed {
            Some(Ok(Err(error))) => Err(error.context("Service worker failed")),
            Some(Err(error)) => Err(error.into()),
            _ => Err(anyhow::anyhow!("Service worker stopped unexpectedly")),
        },
    };
    shutdown.cancel();
    let drained = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(task) = tasks.join_next().await {
            if let Err(error) = task {
                tracing::error!(%error, "Worker failed during shutdown");
            }
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("Shutdown deadline reached; cancelling remaining workers");
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    result
}
