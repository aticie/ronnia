//! Bot readiness and access-token access.
//!
//! Every entry point holds the single-flight gate. A refresh transfers ownership of
//! that gate to its spawned refresh-and-persist task, so caller cancellation cannot
//! lose a successful rotation or let a second caller reuse its single-use token.

use super::{
    BotIdentity, MAX_STATE_ATTEMPTS, REFRESH_MARGIN_SECS, TwitchAuth, UNUSABLE,
    VALIDATION_INTERVAL_SECS, credentials::StoredGrant, crypto::Credentials,
    tokens::ValidationOutcome,
};
use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use std::sync::atomic::Ordering;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

impl TwitchAuth {
    /// Wait for a detached refresh to finish its durable write before runtime shutdown.
    pub async fn finish_refresh(&self) {
        let _single_flight = self.refresh.lock().await;
    }

    async fn lock_refresh(&self, shutdown: &CancellationToken) -> Result<OwnedMutexGuard<()>> {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => bail!("Twitch bot authorization cancelled"),
            guard = self.refresh.clone().lock_owned() => Ok(guard),
        }
    }

    /// Return the configured bot identity, validating hourly and refreshing proactively.
    ///
    /// `Ok(None)` means no bot has ever authorized. A persisted unusable/revoked grant is
    /// an error without network traffic, so supervisors back off instead of hot-looping.
    /// Transient provider failures are errors and leave stored evidence untouched.
    pub async fn ready_bot(&self, shutdown: &CancellationToken) -> Result<Option<BotIdentity>> {
        let mut single_flight = self.lock_refresh(shutdown).await?;
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
            let stale = !self.validated_this_process.load(Ordering::Acquire)
                || grant.validated_at <= 0
                || now.saturating_sub(grant.validated_at) >= VALIDATION_INTERVAL_SECS;
            if !expiring && !stale {
                return Ok(Some(BotIdentity {
                    user_id: grant.user_id,
                    login: grant.login,
                }));
            }

            let mut credentials = self.decrypt(&grant)?;
            let mut revision = grant.revision;
            let mut rotated = false;
            if expiring {
                let (guard, rotation) = self
                    .rotate(&grant, credentials, single_flight, shutdown)
                    .await?;
                single_flight = guard;
                let Some((next, next_revision)) = rotation else {
                    continue;
                };
                credentials = next;
                revision = next_revision;
                rotated = true;
            }

            let validation = match self
                .validate_token(&credentials.access_token, shutdown)
                .await?
            {
                ValidationOutcome::Valid(validation) => validation,
                // A 401 is definitive only after one refresh attempt, per Twitch's
                // validation requirement. Transient failures never reach this branch.
                ValidationOutcome::Unauthorized if rotated => {
                    self.revoke(
                        revision,
                        anyhow!(
                            "Twitch rejected the refreshed bot token; run `authorize-bot` again"
                        ),
                    )
                    .await?;
                    continue;
                }
                ValidationOutcome::Unauthorized => {
                    let (guard, rotation) = self
                        .rotate(&grant, credentials, single_flight, shutdown)
                        .await?;
                    single_flight = guard;
                    let Some((next, next_revision)) = rotation else {
                        continue;
                    };
                    credentials = next;
                    revision = next_revision;
                    match self
                        .validate_token(&credentials.access_token, shutdown)
                        .await?
                    {
                        ValidationOutcome::Valid(validation) => validation,
                        ValidationOutcome::Unauthorized => {
                            self.revoke(
                                revision,
                                anyhow!("Twitch revoked the bot token; run `authorize-bot` again"),
                            )
                            .await?;
                            continue;
                        }
                    }
                }
            };

            if let Some(problem) =
                self.validation_problem(&validation, Some(&grant.user_id), Some(&grant.login))
            {
                self.revoke(revision, anyhow!("{problem}; run `authorize-bot` again"))
                    .await?;
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
        self.access_token(shutdown, |grant, _| {
            grant.expires_at.saturating_sub(Utc::now().timestamp()) >= REFRESH_MARGIN_SECS
        })
        .await
    }

    /// Rotate the token after a resource request returned 401. If another caller already
    /// replaced the rejected token, its newer token is returned instead of refreshing twice.
    pub async fn bot_access_token_after_rejection(
        &self,
        rejected: &str,
        shutdown: &CancellationToken,
    ) -> Result<String> {
        self.access_token(shutdown, |_, credentials| {
            credentials.access_token != rejected
        })
        .await
    }

    /// Shared token access loop; `usable` decides whether the stored token still serves.
    async fn access_token(
        &self,
        shutdown: &CancellationToken,
        usable: impl Fn(&StoredGrant, &Credentials) -> bool,
    ) -> Result<String> {
        let mut single_flight = self.lock_refresh(shutdown).await?;
        for _ in 0..MAX_STATE_ATTEMPTS {
            let grant = self.usable_grant().await?;
            let credentials = self.decrypt(&grant)?;
            if usable(&grant, &credentials) {
                return Ok(credentials.access_token);
            }
            let (guard, rotated) = self
                .rotate(&grant, credentials, single_flight, shutdown)
                .await?;
            single_flight = guard;
            match rotated {
                Some((rotated, _)) => return Ok(rotated.access_token),
                None => continue,
            }
        }
        bail!("Twitch bot authorization kept changing concurrently; retry")
    }

    /// Load the stored grant, rejecting a missing or already revoked authorization
    /// without any network traffic.
    async fn usable_grant(&self) -> Result<StoredGrant> {
        let grant = self
            .load()
            .await?
            .ok_or_else(|| anyhow!("Twitch bot is not authorized; run `authorize-bot` first"))?;
        if grant.validated_at == UNUSABLE {
            bail!("Twitch bot authorization is no longer usable; run `authorize-bot` again");
        }
        Ok(grant)
    }
}
