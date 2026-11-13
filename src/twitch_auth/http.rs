//! Cancellation-aware HTTP plumbing shared by the Twitch OAuth endpoints.
//!
//! Every request is abortable, body reads are bounded, and the accessors below encode
//! which fields a caller is allowed to find missing. Response payloads are never logged
//! nor embedded in errors, so token material cannot leak through diagnostics.

use super::{MAX_AUTH_BODY_BYTES, TwitchAuth};
use anyhow::{Context, Result, bail};
use reqwest::{RequestBuilder, Response};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

impl TwitchAuth {
    pub(super) fn endpoint(&self, suffix: &str) -> String {
        format!(
            "{}{suffix}",
            self.config.twitch_oauth_base.trim_end_matches('/')
        )
    }

    /// Client identification carried by token requests; the secret is optional.
    pub(super) fn client_form(&self) -> Vec<(&str, &str)> {
        let mut form = vec![("client_id", self.config.twitch_client_id.as_str())];
        if !self.config.twitch_client_secret.is_empty() {
            form.push(("client_secret", self.config.twitch_client_secret.as_str()));
        }
        form
    }

    pub(super) async fn send(
        &self,
        request: RequestBuilder,
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
/// particular status requires a payload.
pub(super) async fn read_body(mut response: Response) -> Result<Option<Value>> {
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

pub(super) fn required_string(body: &Value, field: &str) -> Result<String> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("Twitch authorization response omitted {field}"))
}

pub(super) fn required_positive(body: &Value, field: &str) -> Result<i64> {
    body.get(field)
        .and_then(Value::as_i64)
        .filter(|value| *value > 0)
        .with_context(|| format!("Twitch authorization response omitted {field}"))
}

/// OAuth error code of a failed token poll, used to drive the device-grant backoff.
pub(super) fn message_code(body: &Value) -> Option<String> {
    ["message", "error"]
        .iter()
        .find_map(|field| body.get(*field).and_then(Value::as_str))
        .map(str::to_ascii_lowercase)
}
