//! Twitch user authorization for the chat bot.
//!
//! The bot identity is obtained with the OAuth device grant, validated at startup and
//! hourly, and refreshed with a single-flight rotation. Tokens are only persisted as
//! nonce-prefixed AES-256-GCM ciphertext; the key lives in configuration and is never
//! written to the database. Nothing in this module logs or derives `Debug` for tokens.

mod crypto;

use crate::{config::Config, storage::Store};
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use reqwest::{Client, Response, StatusCode, redirect::Policy};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::sleep};
use tokio_util::sync::CancellationToken;

use crypto::Credentials;

/// Scopes required for EventSub chat delivery and for sending chat messages as a user.
pub const BOT_SCOPES: [&str; 3] = ["user:read:chat", "user:write:chat", "user:bot"];
const SCOPE_STRING: &str = "user:read:chat user:write:chat user:bot";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const VALIDATION_INTERVAL_SECS: i64 = 60 * 60;
const REFRESH_MARGIN_SECS: i64 = 60;
const MAX_AUTH_BODY_BYTES: usize = 64 * 1024;
const MAX_STATE_ATTEMPTS: usize = 4;
/// `validated_at` sentinel: the stored grant can no longer be used, but its encrypted
/// evidence is retained until the operator reauthorizes (never delete by accident).
const UNUSABLE: i64 = -1;

#[derive(Clone, Debug, Serialize)]
pub struct BotIdentity {
    pub user_id: String,
    pub login: String,
}

/// CLI entry point for the browser-based device grant.
///
/// The browser authorizes the account; this process only receives the resulting tokens.
pub async fn authorize_bot(
    config: Arc<Config>,
    store: Store,
    expected_login: &str,
    replace: bool,
    shutdown: CancellationToken,
) -> Result<BotIdentity> {
    TwitchAuth::authorize_bot(config, store, expected_login, replace, shutdown).await
}

/// Shared handle to the bot authorization. Clones share the refresh single-flight lock.
#[derive(Clone)]
pub struct TwitchAuth {
    config: Arc<Config>,
    store: Store,
    client: Client,
    key: [u8; 32],
    refresh: Arc<Mutex<()>>,
    validated_this_process: Arc<AtomicBool>,
}

/// A row of `twitch_bot_auth`. Ciphertext is opaque to consumers; no tokens are exposed.
struct StoredGrant {
    user_id: String,
    login: String,
    ciphertext: Vec<u8>,
    expires_at: i64,
    scopes: Vec<String>,
    validated_at: i64,
    revision: i64,
}

struct TokenGrant {
    credentials: Credentials,
    expires_at: i64,
}

struct Validation {
    client_id: String,
    user_id: String,
    login: String,
    scopes: Vec<String>,
    expires_in: i64,
}

enum ValidationOutcome {
    Valid(Validation),
    Unauthorized,
}

enum RefreshError {
    Invalid(anyhow::Error),
    Transient(anyhow::Error),
}

