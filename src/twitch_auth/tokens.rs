//! Twitch token rotation and validation.
//!
//! Owns the `/oauth2/token` and `/oauth2/validate` exchanges, the transient-versus-
//! definitive error split, and the checks that a validated grant really belongs to this
//! bot with the scopes it needs.

use super::{
    BOT_SCOPES, TwitchAuth,
    credentials::StoredGrant,
    crypto::Credentials,
    http::{read_body, required_positive, required_string},
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use reqwest::StatusCode;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Rotated tokens plus their expiry. Refresh tokens are single use, so the caller must
/// persist this before doing anything else that can fail.
pub(super) struct TokenGrant {
    pub(super) credentials: Credentials,
    pub(super) expires_at: i64,
}

pub(super) struct Validation {
    pub(super) client_id: String,
    pub(super) user_id: String,
    pub(super) login: String,
    pub(super) scopes: Vec<String>,
    pub(super) expires_in: i64,
}

pub(super) enum ValidationOutcome {
    Valid(Validation),
    Unauthorized,
}

pub(super) enum RefreshError {
    Invalid(anyhow::Error),
    Transient(anyhow::Error),
}

impl TwitchAuth {
    /// Rotate and persist the stored tokens, turning a definitive revocation into an
    /// error. `Ok(None)` means another writer advanced the row; the caller must reload.
    pub(super) async fn rotate(
        &self,
        grant: &StoredGrant,
        current: Credentials,
        shutdown: &CancellationToken,
    ) -> Result<Option<(Credentials, i64)>> {
        match self.refresh_tokens(&current, shutdown).await {
            Ok(rotated) => Ok(self
                .persist_refresh(
                    grant.revision,
                    &grant.user_id,
                    &grant.scopes,
                    rotated.expires_at,
                    &rotated.credentials,
                )
                .await?
                .map(|revision| (rotated.credentials, revision))),
            Err(RefreshError::Invalid(error)) => {
                self.revoke(grant.revision, error).await?;
                Ok(None)
            }
            Err(RefreshError::Transient(error)) => Err(error),
        }
    }

    /// Mark the stored grant unusable. `Ok(())` means the row was already rewritten, so
    /// the caller simply retries; `Err` carries the definitive failure to report.
    pub(super) async fn revoke(&self, revision: i64, error: anyhow::Error) -> Result<()> {
        if self.mark_unusable(revision).await? {
            Err(error)
        } else {
            Ok(())
        }
    }

    async fn refresh_tokens(
        &self,
        current: &Credentials,
        shutdown: &CancellationToken,
    ) -> std::result::Result<TokenGrant, RefreshError> {
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", current.refresh_token.as_str()),
        ];
        form.extend(self.client_form());
        let response = self
            .send(
                self.client.post(self.endpoint("/token")).form(&form),
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
                token_grant(&body, Some(current)).map_err(RefreshError::Transient)
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

    pub(super) async fn validate_token(
        &self,
        token: &str,
        shutdown: &CancellationToken,
    ) -> Result<ValidationOutcome> {
        let response = self
            .send(
                self.client
                    .get(self.endpoint("/validate"))
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
                Ok(ValidationOutcome::Valid(Validation {
                    client_id: required_string(&body, "client_id")?,
                    user_id: required_string(&body, "user_id")?,
                    login: required_string(&body, "login")?,
                    scopes,
                    expires_in: required_positive(&body, "expires_in")?,
                }))
            }
            StatusCode::UNAUTHORIZED => Ok(ValidationOutcome::Unauthorized),
            status => bail!("Twitch token validation temporarily failed (HTTP {status})"),
        }
    }

    /// Explain why a validated token must not be used for this bot, if it must not.
    pub(super) fn validation_problem(
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
}

/// Parse an RFC 6749 token response. `current` supplies the refresh token when the
/// provider does not rotate it, which the RFC permits.
pub(super) fn token_grant(body: &Value, current: Option<&Credentials>) -> Result<TokenGrant> {
    let refreshed = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    let refresh_token = match refreshed {
        Some(token) => token,
        None => current
            .map(|credentials| credentials.refresh_token.clone())
            .context("Twitch authorization response omitted refresh_token")?,
    };
    Ok(TokenGrant {
        credentials: Credentials {
            access_token: required_string(body, "access_token")?,
            refresh_token,
        },
        expires_at: Utc::now()
            .timestamp()
            .saturating_add(required_positive(body, "expires_in")?),
    })
}
