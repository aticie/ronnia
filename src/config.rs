//! Runtime configuration parsed from the environment, validated once at startup
//! with keyed diagnostics.

use std::{env, net::IpAddr, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use url::Url;

/// Runtime configuration. Never log this value: it contains provider credentials.
#[derive(Clone)]
pub struct Config {
    pub database_path: PathBuf,
    pub bind: IpAddr,
    pub bind_port: u16,
    pub public_origin: String,
    pub api_origin: String,
    pub cookie_secure: bool,
    pub debug_mode: bool,
    pub twitch_client_id: String,
    pub twitch_client_secret: String,
    pub twitch_redirect_uri: String,
    pub osu_client_id: String,
    pub osu_client_secret: String,
    pub osu_redirect_uri: String,
    pub twitch_eventsub_url: String,
    pub twitch_token_key: Option<[u8; 32]>,
    pub twitch_api_base: String,
    pub twitch_oauth_base: String,
    pub osu_api_base: String,
    pub osu_oauth_base: String,
    pub dry_run: bool,
    pub reconcile_interval_secs: u64,
    pub eventsub_channels_per_shard: usize,
    pub eventsub_max_shards: u32,
    pub request_workers: usize,
    pub request_queue_capacity: usize,
    pub http_timeout_secs: u64,
    pub session_ttl_secs: i64,
    pub oauth_ttl_secs: i64,
    pub bot_enabled: bool,
    pub migration_active: bool,
    pub migration_cutoff_at: Option<String>,
    pub migration_grant_grace_secs: u64,
    pub internal_api_token: Option<String>,
}

/// Every safe field is listed once; credentials are always redacted.
macro_rules! debug_config {
    ($formatter:expr, $config:expr, [$($field:ident),* $(,)?]) => {{
        let mut debug = $formatter.debug_struct("Config");
        $(debug.field(stringify!($field), &$config.$field);)*
        debug.field("internal_api_configured", &$config.internal_api_token.is_some())
            .field("credentials", &"[REDACTED]")
            .finish()
    }};
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_config!(
            formatter,
            self,
            [
                database_path,
                bind,
                bind_port,
                public_origin,
                api_origin,
                cookie_secure,
                debug_mode,
                twitch_eventsub_url,
                twitch_api_base,
                twitch_oauth_base,
                osu_api_base,
                osu_oauth_base,
                dry_run,
                reconcile_interval_secs,
                eventsub_channels_per_shard,
                eventsub_max_shards,
                request_workers,
                request_queue_capacity,
                http_timeout_secs,
                session_ttl_secs,
                oauth_ttl_secs,
                bot_enabled,
                migration_active,
                migration_cutoff_at,
                migration_grant_grace_secs,
            ]
        )
    }
}

fn value(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn parsed<T: FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(v) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("Invalid {name}: {e}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(e).with_context(|| format!("Reading {name}")),
    }
}

fn within<T: PartialOrd + Copy + std::fmt::Display>(
    name: &str,
    value: T,
    min: T,
    max: T,
) -> Result<()> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        bail!("{name} must be between {min} and {max}")
    }
}

