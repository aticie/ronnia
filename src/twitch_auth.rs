//! Twitch user authorization for the chat bot: facade and shared handle.
//!
//! The behaviour lives in the submodules, each with a single responsibility:
//!
//! - `credentials`: AES-256-GCM credential envelopes and revision compare-and-swap writes.
//! - `device`: the interactive OAuth device grant used by the `authorize-bot` CLI.
//! - `http`: bounded, cancellation-aware HTTP plumbing for the Twitch OAuth endpoints.
//! - `lifecycle`: readiness and access-token access behind one single-flight refresh gate.
//! - `tokens`: token rotation, token validation, and scope/identity checks.
//!
//! Security invariants: tokens are only persisted as nonce-prefixed AES-256-GCM
//! ciphertext bound to the user id, the key lives in configuration and never in the
//! database, every row change is a revision compare-and-swap, and nothing here logs,
//! formats, or derives `Debug` for token material.
mod credentials;
mod crypto;
mod device;
mod http;
mod lifecycle;
mod tokens;

use crate::{config::Config, storage::Store};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, redirect::Policy};
use serde::Serialize;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::sync::Mutex;

/// Scopes required for EventSub chat delivery and for sending chat messages as a user.
pub const BOT_SCOPES: [&str; 3] = ["user:read:chat", "user:write:chat", "user:bot"];
/// Space-separated form of [`BOT_SCOPES`], as the OAuth endpoints expect it.
const SCOPE_STRING: &str = "user:read:chat user:write:chat user:bot";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const VALIDATION_INTERVAL_SECS: i64 = 60 * 60;
const REFRESH_MARGIN_SECS: i64 = 60;
const MAX_AUTH_BODY_BYTES: usize = 64 * 1024;
/// Bound on compare-and-swap retries when another writer keeps advancing the row.
const MAX_STATE_ATTEMPTS: usize = 4;
/// `validated_at` sentinel: the stored grant can no longer be used, but its encrypted
/// evidence is retained until the operator reauthorizes (never delete by accident).
const UNUSABLE: i64 = -1;

#[derive(Clone, Debug, Serialize)]
pub struct BotIdentity {
    pub user_id: String,
    pub login: String,
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
}

/// CLI entry point for the browser-based device grant.
///
/// The browser authorizes the account; this process only receives the resulting tokens.
pub async fn authorize_bot(
    config: Arc<Config>,
    store: Store,
    expected_login: &str,
    replace: bool,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<BotIdentity> {
    TwitchAuth::authorize_bot(config, store, expected_login, replace, shutdown).await
}

#[cfg(test)]
mod tests;
