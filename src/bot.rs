//! Bot orchestration: bot-grant supervision, chat-feedback pacing and process
//! wiring. Transport, dispatch, delivery, maintenance and request execution live
//! in their own modules; this file only starts and restarts them.

use crate::{
    config::Config,
    parser::{Beatmap, BeatmapKind},
    providers::Providers,
    storage::Store,
    twitch_auth::{BotIdentity, TwitchAuth},
};
use anyhow::{Result, bail};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

mod backoff;
mod conduit;
mod delivery;
mod dispatch;
mod maintenance;
mod requests;
mod transport;

use self::{
    backoff::Backoff,
    delivery::{Echo, sanitize_chat},
    dispatch::Runtime,
    requests::{RequestIngress, process_requests},
};

/// Revalidate the managed bot grant well inside Twitch's validation window.
const AUTH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const AUTH_DEADLINE: Duration = Duration::from_secs(20);
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_CAP: Duration = Duration::from_secs(30);
/// Twitch allows only a small number of chat messages per window; feedback is
/// paced instead of being retried after a 429.
const FEEDBACK_PACE: Duration = Duration::from_millis(1500);

/// The bot worker never takes the API down. Transport failures are logged and
/// retried with capped jitter; a missing grant leaves the process waiting while
/// the HTTP API keeps serving.
pub async fn run(config: Arc<Config>, store: Store, shutdown: CancellationToken) -> Result<()> {
    let providers = Providers::new(config.clone(), shutdown.clone())?;
    let auth = TwitchAuth::new(config.clone(), store.clone())?;
    let clock = delivery::Clock::new();
    store
        .call(|db| {
            db.execute("UPDATE deliveries SET status='uncertain',error='process stopped during send; not replayed' WHERE status='sending'", [])?;
            db.execute("UPDATE deliveries SET status='failed',error='process stopped before send; safe to submit a new request' WHERE status='pending'", [])?;
            Ok(())
        })
        .await?;
    let capacity = config.request_queue_capacity;
    let permits = Arc::new(tokio::sync::Semaphore::new(capacity));
    let (requests_tx, requests_rx) = mpsc::channel(capacity);
    let (echo_tx, echo_rx) = mpsc::channel(capacity);
    let (identity_tx, identity_rx) = watch::channel::<Option<String>>(None);
    tokio::join!(
        process_requests(
            config.clone(),
            store.clone(),
            providers.clone(),
            requests_rx,
            echo_tx,
            shutdown.clone(),
            clock,
        ),
        send_feedback(
            config.clone(),
            providers.clone(),
            auth.clone(),
            echo_rx,
            identity_rx,
            shutdown.clone(),
        ),
        supervise(
            config,
            store,
            providers,
            auth,
            identity_tx,
            RequestIngress {
                requests: requests_tx,
                permits,
            },
            shutdown,
        ),
    );
    Ok(())
}

pub async fn backfill(config: Arc<Config>, store: Store) -> Result<()> {
    let providers = Providers::new(config, CancellationToken::new())?;
    let mut after = 0;
    let mut failures = 0u64;
    loop {
        let ids = store.missing_beatmap_ids(after, 100).await?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            after = id;
            match providers
                .get_beatmap(&Beatmap {
                    id,
                    kind: BeatmapKind::Map,
                    mods: String::new(),
                })
                .await
            {
                Ok((map, _)) => {
                    store.cache_put(map).await?;
                    tracing::info!(id, "backfilled beatmap");
                }
                Err(error) => {
                    failures += 1;
                    tracing::error!(id, error = %error, "backfill failed; continuing remaining IDs");
                }
            }
        }
    }
    if failures != 0 {
        bail!("backfill completed with {failures} failed beatmaps; rerun to retry missing IDs");
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Cancelled,
    Restart,
}

async fn supervise(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
    auth: TwitchAuth,
    identity_tx: watch::Sender<Option<String>>,
    ingress: RequestIngress,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP);
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let Some(identity) = wait_for_identity(&auth, &shutdown).await else {
            identity_tx.send_replace(None);
            return;
        };
        identity_tx.send_replace(Some(identity.user_id.clone()));
        let started = Instant::now();
        let result = Runtime::new(
            config.clone(),
            store.clone(),
            providers.clone(),
            auth.clone(),
            identity,
            ingress.clone(),
        )
        .await;
        let result = match result {
            Ok(mut runtime) => runtime.run(&shutdown).await,
            Err(error) => Err(error),
        };
        identity_tx.send_replace(None);
        if shutdown.is_cancelled() {
            return;
        }
        match result {
            Ok(Outcome::Cancelled) => return,
            Ok(Outcome::Restart) => {
                tracing::info!("restarting the EventSub runtime for the updated bot authorization");
                backoff.reset();
            }
            Err(error) => {
                if started.elapsed() > Duration::from_secs(120) {
                    backoff.reset();
                }
                let delay = backoff.next_delay();
                tracing::error!(
                    error = %error,
                    retry_ms = delay.as_millis(),
                    "EventSub transport degraded; reconnecting without deleting accounts"
                );
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = sleep(delay) => {}
                }
            }
        }
    }
}