impl TwitchAuth {
    pub fn new(config: Arc<Config>, store: Store) -> Result<Self> {
        let key = config
            .twitch_token_key
            .context("TWITCH_TOKEN_KEY is required for Twitch bot authorization")?;
        ensure!(
            !config.twitch_client_id.trim().is_empty(),
            "TWITCH_CLIENT_ID is required for Twitch bot authorization"
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(config.http_timeout_secs.clamp(1, 15)))
            .redirect(Policy::none())
            .user_agent(concat!("ronnia/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building Twitch authorization HTTP client")?;
        Ok(Self {
            config,
            store,
            client,
            key,
            refresh: Arc::new(Mutex::new(())),
            validated_this_process: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Return the configured bot identity, validating hourly and refreshing proactively.
    ///
    /// `Ok(None)` means no bot has ever authorized. A persisted unusable/revoked grant is
    /// an error without network traffic, so supervisors back off instead of hot-looping.
    /// Transient provider failures are errors and leave stored evidence untouched.
    pub async fn ready_bot(&self, shutdown: &CancellationToken) -> Result<Option<BotIdentity>> {
        let _single_flight = self.refresh.lock().await;
        for _ in 0..MAX_STATE_ATTEMPTS {
            let Some(grant) = self.load().await? else {
                return Ok(None);
            };
            if grant.validated_at == UNUSABLE {
                bail!(
                    "Twitch bot authorization is no longer usable; run `authorize-bot --login {}` again",
                    grant.login
                );
            }
            let now = Utc::now().timestamp();
            let expiring = grant.expires_at.saturating_sub(now) < REFRESH_MARGIN_SECS;
            // Twitch requires validation when the application starts and hourly after.
            let startup_validation = !self.validated_this_process.load(Ordering::Acquire);
            let stale_validation = startup_validation
                || grant.validated_at <= 0
                || now.saturating_sub(grant.validated_at) >= VALIDATION_INTERVAL_SECS;
            if !expiring && !stale_validation {
                return Ok(Some(BotIdentity {
                    user_id: grant.user_id,
                    login: grant.login,
                }));
            }

            let mut credentials = crypto::decrypt(&self.key, &grant.user_id, &grant.ciphertext)
                .context("decrypting stored Twitch bot credentials")?;
            let mut revision = grant.revision;
            let mut refreshed = false;

            if expiring {
                match self.refresh_tokens(&credentials, shutdown).await {
                    Ok(rotated) => {
                        let Some(next) = self
                            .persist_refresh(
                                revision,
                                &grant.user_id,
                                &grant.scopes,
                                rotated.expires_at,
                                &rotated.credentials,
                            )
                            .await?
                        else {
                            continue;
                        };
                        revision = next;
                        credentials = rotated.credentials;
                        refreshed = true;
                    }
                    Err(RefreshError::Invalid(error)) => {
                        if self.mark_unusable(revision).await? {
                            return Err(error);
                        }
                        continue;
                    }
                    Err(RefreshError::Transient(error)) => return Err(error),
                }
            }

            let validation = match self
                .validate_token(&credentials.access_token, shutdown)
                .await?
            {
                ValidationOutcome::Valid(validation) => validation,
                ValidationOutcome::Unauthorized if refreshed => {
                    let error = anyhow!(
                        "Twitch rejected the refreshed bot token; run `authorize-bot` again"
                    );
                    if self.mark_unusable(revision).await? {
                        return Err(error);
                    }
                    continue;
                }
                ValidationOutcome::Unauthorized => {
                    // A 401 is definitive only after one refresh attempt, per Twitch's
                    // validation requirement. Transient validation failures never get here.
                    match self.refresh_tokens(&credentials, shutdown).await {
                        Ok(rotated) => {
                            let Some(next) = self
                                .persist_refresh(
                                    revision,
                                    &grant.user_id,
                                    &grant.scopes,
                                    rotated.expires_at,
                                    &rotated.credentials,
                                )
                                .await?
                            else {
                                continue;
                            };
                            revision = next;
                            credentials = rotated.credentials;
                        }
                        Err(RefreshError::Invalid(error)) => {
                            if self.mark_unusable(revision).await? {
                                return Err(error);
                            }
                            continue;
                        }
                        Err(RefreshError::Transient(error)) => return Err(error),
                    }
                    match self
                        .validate_token(&credentials.access_token, shutdown)
                        .await?
                    {
                        ValidationOutcome::Valid(validation) => validation,
                        ValidationOutcome::Unauthorized => {
                            let error =
                                anyhow!("Twitch revoked the bot token; run `authorize-bot` again");
                            if self.mark_unusable(revision).await? {
                                return Err(error);
                            }
                            continue;
                        }
                    }
                }
            };

            if let Some(problem) =
                self.validation_problem(&validation, Some(&grant.user_id), Some(&grant.login))
            {
                if self.mark_unusable(revision).await? {
                    bail!("{problem}; run `authorize-bot` again");
                }
                continue;
            }
            let expires_at = Utc::now().timestamp().saturating_add(validation.expires_in);
            if self
                .persist_validation(revision, &validation.login, &validation.scopes, expires_at)
                .await?
                .is_none()
            {
                continue;
            }
            self.validated_this_process.store(true, Ordering::Release);
            return Ok(Some(BotIdentity {
                user_id: validation.user_id,
                login: validation.login,
            }));
        }
        bail!("Twitch bot authorization kept changing concurrently; retry")
    }

    /// Current bot user access token, refreshing only when it is close to expiry.
    ///
    /// This token is for the bot's own user APIs (chat send); it is never used as an app
    /// token for conduits/subscriptions.
    pub async fn bot_access_token(&self, shutdown: &CancellationToken) -> Result<String> {
        let _single_flight = self.refresh.lock().await;
        for _ in 0..MAX_STATE_ATTEMPTS {
            let Some(grant) = self.load().await? else {
                bail!("Twitch bot is not authorized; run `authorize-bot` first");
            };
            if grant.validated_at == UNUSABLE {
                bail!("Twitch bot authorization is no longer usable; run `authorize-bot` again");
            }
            let credentials = crypto::decrypt(&self.key, &grant.user_id, &grant.ciphertext)
                .context("decrypting stored Twitch bot credentials")?;
            if grant.expires_at.saturating_sub(Utc::now().timestamp()) >= REFRESH_MARGIN_SECS {
                return Ok(credentials.access_token);
            }
            match self.refresh_tokens(&credentials, shutdown).await {
                Ok(rotated) => {
                    if self
                        .persist_refresh(
                            grant.revision,
                            &grant.user_id,
                            &grant.scopes,
                            rotated.expires_at,
                            &rotated.credentials,
                        )
                        .await?
                        .is_none()
                    {
                        continue;
                    }
                    return Ok(rotated.credentials.access_token);
                }
                Err(RefreshError::Invalid(error)) => {
                    if self.mark_unusable(grant.revision).await? {
                        return Err(error);
                    }
                }
                Err(RefreshError::Transient(error)) => return Err(error),
            }
        }
        bail!("Twitch bot authorization kept changing concurrently; retry")
    }

    /// Rotate the token after a resource request returned 401. If another caller already
    /// replaced the rejected token, its newer token is returned instead of refreshing twice.
    pub async fn bot_access_token_after_rejection(
        &self,
        rejected: &str,
        shutdown: &CancellationToken,
    ) -> Result<String> {
        let _single_flight = self.refresh.lock().await;
        for _ in 0..MAX_STATE_ATTEMPTS {
            let Some(grant) = self.load().await? else {
                bail!("Twitch bot is not authorized; run `authorize-bot` first");
            };
            if grant.validated_at == UNUSABLE {
                bail!("Twitch bot authorization is no longer usable; run `authorize-bot` again");
            }
            let credentials = crypto::decrypt(&self.key, &grant.user_id, &grant.ciphertext)
                .context("decrypting stored Twitch bot credentials")?;
            if credentials.access_token != rejected {
                return Ok(credentials.access_token);
            }
            match self.refresh_tokens(&credentials, shutdown).await {
                Ok(rotated) => {
                    if self
                        .persist_refresh(
                            grant.revision,
                            &grant.user_id,
                            &grant.scopes,
                            rotated.expires_at,
                            &rotated.credentials,
                        )
                        .await?
                        .is_none()
                    {
                        continue;
                    }
                    return Ok(rotated.credentials.access_token);
                }
                Err(RefreshError::Invalid(error)) => {
                    if self.mark_unusable(grant.revision).await? {
                        return Err(error);
                    }
                }
                Err(RefreshError::Transient(error)) => return Err(error),
            }
        }
        bail!("Twitch bot authorization kept changing concurrently; retry")
    }

    /// Interactive OAuth device grant used by the `authorize-bot` CLI.
    ///
    /// The browser authorizes the account; this process only ever sees the resulting
    /// tokens, never a pasted credential. The verification URL and user code are printed
    /// to stdout, while the device code and tokens stay off all logs.
    pub async fn authorize_bot(
        config: Arc<Config>,
        store: Store,
        expected_login: &str,
        replace: bool,
        shutdown: CancellationToken,
    ) -> Result<BotIdentity> {
        let auth = Self::new(config, store)?;
        auth.authorize(expected_login, replace, &shutdown).await
    }

    async fn authorize(
        &self,
        expected_login: &str,
        replace: bool,
        shutdown: &CancellationToken,
    ) -> Result<BotIdentity> {
        let expected_login = expected_login.trim().to_ascii_lowercase();
        ensure!(
            !expected_login.is_empty(),
            "expected bot login cannot be empty"
        );
        ensure!(
            expected_login
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "expected bot login must contain only letters, digits and underscores"
        );

        let device_url = self.endpoint("/device");
        let response = self
            .send(
                self.client.post(&device_url).form(&[
                    ("client_id", self.config.twitch_client_id.as_str()),
                    ("scopes", SCOPE_STRING),
                ]),
                shutdown,
                "starting Twitch device authorization",
            )
            .await?;
        let status = response.status();
        let body = read_body(response).await?;
        if status != StatusCode::OK {
            bail!("Twitch device authorization failed (HTTP {status})");
        }
        let body = body.context("Twitch device authorization returned no payload")?;
        let device_code = required_string(&body, "device_code")?;
        let user_code = required_string(&body, "user_code")?;
        let verification_uri = required_string(&body, "verification_uri")?;
        let expires_in = required_positive(&body, "expires_in")?;
        let mut interval = body
            .get("interval")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 60);

        println!("Open this URL in a browser: {verification_uri}");
        println!("Enter the code: {user_code}");

        let deadline = Utc::now().timestamp().saturating_add(expires_in);
        let grant = loop {
            if shutdown.is_cancelled() {
                bail!("Twitch device authorization cancelled");
            }
            let remaining = deadline.saturating_sub(Utc::now().timestamp());
            if remaining <= 0 {
                bail!("Twitch device authorization expired");
            }
            let wait = (interval as i64).min(remaining) as u64;
            tokio::select! {
                _ = shutdown.cancelled() => bail!("Twitch device authorization cancelled"),
                _ = sleep(Duration::from_secs(wait)) => {}
            }
            if Utc::now().timestamp() >= deadline {
                bail!("Twitch device authorization expired");
            }

            let token_url = self.endpoint("/token");
            let mut form = vec![
                ("client_id", self.config.twitch_client_id.as_str()),
                ("scopes", SCOPE_STRING),
                ("device_code", device_code.as_str()),
                ("grant_type", DEVICE_GRANT_TYPE),
            ];
            if !self.config.twitch_client_secret.is_empty() {
                form.push(("client_secret", self.config.twitch_client_secret.as_str()));
            }
            let response = match self
                .send(
                    self.client.post(&token_url).form(&form),
                    shutdown,
                    "polling Twitch device authorization",
                )
                .await
            {
                Ok(response) => response,
                Err(error) if shutdown.is_cancelled() => return Err(error),
                Err(_) => {
                    // Network faults during explicit user consent are retried until the
                    // device code expires; no token payload can be involved here.
                    eprintln!("Twitch device authorization polling failed; retrying");
                    continue;
                }
            };
            let status = response.status();
            let body = read_body(response).await?;
            let retry = body.as_ref().and_then(message_code).unwrap_or_default();
            match status {
                StatusCode::OK => {
                    let body = body.context("Twitch token endpoint returned no payload")?;
                    let access_token = required_string(&body, "access_token")?;
                    let refresh_token = required_string(&body, "refresh_token")?;
                    required_positive(&body, "expires_in")?;
                    break Credentials {
                        access_token,
                        refresh_token,
                    };
                }
                StatusCode::BAD_REQUEST => {
                    if retry.contains("authorization_pending") {
                        continue;
                    }
                    if retry.contains("slow_down") {
                        interval = (interval + 5).min(60);
                        continue;
                    }
                    if retry.contains("access_denied") {
                        bail!("Twitch device authorization was denied");
                    }
                    if retry.contains("expired") {
                        bail!("Twitch device authorization expired");
                    }
                    bail!("Twitch rejected the device authorization request");
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    interval = (interval + 5).min(60);
                    continue;
                }
                status if status.is_server_error() => {
                    eprintln!("Twitch device authorization is temporarily unavailable; retrying");
                    continue;
                }
                _ => bail!("Twitch device authorization failed (HTTP {status})"),
            }
            // Unreachable: every branch either continues, breaks, or returns.
        };

        let validation = match self.validate_token(&grant.access_token, shutdown).await? {
            ValidationOutcome::Valid(validation) => validation,
            ValidationOutcome::Unauthorized => {
                bail!("Twitch rejected the newly authorized bot token")
            }
        };
        if let Some(problem) = self.validation_problem(&validation, None, Some(&expected_login)) {
            bail!("{problem}; the browser account did not match the configured bot");
        }
        let user_id = validation.user_id;
        let login = validation.login.to_ascii_lowercase();
        let expires_at = Utc::now().timestamp().saturating_add(validation.expires_in);
        let scopes = validation.scopes;
        let ciphertext = crypto::encrypt(&self.key, &user_id, &grant)?;
        let scopes_json = serde_json::to_string(&scopes)?;
        let validated_at = Utc::now().timestamp();
        let identity = BotIdentity {
            user_id: user_id.clone(),
            login: login.clone(),
        };
        self.store
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT user_id FROM twitch_bot_auth WHERE singleton=1",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing) = existing
                    && existing != user_id
                    && !replace
                {
                    bail!(
                        "a different Twitch bot account is already authorized; \
                         re-run with --replace to replace it"
                    );
                }
                transaction.execute(
                    "INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision) \
                     VALUES(1,?1,?2,?3,?4,?5,?6,1) \
                     ON CONFLICT(singleton) DO UPDATE SET \
                       user_id=excluded.user_id,login=excluded.login,ciphertext=excluded.ciphertext, \
                       expires_at=excluded.expires_at,scopes_json=excluded.scopes_json, \
                       validated_at=excluded.validated_at,revision=twitch_bot_auth.revision+1",
                    params![user_id, login, ciphertext, expires_at, scopes_json, validated_at],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await?;
        Ok(identity)
    }

    fn endpoint(&self, suffix: &str) -> String {
        format!(
            "{}{suffix}",
            self.config.twitch_oauth_base.trim_end_matches('/')
        )
    }

    async fn load(&self) -> Result<Option<StoredGrant>> {
        self.store
            .call(|connection| {
                let row = connection
                    .query_row(
                        "SELECT user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision \
                         FROM twitch_bot_auth WHERE singleton=1",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Vec<u8>>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, String>(4)?,
                                row.get::<_, i64>(5)?,
                                row.get::<_, i64>(6)?,
                            ))
                        },
                    )
                    .optional()?;
                row.map(
                    |(user_id, login, ciphertext, expires_at, scopes_json, validated_at, revision)| {
                        let scopes = serde_json::from_str::<Vec<String>>(&scopes_json)
                            .context("stored Twitch bot scopes are invalid")?;
                        Ok(StoredGrant {
                            user_id,
                            login,
                            ciphertext,
                            expires_at,
                            scopes,
                            validated_at,
                            revision,
                        })
                    },
                )
                .transpose()
            })
            .await
    }

    /// Store rotated tokens immediately. Device-grant refresh tokens are single use, so a
    /// successful exchange must never be dropped just because a later validation fails.
    /// `validated_at=0` forces validation on the next check.
    async fn persist_refresh(
        &self,
        revision: i64,
        user_id: &str,
        scopes: &[String],
        expires_at: i64,
        credentials: &Credentials,
    ) -> Result<Option<i64>> {
        let ciphertext = crypto::encrypt(&self.key, user_id, credentials)?;
        let scopes_json = serde_json::to_string(scopes)?;
        let changed = self
            .store
            .call(move |connection| {
                let count = connection.execute(
                    "UPDATE twitch_bot_auth \
                     SET ciphertext=?1,expires_at=?2,scopes_json=?3,validated_at=0,revision=revision+1 \
                     WHERE singleton=1 AND revision=?4 AND validated_at<>?5",
                    params![ciphertext, expires_at, scopes_json, revision, UNUSABLE],
                )?;
                Ok(count == 1)
            })
            .await?;
        Ok(changed.then_some(revision + 1))
    }

    async fn persist_validation(
        &self,
        revision: i64,
        login: &str,
        scopes: &[String],
        expires_at: i64,
    ) -> Result<Option<i64>> {
        let scopes_json = serde_json::to_string(scopes)?;
        let login = login.to_owned();
        let validated_at = Utc::now().timestamp();
        let changed = self
            .store
            .call(move |connection| {
                let count = connection.execute(
                    "UPDATE twitch_bot_auth \
                     SET login=?1,scopes_json=?2,expires_at=?3,validated_at=?4,revision=revision+1 \
                     WHERE singleton=1 AND revision=?5 AND validated_at<>?6",
                    params![
                        login,
                        scopes_json,
                        expires_at,
                        validated_at,
                        revision,
                        UNUSABLE
                    ],
                )?;
                Ok(count == 1)
            })
            .await?;
        Ok(changed.then_some(revision + 1))
    }

    /// Persist a definitive revocation without deleting evidence. Returns false when the
    /// row changed concurrently (for example a CLI reauthorization), in which case the
    /// caller reloads instead of overwriting fresher credentials.
    async fn mark_unusable(&self, revision: i64) -> Result<bool> {
        self.store
            .call(move |connection| {
                let count = connection.execute(
                    "UPDATE twitch_bot_auth SET validated_at=?1,revision=revision+1 \
                     WHERE singleton=1 AND revision=?2 AND validated_at<>?1",
                    params![UNUSABLE, revision],
                )?;
                Ok(count == 1)
            })
            .await
    }

    async fn refresh_tokens(
        &self,
        current: &Credentials,
        shutdown: &CancellationToken,
    ) -> std::result::Result<TokenGrant, RefreshError> {
        let url = self.endpoint("/token");
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", current.refresh_token.as_str()),
            ("client_id", self.config.twitch_client_id.as_str()),
        ];
        if !self.config.twitch_client_secret.is_empty() {
            form.push(("client_secret", self.config.twitch_client_secret.as_str()));
        }
        let response = self
            .send(
                self.client.post(&url).form(&form),
                shutdown,
                "refreshing Twitch bot token",
            )
            .await
            .map_err(RefreshError::Transient)?;
        let status = response.status();
        let body = read_body(response).await.map_err(RefreshError::Transient)?;
        match status {
            StatusCode::OK => {
                let body = body.ok_or_else(|| {
                    RefreshError::Transient(anyhow!("Twitch token refresh returned no payload"))
                })?;
                let access_token =
                    required_string(&body, "access_token").map_err(RefreshError::Transient)?;
                let refresh_token = body
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .filter(|token| !token.is_empty())
                    .map(str::to_owned)
                    // RFC 6749 permits reusing the existing refresh token when the
                    // provider does not rotate it.
                    .unwrap_or_else(|| current.refresh_token.clone());
                let expires_in =
                    required_positive(&body, "expires_in").map_err(RefreshError::Transient)?;
                Ok(TokenGrant {
                    credentials: Credentials {
                        access_token,
                        refresh_token,
                    },
                    expires_at: Utc::now().timestamp().saturating_add(expires_in),
                })
            }
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED => {
                Err(RefreshError::Invalid(anyhow!(
                    "Twitch bot refresh token is no longer valid; run `authorize-bot` again (HTTP {status})"
                )))
            }
            status => Err(RefreshError::Transient(anyhow!(
                "Twitch token refresh temporarily failed (HTTP {status})"
            ))),
        }
    }