fn origin(name: &str, value: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("Invalid {name}"))?;
    let clean = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none();
    if !clean {
        bail!("{name} must be an HTTP(S) origin without path, query, fragment or credentials");
    }
    Ok(())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let api_origin = value("API_ORIGIN", "http://localhost:8080");
        let twitch_token_key = match env::var("TWITCH_TOKEN_KEY") {
            Ok(value) if !value.is_empty() => {
                let bytes = hex::decode(value)
                    .context("TWITCH_TOKEN_KEY must be 64 hexadecimal characters")?;
                Some(bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("TWITCH_TOKEN_KEY must encode exactly 32 bytes")
                })?)
            }
            Ok(_) | Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(error).context("Reading TWITCH_TOKEN_KEY"),
        };
        let migration_cutoff_at = env::var("MIGRATION_CUTOFF_AT")
            .ok()
            .filter(|value| !value.is_empty());
        if let Some(value) = &migration_cutoff_at {
            chrono::DateTime::parse_from_rfc3339(value)
                .context("MIGRATION_CUTOFF_AT must be an RFC 3339 timestamp")?;
        }
        let internal_api_token = env::var("INTERNAL_API_TOKEN")
            .ok()
            .filter(|value| !value.is_empty());
        let result = Self {
            database_path: value("DATABASE_PATH", "ronnia.sqlite3").into(),
            bind: parsed("BIND_IP", IpAddr::from([0, 0, 0, 0]))?,
            bind_port: parsed("BIND_PORT", 8080)?,
            public_origin: value("PUBLIC_ORIGIN", "http://localhost:3000"),
            cookie_secure: parsed("COOKIE_SECURE", api_origin.starts_with("https://"))?,
            debug_mode: parsed("DEBUG_MODE", false)?,
            twitch_client_id: value("TWITCH_CLIENT_ID", ""),
            twitch_client_secret: value("TWITCH_CLIENT_SECRET", ""),
            twitch_redirect_uri: value(
                "TWITCH_REDIRECT_URI",
                &format!("{api_origin}/oauth2/twitch-redirect"),
            ),
            osu_client_id: value("OSU_CLIENT_ID", ""),
            osu_client_secret: value("OSU_CLIENT_SECRET", ""),
            osu_redirect_uri: value(
                "OSU_REDIRECT_URI",
                &format!("{api_origin}/oauth2/osu-redirect"),
            ),
            twitch_eventsub_url: value(
                "TWITCH_EVENTSUB_URL",
                "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30",
            ),
            twitch_token_key,
            twitch_api_base: value("TWITCH_API_BASE", "https://api.twitch.tv/helix"),
            twitch_oauth_base: value("TWITCH_OAUTH_BASE", "https://id.twitch.tv/oauth2"),
            osu_api_base: value("OSU_API_BASE", "https://osu.ppy.sh/api/v2"),
            osu_oauth_base: value("OSU_OAUTH_BASE", "https://osu.ppy.sh/oauth"),
            dry_run: parsed("DRY_RUN", false)? || value("ENVIRONMENT", "") == "testing",
            reconcile_interval_secs: parsed("RECONCILE_INTERVAL_SECS", 600)?,
            eventsub_channels_per_shard: parsed("EVENTSUB_CHANNELS_PER_SHARD", 100)?,
            eventsub_max_shards: parsed("EVENTSUB_MAX_SHARDS", 16)?,
            request_workers: parsed("REQUEST_WORKERS", 8)?,
            request_queue_capacity: parsed("REQUEST_QUEUE_CAPACITY", 128)?,
            http_timeout_secs: parsed("HTTP_TIMEOUT_SECS", 20)?,
            session_ttl_secs: parsed("SESSION_TTL_SECS", 30 * 86400)?,
            oauth_ttl_secs: parsed("OAUTH_TTL_SECS", 600)?,
            bot_enabled: parsed("BOT_ENABLED", true)?,
            migration_active: parsed("MIGRATION_ACTIVE", false)?,
            migration_cutoff_at,
            migration_grant_grace_secs: parsed("MIGRATION_GRANT_GRACE_SECS", 10)?,
            internal_api_token,
            api_origin,
        };
        if result.eventsub_channels_per_shard == 0 {
            bail!("EVENTSUB_CHANNELS_PER_SHARD must be positive");
        }
        within("EVENTSUB_MAX_SHARDS", result.eventsub_max_shards, 1, 64)?;
        within("REQUEST_WORKERS", result.request_workers, 1, 64)?;
        within(
            "REQUEST_QUEUE_CAPACITY",
            result.request_queue_capacity,
            1,
            4096,
        )?;
        if result.reconcile_interval_secs == 0
            || result.http_timeout_secs == 0
            || result.session_ttl_secs <= 0
            || result.oauth_ttl_secs <= 0
        {
            bail!("Intervals and expiration durations must be positive");
        }
        origin("PUBLIC_ORIGIN", &result.public_origin)?;
        origin("API_ORIGIN", &result.api_origin)?;
        if result.api_origin.starts_with("https://") && !result.cookie_secure {
            bail!("COOKIE_SECURE must be true for an HTTPS API_ORIGIN");
        }
        Ok(result)
    }

    pub fn validate_serve(&self) -> Result<()> {
        if !self.bot_enabled {
            return Ok(());
        }
        for (key, val) in [
            ("TWITCH_CLIENT_ID", &self.twitch_client_id),
            ("TWITCH_CLIENT_SECRET", &self.twitch_client_secret),
            ("OSU_CLIENT_ID", &self.osu_client_id),
            ("OSU_CLIENT_SECRET", &self.osu_client_secret),
        ] {
            if val.is_empty() {
                bail!("{key} is required when the bot is enabled");
            }
        }
        if self.twitch_token_key.is_none() {
            bail!(
                "TWITCH_TOKEN_KEY is required when the bot is enabled; generate it with generate-token-key"
            );
        }
        let endpoint =
            Url::parse(&self.twitch_eventsub_url).context("Invalid TWITCH_EVENTSUB_URL")?;
        if !matches!(endpoint.scheme(), "ws" | "wss")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            bail!("TWITCH_EVENTSUB_URL must be a WebSocket URL without credentials or fragment");
        }
        if endpoint.scheme() == "ws" {
            tracing::warn!(
                "Unencrypted EventSub endpoint configured; use only for isolated local tests"
            );
        }
        Ok(())
    }
}