/// Wait for a usable managed bot grant. Missing authorization is reported once
/// and polled slowly so an `authorize-bot` run is picked up without a restart.
async fn wait_for_identity(auth: &TwitchAuth, shutdown: &CancellationToken) -> Option<BotIdentity> {
    let mut reported = false;
    let mut backoff = Backoff::new(RETRY_BASE, RETRY_CAP);
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        let result = tokio::select! {
            _ = shutdown.cancelled() => return None,
            result = auth.ready_bot(shutdown) => result,
        };
        match result {
            Ok(Some(identity)) => {
                if reported {
                    tracing::info!(login = %identity.login, "Twitch bot authorization is available");
                }
                return Some(identity);
            }
            Ok(None) => {
                if !reported {
                    tracing::warn!(
                        "no Twitch bot authorization stored; run the authorize-bot command. \
                         The HTTP API stays available while the bot waits."
                    );
                    reported = true;
                }
                backoff.reset();
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = sleep(Duration::from_secs(30)) => {}
                }
            }
            Err(error) => {
                if !reported {
                    tracing::warn!(
                        error = %error,
                        "stored Twitch bot authorization is not usable; run authorize-bot to reauthorize"
                    );
                    reported = true;
                } else {
                    tracing::debug!(error = %error, "Twitch bot authorization still unusable");
                }
                let delay = backoff.next_delay().max(Duration::from_secs(30));
                tokio::select! {
                    _ = shutdown.cancelled() => return None,
                    _ = sleep(delay) => {}
                }
            }
        }
    }
}

async fn send_feedback(
    config: Arc<Config>,
    providers: Providers,
    auth: TwitchAuth,
    mut echoes: mpsc::Receiver<Echo>,
    identities: watch::Receiver<Option<String>>,
    shutdown: CancellationToken,
) {
    let mut next_send = Instant::now();
    loop {
        let echo = tokio::select! {
            _ = shutdown.cancelled() => return,
            echo = echoes.recv() => echo,
        };
        let Some(echo) = echo else { return };
        if config.dry_run {
            continue;
        }
        let Some(sender) = identities.borrow().clone() else {
            continue;
        };
        let text = sanitize_chat(&echo.text, 450);
        if text.is_empty() {
            continue;
        }
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = sleep_until(next_send) => {}
        }
        next_send = Instant::now() + FEEDBACK_PACE;
        if let Err(error) = providers
            .send_twitch_message(&auth, echo.channel_id, &sender, &text)
            .await
        {
            tracing::warn!(
                channel_id = echo.channel_id,
                uncertain = error.uncertain,
                error = %error,
                "Twitch chat feedback was not delivered"
            );
        }
    }
}

/// A single in-flight authorization check, independent of event consumption.
async fn validate_identity(
    auth: TwitchAuth,
    bot_id: String,
    shutdown: CancellationToken,
) -> Outcome {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Outcome::Cancelled,
            _ = sleep(AUTH_INTERVAL) => {},
        }
        let result = tokio::select! {
            _ = shutdown.cancelled() => return Outcome::Cancelled,
            result = timeout(AUTH_DEADLINE, auth.ready_bot(&shutdown)) => result,
        };
        match result {
            Ok(Ok(Some(current))) if current.user_id == bot_id => {}
            Ok(Ok(Some(changed))) => {
                tracing::info!(login = %changed.login, "Twitch bot identity changed; restarting the transport");
                return Outcome::Restart;
            }
            Ok(Ok(None)) => {
                tracing::warn!("Twitch bot authorization was revoked; waiting for authorize-bot");
                return Outcome::Restart;
            }
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "Twitch bot revalidation failed; returning to the authorization wait");
                return Outcome::Restart;
            }
            Err(_) => tracing::warn!("Twitch bot revalidation timed out"),
        }
    }
}

#[cfg(test)]
mod tests;