    async fn validate_token(
        &self,
        token: &str,
        shutdown: &CancellationToken,
    ) -> Result<ValidationOutcome> {
        let url = self.endpoint("/validate");
        let response = self
            .send(
                self.client
                    .get(&url)
                    .header(reqwest::header::AUTHORIZATION, format!("OAuth {token}")),
                shutdown,
                "validating Twitch bot token",
            )
            .await?;
        let status = response.status();
        let body = read_body(response).await?;
        match status {
            StatusCode::OK => {
                let body = body.context("Twitch token validation returned no payload")?;
                let client_id = required_string(&body, "client_id")?;
                let user_id = required_string(&body, "user_id")?;
                let login = required_string(&body, "login")?;
                let scopes = body
                    .get("scopes")
                    .and_then(Value::as_array)
                    .context("Twitch token validation omitted scopes")?
                    .iter()
                    .map(|scope| {
                        scope
                            .as_str()
                            .map(str::to_owned)
                            .context("Twitch token validation had an invalid scope")
                    })
                    .collect::<Result<Vec<_>>>()?;
                let expires_in = required_positive(&body, "expires_in")?;
                Ok(ValidationOutcome::Valid(Validation {
                    client_id,
                    user_id,
                    login,
                    scopes,
                    expires_in,
                }))
            }
            StatusCode::UNAUTHORIZED => Ok(ValidationOutcome::Unauthorized),
            status => bail!("Twitch token validation temporarily failed (HTTP {status})"),
        }
    }

