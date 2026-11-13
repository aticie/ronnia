//! Interactive OAuth device grant used by the `authorize-bot` CLI.
//!
//! The browser authorizes the account; this process only ever sees the resulting tokens,
//! never a pasted credential. The verification URL and user code are printed to stdout,
//! while the device code and tokens stay off every log.

use super::{
    BotIdentity, DEVICE_GRANT_TYPE, SCOPE_STRING, TwitchAuth,
    crypto::Credentials,
    http::{message_code, read_body, required_positive, required_string},
    tokens::{ValidationOutcome, token_grant},
};
use crate::{config::Config, storage::Store};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::StatusCode;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

impl TwitchAuth {
    /// Interactive OAuth device grant used by the `authorize-bot` CLI.
    pub async fn authorize_bot(
        config: Arc<Config>,
        store: Store,
        expected_login: &str,
        replace: bool,
        shutdown: CancellationToken,
    ) -> Result<BotIdentity> {
        Self::new(config, store)?
            .authorize(expected_login, replace, &shutdown)
            .await
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

        let credentials = self.request_device_grant(shutdown).await?;
        let validation = match self
            .validate_token(&credentials.access_token, shutdown)
            .await?
        {
            ValidationOutcome::Valid(validation) => validation,
            ValidationOutcome::Unauthorized => {
                bail!("Twitch rejected the newly authorized bot token")
            }
        };
        if let Some(problem) = self.validation_problem(&validation, None, Some(&expected_login)) {
            bail!("{problem}; the browser account did not match the configured bot");
        }
        let login = validation.login.to_ascii_lowercase();
        let expires_at = Utc::now().timestamp().saturating_add(validation.expires_in);
        self.store_grant(
            &validation.user_id,
            &login,
            &validation.scopes,
            expires_at,
            &credentials,
            replace,
        )
        .await?;
        Ok(BotIdentity {
            user_id: validation.user_id,
            login,
        })
    }

    /// Print the operator instructions, then poll until the device grant completes.
    async fn request_device_grant(&self, shutdown: &CancellationToken) -> Result<Credentials> {
        let response = self
            .send(
                self.client.post(self.endpoint("/device")).form(&[
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
        loop {
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

            let mut form = vec![
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", device_code.as_str()),
                ("scopes", SCOPE_STRING),
            ];
            form.extend(self.client_form());
            let response = match self
                .send(
                    self.client.post(self.endpoint("/token")).form(&form),
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
                    return token_grant(&body, None).map(|grant| grant.credentials);
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
        }
    }
}