    fn validation_problem(
        &self,
        validation: &Validation,
        expected_user_id: Option<&str>,
        expected_login: Option<&str>,
    ) -> Option<String> {
        if validation.client_id != self.config.twitch_client_id {
            return Some("Twitch bot token was issued to a different client".into());
        }
        if expected_user_id.is_some_and(|expected| validation.user_id != expected) {
            return Some("Twitch bot token belongs to a different user".into());
        }
        if expected_login.is_some_and(|expected| !validation.login.eq_ignore_ascii_case(expected)) {
            return Some("Twitch bot token belongs to a different login".into());
        }
        let missing = BOT_SCOPES
            .iter()
            .filter(|required| {
                !validation
                    .scopes
                    .iter()
                    .any(|scope| scope.as_str() == **required)
            })
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Some(format!(
                "Twitch bot token is missing required scopes: {}",
                missing.join(", ")
            ));
        }
        None
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        shutdown: &CancellationToken,
        operation: &str,
    ) -> Result<Response> {
        tokio::select! {
            _ = shutdown.cancelled() => bail!("{operation} cancelled"),
            response = request.send() => response.with_context(|| format!("{operation} failed")),
        }
    }
}

/// Read a bounded JSON response. Invalid JSON is `None`; callers decide whether the
/// particular status requires a payload. Token bodies are never logged or embedded in
/// errors.
async fn read_body(mut response: Response) -> Result<Option<Value>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_AUTH_BODY_BYTES as u64)
    {
        bail!("Twitch authorization response exceeds body limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_AUTH_BODY_BYTES {
            bail!("Twitch authorization response exceeds body limit");
        }
        body.extend_from_slice(&chunk);
    }
    if body.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_slice(&body).ok())
}

fn required_string(body: &Value, field: &str) -> Result<String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("Twitch authorization response omitted {field}"))
}

fn required_positive(body: &Value, field: &str) -> Result<i64> {
    body.get(field)
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .with_context(|| format!("Twitch authorization response omitted {field}"))
}

fn message_code(body: &Value) -> Option<String> {
    ["message", "error"]
        .iter()
        .find_map(|field| body.get(*field).and_then(Value::as_str))
        .map(str::to_ascii_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::{Form, State},
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use chrono::Utc;
    use rusqlite::params;
    use serde_json::json;
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{Arc, Mutex as StdMutex},
    };
    use tempfile::TempDir;

    const KEY: [u8; 32] = [42; 32];
    const USER: &str = "999";
    const LOGIN: &str = "smokebot";
    const TEST_CLIENT: &str = "test-client";

    #[derive(Clone, Copy, Debug)]
    enum RefreshMode {
        Rotate,
        Invalid,
        Unavailable,
    }

    #[derive(Clone, Copy, Debug)]
    enum ValidateMode {
        Ok,
        Unauthorized,
        Unavailable,
        WrongClient,
        WrongUser,
        WrongLogin,
        MissingScope,
    }

    #[derive(Default)]
    struct Mock {
        device_calls: usize,
        token_calls: usize,
        refresh_calls: usize,
        validate_calls: usize,
        device_polls: VecDeque<&'static str>,
        device_always_pending: bool,
        device_expires_in: i64,
        refresh_script: VecDeque<RefreshMode>,
        validate_script: VecDeque<ValidateMode>,
        user_id: String,
        login: String,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                device_expires_in: 60,
                user_id: USER.into(),
                login: LOGIN.into(),
                ..Self::default()
            }
        }
    }

    #[derive(Clone)]
    struct MockHandle(Arc<StdMutex<Mock>>);

    #[derive(Clone, Copy, Debug, Default)]
    struct Counts {
        device: usize,
        token: usize,
        refresh: usize,
        validate: usize,
    }

    async fn mock_device(
        State(handle): State<MockHandle>,
        Form(form): Form<BTreeMap<String, String>>,
    ) -> Response {
        let mut mock = handle.0.lock().unwrap();
        mock.device_calls += 1;
        assert_eq!(form.get("client_id").map(String::as_str), Some(TEST_CLIENT));
        assert_eq!(form.get("scopes").map(String::as_str), Some(SCOPE_STRING));
        Json(json!({
            "device_code": "device-code",
            "expires_in": mock.device_expires_in,
            "interval": 1,
            "user_code": "ABCDEFGH",
            "verification_uri": "https://www.twitch.tv/activate?public=true&device-code=ABCDEFGH",
        }))
        .into_response()
    }

    async fn mock_token(
        State(handle): State<MockHandle>,
        Form(form): Form<BTreeMap<String, String>>,
    ) -> Response {
        let mut mock = handle.0.lock().unwrap();
        mock.token_calls += 1;
        match form.get("grant_type").map(String::as_str) {
            Some(DEVICE_GRANT_TYPE) => {
                assert_eq!(
                    form.get("device_code").map(String::as_str),
                    Some("device-code")
                );
                let message = mock.device_polls.pop_front();
                if mock.device_always_pending || message.is_some() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "status": 400,
                            "message": message.unwrap_or("authorization_pending"),
                        })),
                    )
                        .into_response();
                }
                Json(json!({
                    "access_token": "device-access",
                    "refresh_token": "device-refresh",
                    "expires_in": 14400,
                    "scope": BOT_SCOPES,
                    "token_type": "bearer",
                }))
                .into_response()
            }
            Some("refresh_token") => {
                mock.refresh_calls += 1;
                assert_eq!(form.get("client_id").map(String::as_str), Some(TEST_CLIENT));
                assert_eq!(
                    form.get("client_secret").map(String::as_str),
                    Some("test-secret")
                );
                match mock
                    .refresh_script
                    .pop_front()
                    .unwrap_or(RefreshMode::Rotate)
                {
                    RefreshMode::Rotate => Json(json!({
                        "access_token": format!("rotated-access-{}", mock.refresh_calls),
                        "refresh_token": format!("rotated-refresh-{}", mock.refresh_calls),
                        "expires_in": 14400,
                        "token_type": "bearer",
                    }))
                    .into_response(),
                    RefreshMode::Invalid => (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"status": 400, "message": "Invalid refresh token"})),
                    )
                        .into_response(),
                    RefreshMode::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                }
            }
            _ => StatusCode::BAD_REQUEST.into_response(),
        }
    }

    async fn mock_validate(State(handle): State<MockHandle>, headers: HeaderMap) -> Response {
        let mut mock = handle.0.lock().unwrap();
        mock.validate_calls += 1;
        let token = headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("OAuth "))
            .unwrap_or_default();
        assert!(!token.is_empty());
        match mock.validate_script.pop_front().unwrap_or(ValidateMode::Ok) {
            ValidateMode::Ok => Json(json!({
                "client_id": TEST_CLIENT,
                "login": mock.login.as_str(),
                "scopes": BOT_SCOPES,
                "user_id": mock.user_id.as_str(),
                "expires_in": 14400,
            }))
            .into_response(),
            ValidateMode::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"status": 401, "message": "invalid access token"})),
            )
                .into_response(),
            ValidateMode::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            ValidateMode::WrongClient => Json(json!({
                "client_id": "other-client",
                "login": mock.login.as_str(),
                "scopes": BOT_SCOPES,
                "user_id": mock.user_id.as_str(),
                "expires_in": 14400,
            }))
            .into_response(),
            ValidateMode::WrongUser => Json(json!({
                "client_id": TEST_CLIENT,
                "login": mock.login.as_str(),
                "scopes": BOT_SCOPES,
                "user_id": "111",
                "expires_in": 14400,
            }))
            .into_response(),
            ValidateMode::WrongLogin => Json(json!({
                "client_id": TEST_CLIENT,
                "login": "otherbot",
                "scopes": BOT_SCOPES,
                "user_id": mock.user_id.as_str(),
                "expires_in": 14400,
            }))
            .into_response(),
            ValidateMode::MissingScope => Json(json!({
                "client_id": TEST_CLIENT,
                "login": mock.login.as_str(),
                "scopes": ["user:read:chat"],
                "user_id": mock.user_id.as_str(),
                "expires_in": 14400,
            }))
            .into_response(),
        }
    }

    struct Fixture {
        _dir: TempDir,
        config: Arc<Config>,
        store: Store,
        mock: MockHandle,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        async fn new() -> Self {
            let mock = MockHandle(Arc::new(StdMutex::new(Mock::new())));
            let app = Router::new()
                .route("/oauth2/device", post(mock_device))
                .route("/oauth2/token", post(mock_token))
                .route("/oauth2/validate", get(mock_validate))
                .with_state(mock.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let config = Arc::new(Config {
                database_path: ":memory:".into(),
                bind: "127.0.0.1".parse().unwrap(),
                bind_port: 0,
                public_origin: "http://website.test".into(),
                api_origin: "http://api.test".into(),
                cookie_secure: false,
                debug_mode: false,
                twitch_client_id: TEST_CLIENT.into(),
                twitch_client_secret: "test-secret".into(),
                twitch_redirect_uri: "http://api.test/oauth2/twitch-redirect".into(),
                osu_client_id: "test-client".into(),
                osu_client_secret: "test-secret".into(),
                osu_redirect_uri: "http://api.test/oauth2/osu-redirect".into(),
                twitch_eventsub_url: "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30"
                    .into(),
                twitch_token_key: Some(KEY),
                twitch_api_base: format!("{base}/helix"),
                twitch_oauth_base: format!("{base}/oauth2"),
                osu_api_base: format!("{base}/osu"),
                osu_oauth_base: format!("{base}/osu-oauth"),
                dry_run: true,
                reconcile_interval_secs: 600,
                eventsub_channels_per_shard: 100,
                eventsub_max_shards: 16,
                request_workers: 8,
                request_queue_capacity: 128,
                http_timeout_secs: 5,
                session_ttl_secs: 86400,
                oauth_ttl_secs: 600,
                bot_enabled: false,
                migration_active: false,
                migration_cutoff_at: None,
                migration_grant_grace_secs: 0,
                internal_api_token: None,
            });
            let dir = TempDir::new().unwrap();
            let store = Store::open(dir.path().join("auth.sqlite3")).await.unwrap();
            Self {
                _dir: dir,
                config,
                store,
                mock,
                server,
            }
        }

        fn auth(&self) -> TwitchAuth {
            TwitchAuth::new(self.config.clone(), self.store.clone()).unwrap()
        }

        fn auth_with_key(&self, key: [u8; 32]) -> TwitchAuth {
            let mut config = (*self.config).clone();
            config.twitch_token_key = Some(key);
            TwitchAuth::new(Arc::new(config), self.store.clone()).unwrap()
        }

        fn queue_device_polls(&self, messages: &[&'static str]) {
            self.mock
                .0
                .lock()
                .unwrap()
                .device_polls
                .extend(messages.iter().copied());
        }

        fn queue_validate(&self, modes: &[ValidateMode]) {
            self.mock
                .0
                .lock()
                .unwrap()
                .validate_script
                .extend(modes.iter().copied());
        }

        fn queue_refresh(&self, modes: &[RefreshMode]) {
            self.mock
                .0
                .lock()
                .unwrap()
                .refresh_script
                .extend(modes.iter().copied());
        }

        fn set_device_pending(&self, pending: bool, expires_in: i64) {
            let mut mock = self.mock.0.lock().unwrap();
            mock.device_always_pending = pending;
            mock.device_expires_in = expires_in;
        }

        fn counts(&self) -> Counts {
            let mock = self.mock.0.lock().unwrap();
            Counts {
                device: mock.device_calls,
                token: mock.token_calls,
                refresh: mock.refresh_calls,
                validate: mock.validate_calls,
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn seed(
            &self,
            key: [u8; 32],
            user_id: &str,
            login: &str,
            access: &str,
            refresh: &str,
            expires_at: i64,
            validated_at: i64,
        ) {
            let ciphertext = crypto::encrypt(
                &key,
                user_id,
                &Credentials {
                    access_token: access.into(),
                    refresh_token: refresh.into(),
                },
            )
            .unwrap();
            let scopes = serde_json::to_string(&BOT_SCOPES).unwrap();
            let (user_id, login) = (user_id.to_owned(), login.to_owned());
            self.store
                .call(move |connection| {
                    connection.execute(
                        "INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision) \
                         VALUES(1,?1,?2,?3,?4,?5,?6,1) \
                         ON CONFLICT(singleton) DO UPDATE SET user_id=excluded.user_id,login=excluded.login, \
                           ciphertext=excluded.ciphertext,expires_at=excluded.expires_at, \
                           scopes_json=excluded.scopes_json,validated_at=excluded.validated_at,revision=1",
                        params![user_id, login, ciphertext, expires_at, scopes, validated_at],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn device_grant_waits_for_pending_and_persists_only_ciphertext() {
        let fixture = Fixture::new().await;
        fixture.queue_device_polls(&["authorization_pending"]);
        let identity = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            "SmokeBot",
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(identity.user_id, USER);
        assert_eq!(identity.login, LOGIN);
        let counts = fixture.counts();
        assert_eq!(counts.device, 1);
        assert_eq!(counts.token, 2);
        assert_eq!(counts.refresh, 0);
        assert_eq!(counts.validate, 1);

        let auth = fixture.auth();
        let grant = auth.load().await.unwrap().unwrap();
        assert_eq!(grant.user_id, USER);
        assert_eq!(grant.login, LOGIN);
        assert!(grant.validated_at > 0);
        assert!(grant.expires_at > Utc::now().timestamp());
        assert_eq!(
            grant.scopes,
            BOT_SCOPES
                .iter()
                .map(|scope| scope.to_string())
                .collect::<Vec<_>>()
        );
        for secret in [b"device-access".as_slice(), b"device-refresh".as_slice()] {
            assert!(
                !grant
                    .ciphertext
                    .windows(secret.len())
                    .any(|window| window == secret),
                "plaintext token leaked into the database"
            );
        }
        let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
        assert_eq!(credentials.access_token, "device-access");
        assert_eq!(credentials.refresh_token, "device-refresh");
    }

    #[tokio::test]
    async fn device_grant_rejects_a_browser_account_that_does_not_match() {
        let fixture = Fixture::new().await;
        let error = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            "differentbot",
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("different login"));
        assert!(fixture.auth().load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn device_grant_requires_replace_for_a_different_existing_identity() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                "111",
                "oldbot",
                "old-access",
                "old-refresh",
                now + 3600,
                now,
            )
            .await;
        let error = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            LOGIN,
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("--replace"));
        let unchanged = fixture.auth().load().await.unwrap().unwrap();
        assert_eq!(unchanged.user_id, "111");
        assert_eq!(unchanged.login, "oldbot");

        TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            LOGIN,
            true,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let replaced = fixture.auth().load().await.unwrap().unwrap();
        assert_eq!(replaced.user_id, USER);
        assert_eq!(replaced.login, LOGIN);
    }

    #[tokio::test]
    async fn device_grant_honours_cancellation_and_expiry() {
        let fixture = Fixture::new().await;
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            LOGIN,
            false,
            cancelled,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("cancelled"));
        assert_eq!(fixture.counts().token, 0);
        assert!(fixture.auth().load().await.unwrap().is_none());

        fixture.set_device_pending(true, 1);
        let error = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            LOGIN,
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("expired"));
        assert!(fixture.auth().load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn device_grant_applies_slow_down_to_polling() {
        let fixture = Fixture::new().await;
        fixture.queue_device_polls(&["slow_down"]);
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            cancel.cancel();
        });
        let error = TwitchAuth::authorize_bot(
            fixture.config.clone(),
            fixture.store.clone(),
            LOGIN,
            false,
            shutdown,
        )
        .await
        .unwrap_err();
        canceller.await.unwrap();
        assert!(format!("{error:#}").contains("cancelled"));
        assert_eq!(fixture.counts().token, 1);
        assert!(fixture.auth().load().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn expiring_grant_rotates_both_tokens_and_validates_once() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now + 10, now)
            .await;
        let auth = fixture.auth();
        let identity = auth
            .ready_bot(&CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.user_id, USER);
        assert_eq!(identity.login, LOGIN);
        let counts = fixture.counts();
        assert_eq!(counts.refresh, 1);
        assert_eq!(counts.validate, 1);
        let grant = auth.load().await.unwrap().unwrap();
        assert!(grant.validated_at > 0);
        assert!(grant.expires_at > now + 3600);
        let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
        assert_eq!(credentials.access_token, "rotated-access-1");
        assert_eq!(credentials.refresh_token, "rotated-refresh-1");
    }

    #[tokio::test]
    async fn concurrent_readiness_shares_one_refresh() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now - 1, now)
            .await;
        let auth = Arc::new(fixture.auth());
        let shutdown = CancellationToken::new();
        let (a, b, c, d) = tokio::join!(
            auth.ready_bot(&shutdown),
            auth.ready_bot(&shutdown),
            auth.ready_bot(&shutdown),
            auth.ready_bot(&shutdown),
        );
        for result in [a, b, c, d] {
            assert_eq!(result.unwrap().unwrap().login, LOGIN);
        }
        let counts = fixture.counts();
        assert_eq!(counts.refresh, 1, "refresh must be single-flight");
        assert_eq!(counts.validate, 1, "cached validation must avoid HTTP");
        let credentials =
            crypto::decrypt(&KEY, USER, &auth.load().await.unwrap().unwrap().ciphertext).unwrap();
        assert_eq!(credentials.access_token, "rotated-access-1");
    }

    #[tokio::test]
    async fn revoked_grant_is_marked_unusable_and_never_hot_loops() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "revoked-access",
                "revoked-refresh",
                now + 3600,
                now - 3700,
            )
            .await;
        fixture.queue_validate(&[ValidateMode::Unauthorized]);
        fixture.queue_refresh(&[RefreshMode::Invalid]);
        let auth = fixture.auth();
        let before = auth.load().await.unwrap().unwrap().ciphertext;
        let error = auth.ready_bot(&CancellationToken::new()).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("refresh token"));
        assert!(!text.contains("revoked-access"));
        assert!(!text.contains("revoked-refresh"));
        let grant = auth.load().await.unwrap().unwrap();
        assert_eq!(grant.validated_at, UNUSABLE);
        assert_eq!(
            grant.ciphertext, before,
            "revocation must preserve evidence"
        );
        let counts = fixture.counts();
        assert_eq!(counts.refresh, 1);
        assert_eq!(counts.validate, 1);

        assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
        assert_eq!(fixture.counts().refresh, 1);
        assert_eq!(fixture.counts().validate, 1);
    }

    #[tokio::test]
    async fn transient_failures_do_not_revoke_or_rewrite_credentials() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "stable-access",
                "stable-refresh",
                now + 3600,
                now - 3700,
            )
            .await;
        fixture.queue_validate(&[ValidateMode::Unavailable]);
        let auth = fixture.auth();
        let before = auth.load().await.unwrap().unwrap().ciphertext;
        assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
        let grant = auth.load().await.unwrap().unwrap();
        assert_eq!(grant.validated_at, now - 3700);
        assert_eq!(grant.ciphertext, before);
        assert_eq!(fixture.counts().refresh, 0);

        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "stable-access",
                "stable-refresh",
                now + 10,
                now,
            )
            .await;
        fixture.queue_refresh(&[RefreshMode::Unavailable]);
        let before = auth.load().await.unwrap().unwrap().ciphertext;
        assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
        let grant = auth.load().await.unwrap().unwrap();
        assert_eq!(grant.validated_at, now);
        assert_eq!(grant.ciphertext, before);
        assert_eq!(fixture.counts().validate, 1);
    }

    #[tokio::test]
    async fn wrong_key_and_tampered_ciphertext_fail_closed() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "secure-access",
                "secure-refresh",
                now + 3600,
                now - 3700,
            )
            .await;
        let before = fixture.auth().load().await.unwrap().unwrap().ciphertext;
        let error = fixture
            .auth_with_key([9; 32])
            .ready_bot(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("TWITCH_TOKEN_KEY"));
        let unchanged = fixture.auth().load().await.unwrap().unwrap();
        assert_eq!(unchanged.ciphertext, before);
        assert_eq!(unchanged.validated_at, now - 3700);

        fixture
            .store
            .call(|connection| {
                let mut ciphertext: Vec<u8> = connection.query_row(
                    "SELECT ciphertext FROM twitch_bot_auth WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )?;
                ciphertext[0] ^= 0xff;
                connection.execute(
                    "UPDATE twitch_bot_auth SET ciphertext=?1 WHERE singleton=1",
                    [ciphertext],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let error = fixture
            .auth()
            .ready_bot(&CancellationToken::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("decrypt"));
        let tampered = fixture.auth().load().await.unwrap().unwrap();
        assert_ne!(tampered.validated_at, UNUSABLE);
        assert_eq!(fixture.counts().refresh + fixture.counts().validate, 0);
    }

    #[tokio::test]
    async fn validation_identity_client_and_scope_mismatches_fail_closed() {
        let fixture = Fixture::new().await;
        let auth = fixture.auth();
        let cases = [
            ValidateMode::WrongClient,
            ValidateMode::WrongUser,
            ValidateMode::WrongLogin,
            ValidateMode::MissingScope,
        ];
        for mode in cases {
            let now = Utc::now().timestamp();
            fixture
                .seed(
                    KEY,
                    USER,
                    LOGIN,
                    "mismatch-access",
                    "mismatch-refresh",
                    now + 3600,
                    now - 3700,
                )
                .await;
            fixture.queue_validate(&[mode]);
            let error = auth.ready_bot(&CancellationToken::new()).await.unwrap_err();
            assert!(
                format!("{error:#}").contains("Twitch bot"),
                "unexpected error for {mode:?}: {error:#}"
            );
            let grant = auth.load().await.unwrap().unwrap();
            assert_eq!(grant.validated_at, UNUSABLE, "{mode:?} must fail closed");
            assert!(grant.ciphertext.len() > 12);
        }
    }

    #[tokio::test]
    async fn bot_token_access_refreshes_expiring_tokens_and_reacts_to_rejection() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "expired-access",
                "expired-refresh",
                now - 1,
                now,
            )
            .await;
        let auth = fixture.auth();
        let shutdown = CancellationToken::new();
        assert_eq!(
            auth.bot_access_token(&shutdown).await.unwrap(),
            "rotated-access-1"
        );
        assert_eq!(
            auth.bot_access_token(&shutdown).await.unwrap(),
            "rotated-access-1"
        );
        assert_eq!(fixture.counts().refresh, 1);
        assert_eq!(fixture.counts().validate, 0);

        let rejected = auth.bot_access_token(&shutdown).await.unwrap();
        assert_eq!(
            auth.bot_access_token_after_rejection(&rejected, &shutdown)
                .await
                .unwrap(),
            "rotated-access-2"
        );
        assert_eq!(
            auth.bot_access_token_after_rejection(&rejected, &shutdown)
                .await
                .unwrap(),
            "rotated-access-2",
            "a concurrent rotation must not trigger a second refresh"
        );
        assert_eq!(fixture.counts().refresh, 2);
    }

    #[tokio::test]
    async fn rejected_access_token_refreshes_only_after_a_401() {
        let fixture = Fixture::new().await;
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "rejected-access",
                "live-refresh",
                now + 3600,
                now - 3700,
            )
            .await;
        fixture.queue_validate(&[ValidateMode::Unauthorized]);
        let auth = fixture.auth();
        let identity = auth
            .ready_bot(&CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.login, LOGIN);
        let counts = fixture.counts();
        assert_eq!(counts.refresh, 1);
        assert_eq!(counts.validate, 2);
        let grant = auth.load().await.unwrap().unwrap();
        let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
        assert_eq!(credentials.access_token, "rotated-access-1");
    }
}
