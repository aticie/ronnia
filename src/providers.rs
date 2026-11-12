use crate::{
    config::Config,
    parser::{Beatmap, BeatmapKind},
    twitch_auth::TwitchAuth,
};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Response, StatusCode};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

pub struct LiveStream {
    pub id: i64,
    pub login: String,
    pub category_id: String,
}

#[derive(Clone, Copy)]
enum Scope {
    Twitch,
    Public,
    Delegate,
}
struct Token {
    access: String,
    expires: Instant,
}
struct Tokens {
    value: Mutex<Option<Token>>,
    scope: Scope,
}

/// Chat failures carry the delivery certainty, not merely the HTTP error class.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ChatError {
    pub uncertain: bool,
    pub status: Option<u16>,
    pub message: String,
}

#[derive(Clone)]
pub struct Providers {
    config: Arc<Config>,
    client: Client,
    shutdown: CancellationToken,
    twitch: Arc<Tokens>,
    public: Arc<Tokens>,
    delegate: Arc<Tokens>,
    osu_next: Arc<Mutex<Instant>>,
}

async fn body_json(mut response: Response) -> Result<Value> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        bail!("provider response exceeds body limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            bail!("provider response exceeds body limit");
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).context("invalid provider JSON")
}

fn provider_operation(endpoint: &str) -> &'static str {
    match endpoint.split('/').next() {
        Some("beatmaps") => "beatmaps",
        Some("beatmapsets") => "beatmapsets",
        Some("chat") => "chat",
        Some("eventsub") => "eventsub",
        Some("streams") => "streams",
        Some("users") => "users",
        _ => "other",
    }
}

fn status_outcome(status: StatusCode) -> &'static str {
    match status.as_u16() / 100 {
        1 => "1xx",
        2 => "2xx",
        3 => "3xx",
        4 => "4xx",
        _ => "5xx",
    }
}

fn record_provider_attempt(
    provider: &'static str,
    operation: &'static str,
    method: &str,
    outcome: &'static str,
    elapsed: Duration,
) {
    sentry::Hub::with_active(|hub| {
        let method = match method {
            "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "OPTIONS" | "CONNECT" | "PATCH"
            | "TRACE" => method,
            _ => "OTHER",
        };
        hub.capture_metric(
            sentry::metrics::counter("ronnia.provider.attempts", 1)
                .attribute("provider", provider)
                .attribute("operation", operation)
                .attribute("http.method", method)
                .attribute("outcome", outcome),
        );
        hub.capture_metric(
            sentry::metrics::distribution(
                "ronnia.provider.headers_latency",
                elapsed.as_secs_f64() * 1000.0,
            )
            .unit(sentry::protocol::Unit::Millisecond)
            .attribute("provider", provider)
            .attribute("operation", operation)
            .attribute("http.method", method)
            .attribute("outcome", outcome),
        );
    });
}

fn record_provider_retry(
    provider: &'static str,
    operation: &'static str,
    reason: &'static str,
    delay: Option<Duration>,
) {
    sentry::Hub::with_active(|hub| {
        hub.capture_metric(
            sentry::metrics::counter("ronnia.provider.retries", 1)
                .attribute("provider", provider)
                .attribute("operation", operation)
                .attribute("reason", reason),
        );
        if let Some(delay) = delay {
            hub.capture_metric(
                sentry::metrics::distribution(
                    "ronnia.provider.retry_delay",
                    delay.as_secs_f64() * 1000.0,
                )
                .unit(sentry::protocol::Unit::Millisecond)
                .attribute("provider", provider)
                .attribute("operation", operation)
                .attribute("reason", reason),
            );
        }
    });
}

impl Providers {
    pub fn new(config: Arc<Config>, shutdown: CancellationToken) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.http_timeout_secs.max(1)))
            .connect_timeout(Duration::from_secs(config.http_timeout_secs.clamp(1, 15)))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ronnia/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            config,
            client,
            shutdown,
            twitch: Arc::new(Tokens {
                value: Mutex::new(None),
                scope: Scope::Twitch,
            }),
            public: Arc::new(Tokens {
                value: Mutex::new(None),
                scope: Scope::Public,
            }),
            delegate: Arc::new(Tokens {
                value: Mutex::new(None),
                scope: Scope::Delegate,
            }),
            osu_next: Arc::new(Mutex::new(Instant::now())),
        })
    }

    async fn pause(&self, duration: Duration) -> Result<()> {
        tokio::select! { _ = self.shutdown.cancelled() => bail!("provider request cancelled"), _ = sleep(duration) => Ok(()) }
    }

    async fn token(&self, state: &Tokens) -> Result<String> {
        // Keeping only the refresh inside this lock makes refresh single-flight.
        // Resource requests and 401 invalidation never run while it is held.
        let mut guard = state.value.lock().await;
        if let Some(token) = guard.as_ref().filter(|t| t.expires > Instant::now()) {
            return Ok(token.access.clone());
        }
        let (base, id, secret, scope) = match state.scope {
            Scope::Twitch => (
                &self.config.twitch_oauth_base,
                &self.config.twitch_client_id,
                &self.config.twitch_client_secret,
                None,
            ),
            Scope::Public => (
                &self.config.osu_oauth_base,
                &self.config.osu_client_id,
                &self.config.osu_client_secret,
                Some("public"),
            ),
            Scope::Delegate => (
                &self.config.osu_oauth_base,
                &self.config.osu_client_id,
                &self.config.osu_client_secret,
                Some("delegate chat.write"),
            ),
        };
        if id.is_empty() || secret.is_empty() {
            bail!("provider credentials missing");
        }
        let url = format!("{}/token", base.trim_end_matches('/'));
        let mut data =
            json!({"client_id": id, "client_secret": secret, "grant_type": "client_credentials"});
        if let Some(scope) = scope {
            data["scope"] = json!(scope);
        }
        let provider = match state.scope {
            Scope::Twitch => "twitch",
            _ => "osu",
        };
        for attempt in 0..3 {
            let builder = self.client.post(&url);
            let builder = match state.scope {
                Scope::Twitch => builder.form(&data),
                _ => builder.json(&data),
            };
            let started = Instant::now();
            let result = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    record_provider_attempt(provider, "token", "POST", "transport_error", started.elapsed());
                    bail!("token refresh cancelled");
                },
                result = builder.send() => result,
            };
            let elapsed = started.elapsed();
            let response = match result {
                Ok(response) => {
                    record_provider_attempt(
                        provider,
                        "token",
                        "POST",
                        status_outcome(response.status()),
                        elapsed,
                    );
                    response
                }
                Err(error) if attempt < 2 => {
                    record_provider_attempt(provider, "token", "POST", "transport_error", elapsed);
                    tracing::warn!(attempt, error = %error, "token endpoint unavailable");
                    let delay = Duration::from_secs(1 << attempt);
                    record_provider_retry(provider, "token", "transport", Some(delay));
                    self.pause(delay).await?;
                    continue;
                }
                Err(error) => {
                    record_provider_attempt(provider, "token", "POST", "transport_error", elapsed);
                    return Err(error.into());
                }
            };
            let status = response.status();
            if (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()) && attempt < 2
            {
                let delay = retry_after(&response, attempt);
                let reason = if status == StatusCode::TOO_MANY_REQUESTS {
                    "rate_limited"
                } else {
                    "server_error"
                };
                record_provider_retry(provider, "token", reason, Some(delay));
                drop(response);
                self.pause(delay).await?;
                continue;
            }
            if !status.is_success() {
                bail!("token endpoint rejected authentication: HTTP {status}");
            }
            let value = body_json(response).await?;
            let access = value["access_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .context("token endpoint omitted access_token")?
                .to_owned();
            let lifetime = value["expires_in"]
                .as_u64()
                .context("token endpoint omitted expires_in")?
                .min(365 * 86400);
            *guard = Some(Token {
                access: access.clone(),
                expires: Instant::now()
                    + Duration::from_secs(
                        lifetime.saturating_sub((lifetime / 10).clamp(1, 60)).max(1),
                    ),
            });
            return Ok(access);
        }
        bail!("token endpoint retry limit reached")
    }

    async fn invalidate(&self, state: &Tokens, used: &str) {
        let mut guard = state.value.lock().await;
        if guard.as_ref().is_some_and(|t| t.access == used) {
            *guard = None;
        }
    }

    async fn pace_osu(&self) -> Result<()> {
        let mut next = self.osu_next.lock().await;
        let wait = next.saturating_duration_since(Instant::now());
        self.pause(wait).await?;
        *next = Instant::now() + Duration::from_secs(1);
        Ok(())
    }

    #[tracing::instrument(
        name = "provider.http",
        level = "info",
        skip_all,
        fields(
            sentry.op = "http.client",
            provider = match scope { Scope::Twitch => "twitch", _ => "osu" },
            operation = provider_operation(endpoint),
            http.method = %method,
            http.status_code = tracing::field::Empty,
        )
    )]
    async fn request(
        &self,
        scope: Scope,
        method: Method,
        endpoint: &str,
        query: &[(String, String)],
        data: Option<Value>,
        chat: bool,
    ) -> std::result::Result<Value, ChatError> {
        let state = match scope {
            Scope::Twitch => &self.twitch,
            Scope::Public => &self.public,
            Scope::Delegate => &self.delegate,
        };
        let base = match scope {
            Scope::Twitch => &self.config.twitch_api_base,
            _ => &self.config.osu_api_base,
        };
        let provider = match scope {
            Scope::Twitch => "twitch",
            _ => "osu",
        };
        let operation = provider_operation(endpoint);
        let mut refreshed = false;
        let mut retries = 0;
        let mut attempt = 0u32;
        loop {
            let access = self.token(state).await.map_err(known_error)?;
            if !matches!(scope, Scope::Twitch) {
                self.pace_osu().await.map_err(known_error)?;
            }
            let mut builder = self
                .client
                .request(
                    method.clone(),
                    format!("{}/{}", base.trim_end_matches('/'), endpoint),
                )
                .bearer_auth(&access)
                .query(query);
            if matches!(scope, Scope::Twitch) {
                builder = builder.header("Client-Id", &self.config.twitch_client_id);
            }
            if let Some(data) = &data {
                builder = builder.json(data);
            }
            attempt += 1;
            let started = Instant::now();
            let result = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    record_provider_attempt(provider, operation, method.as_str(), "transport_error", started.elapsed());
                    return Err(ChatError { uncertain: chat, status: None, message: "request cancelled during provider send".into() });
                },
                result = builder.send() => result,
            };
            let elapsed = started.elapsed();
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    record_provider_attempt(
                        provider,
                        operation,
                        method.as_str(),
                        "transport_error",
                        elapsed,
                    );
                    tracing::debug!(
                        provider,
                        operation = endpoint,
                        method = %method,
                        attempt,
                        elapsed_ms = elapsed.as_millis() as u64,
                        timeout = error.is_timeout(),
                        connect = error.is_connect(),
                        "provider HTTP attempt failed"
                    );
                    let uncertain = chat && !error.is_connect() && !error.is_builder();
                    if !chat && retries < 2 {
                        retries += 1;
                        let delay = Duration::from_secs(1 << retries);
                        record_provider_retry(provider, operation, "transport", Some(delay));
                        tracing::warn!(
                            provider,
                            operation = endpoint,
                            method = %method,
                            attempt,
                            reason = "transport",
                            retry_delay_ms = delay.as_millis() as u64,
                            "provider HTTP retry scheduled"
                        );
                        self.pause(delay).await.map_err(known_error)?;
                        continue;
                    }
                    return Err(ChatError {
                        uncertain,
                        status: None,
                        message: format!("provider transport failure: {error}"),
                    });
                }
            };
            let status = response.status();
            record_provider_attempt(
                provider,
                operation,
                method.as_str(),
                status_outcome(status),
                elapsed,
            );
            tracing::Span::current().record("http.status_code", status.as_u16());
            tracing::debug!(
                provider,
                operation = endpoint,
                method = %method,
                attempt,
                elapsed_ms = elapsed.as_millis() as u64,
                http_status = status.as_u16(),
                "provider HTTP response headers received"
            );
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                record_provider_retry(provider, operation, "token_refresh", None);
                tracing::debug!(
                    provider,
                    operation = endpoint,
                    method = %method,
                    attempt,
                    http_status = status.as_u16(),
                    reason = "token_refresh",
                    "provider HTTP retry scheduled"
                );
                drop(response);
                self.invalidate(state, &access).await;
                refreshed = true;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS && retries < 2 {
                let delay = retry_after(&response, retries);
                record_provider_retry(provider, operation, "rate_limited", Some(delay));
                tracing::warn!(
                    provider,
                    operation = endpoint,
                    method = %method,
                    attempt,
                    http_status = status.as_u16(),
                    reason = "rate_limited",
                    retry_delay_ms = delay.as_millis() as u64,
                    "provider HTTP retry scheduled"
                );
                drop(response);
                retries += 1;
                self.pause(delay).await.map_err(known_error)?;
                continue;
            }
            if status.is_server_error() && !chat && retries < 2 {
                let delay = retry_after(&response, retries);
                record_provider_retry(provider, operation, "server_error", Some(delay));
                tracing::warn!(
                    provider,
                    operation = endpoint,
                    method = %method,
                    attempt,
                    http_status = status.as_u16(),
                    reason = "server_error",
                    retry_delay_ms = delay.as_millis() as u64,
                    "provider HTTP retry scheduled"
                );
                drop(response);
                retries += 1;
                self.pause(delay).await.map_err(known_error)?;
                continue;
            }
            if !status.is_success() {
                return Err(ChatError {
                    uncertain: chat
                        && (status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT),
                    status: Some(status.as_u16()),
                    message: format!("provider HTTP {status} at {endpoint}"),
                });
            }
            if status == StatusCode::NO_CONTENT {
                return Ok(Value::Null);
            }
            let value = tokio::select! {
                _ = self.shutdown.cancelled() => return Err(ChatError { uncertain: chat, status: None, message: "cancelled reading provider response".into() }),
                value = body_json(response) => value,
            };
            return value.map_err(|error| ChatError {
                uncertain: chat,
                status: None,
                message: format!("invalid provider response: {error:#}"),
            });
        }
    }

    pub async fn get_beatmap(&self, beatmap: &Beatmap) -> Result<(Value, Value)> {
        match beatmap.kind {
            BeatmapKind::Map => {
                let map = self
                    .request(
                        Scope::Public,
                        Method::GET,
                        &format!("beatmaps/{}", beatmap.id),
                        &[],
                        None,
                        false,
                    )
                    .await?;
                let set = map
                    .get("beatmapset")
                    .filter(|s| s.is_object())
                    .context("beatmap omitted beatmapset")?
                    .clone();
                Ok((map, set))
            }
            BeatmapKind::Mapset => {
                let mut set = self
                    .request(
                        Scope::Public,
                        Method::GET,
                        &format!("beatmapsets/{}", beatmap.id),
                        &[],
                        None,
                        false,
                    )
                    .await?;
                let maps = set
                    .as_object_mut()
                    .context("invalid beatmapset object")?
                    .remove("beatmaps")
                    .context("beatmapset omitted beatmaps")?;
                let Value::Array(maps) = maps else {
                    bail!("invalid beatmapset beatmaps");
                };
                let mut map = maps
                    .into_iter()
                    .next()
                    .context("beatmapset has no beatmaps")?;
                map["beatmapset"] = set.clone();
                Ok((map, set))
            }
        }
    }

    pub async fn get_beatmap_attributes(
        &self,
        beatmap_id: i64,
        mods: Option<&str>,
    ) -> Result<Value> {
        Ok(self
            .request(
                Scope::Public,
                Method::POST,
                &format!("beatmaps/{beatmap_id}/attributes"),
                &[],
                Some(json!({"mods":mods})),
                false,
            )
            .await?)
    }

    pub async fn get_user_info(
        &self,
        user_id: &str,
        game_mode: Option<&str>,
        key: Option<&str>,
    ) -> Result<Value> {
        let encoded: String = url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect();
        let mut endpoint = format!("users/{encoded}");
        if let Some(mode) = game_mode {
            if !matches!(mode, "osu" | "taiko" | "fruits" | "mania") {
                bail!("invalid osu game mode");
            }
            endpoint.push('/');
            endpoint.push_str(mode);
        }
        let query = key
            .map(|key| vec![("key".to_owned(), key.to_owned())])
            .unwrap_or_default();
        Ok(self
            .request(Scope::Public, Method::GET, &endpoint, &query, None, false)
            .await?)
    }

    pub async fn send_message(
        &self,
        target_id: i64,
        message: &str,
        is_action: bool,
    ) -> std::result::Result<(), ChatError> {
        self.request(
            Scope::Delegate,
            Method::POST,
            "chat/new",
            &[],
            Some(json!({"target_id":target_id,"message":message,"is_action":is_action})),
            true,
        )
        .await?;
        Ok(())
    }

    /// Conduit management uses the app token, never the managed bot user token.
    pub async fn twitch_request(
        &self,
        method: Method,
        endpoint: &str,
        query: &[(String, String)],
        data: Option<Value>,
    ) -> std::result::Result<Value, ChatError> {
        // Mutating calls are not replayed after an ambiguous transport/5xx failure.
        let mutation = method != Method::GET && method != Method::DELETE;
        self.request(Scope::Twitch, method, endpoint, query, data, mutation)
            .await
    }

    /// Send chat as the managed bot account. This is the one Twitch call that
    /// needs the bot's user token; the response must acknowledge `is_sent`.
    #[tracing::instrument(
        name = "provider.http",
        level = "info",
        skip_all,
        fields(
            sentry.op = "http.client",
            provider = "twitch",
            operation = "chat",
            http.method = "POST",
            http.status_code = tracing::field::Empty,
        )
    )]
    pub async fn send_twitch_message(
        &self,
        auth: &TwitchAuth,
        broadcaster: i64,
        sender: &str,
        message: &str,
    ) -> std::result::Result<(), ChatError> {
        if self.config.dry_run {
            return Ok(());
        }
        let url = format!(
            "{}/chat/messages",
            self.config.twitch_api_base.trim_end_matches('/')
        );
        let body =
            json!({"broadcaster_id":broadcaster.to_string(),"sender_id":sender,"message":message});
        let mut token = auth
            .bot_access_token(&self.shutdown)
            .await
            .map_err(known_error)?;
        let mut refreshed = false;
        let mut retries = 0u32;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let started = Instant::now();
            let result = self
                .client
                .post(&url)
                .bearer_auth(&token)
                .header("Client-Id", &self.config.twitch_client_id)
                .json(&body)
                .send();
            let result = tokio::select! {
                _ = self.shutdown.cancelled() => {
                    record_provider_attempt("twitch", "chat", "POST", "transport_error", started.elapsed());
                    return Err(ChatError { uncertain: false, status: None, message: "chat send cancelled".into() });
                },
                result = result => result,
            };
            let elapsed = started.elapsed();
            let response = match result {
                Ok(response) => response,
                Err(error) => {
                    record_provider_attempt("twitch", "chat", "POST", "transport_error", elapsed);
                    tracing::debug!(
                        provider = "twitch",
                        operation = "chat/messages",
                        method = "POST",
                        attempt,
                        elapsed_ms = elapsed.as_millis() as u64,
                        timeout = error.is_timeout(),
                        connect = error.is_connect(),
                        "provider HTTP attempt failed"
                    );
                    // A transport failure after the request may have reached
                    // Twitch, so it is never replayed automatically.
                    let uncertain = !error.is_connect() && !error.is_builder();
                    return Err(ChatError {
                        uncertain,
                        status: None,
                        message: format!("chat transport failure: {error}"),
                    });
                }
            };
            let status = response.status();
            record_provider_attempt("twitch", "chat", "POST", status_outcome(status), elapsed);
            tracing::Span::current().record("http.status_code", status.as_u16());
            tracing::debug!(
                provider = "twitch",
                operation = "chat/messages",
                method = "POST",
                attempt,
                elapsed_ms = elapsed.as_millis() as u64,
                http_status = status.as_u16(),
                "provider HTTP response headers received"
            );
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                record_provider_retry("twitch", "chat", "token_refresh", None);
                tracing::debug!(
                    provider = "twitch",
                    operation = "chat/messages",
                    method = "POST",
                    attempt,
                    http_status = status.as_u16(),
                    reason = "token_refresh",
                    "provider HTTP retry scheduled"
                );
                drop(response);
                token = auth
                    .bot_access_token_after_rejection(&token, &self.shutdown)
                    .await
                    .map_err(known_error)?;
                refreshed = true;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS && retries < 2 {
                let delay = retry_after(&response, retries);
                record_provider_retry("twitch", "chat", "rate_limited", Some(delay));
                tracing::warn!(
                    provider = "twitch",
                    operation = "chat/messages",
                    method = "POST",
                    attempt,
                    http_status = status.as_u16(),
                    reason = "rate_limited",
                    retry_delay_ms = delay.as_millis() as u64,
                    "provider HTTP retry scheduled"
                );
                drop(response);
                retries += 1;
                self.pause(delay).await.map_err(known_error)?;
                continue;
            }
            if !status.is_success() {
                return Err(ChatError {
                    uncertain: status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT,
                    status: Some(status.as_u16()),
                    message: format!("Twitch chat message failed: HTTP {status}"),
                });
            }
            let value = body_json(response).await.map_err(|error| ChatError {
                uncertain: true,
                status: None,
                message: format!("invalid chat response: {error:#}"),
            })?;
            let result = value["data"].as_array().and_then(|data| data.first());
            return match result.and_then(|row| row["is_sent"].as_bool()) {
                Some(true) => Ok(()),
                Some(false) => Err(ChatError {
                    uncertain: false,
                    status: Some(status.as_u16()),
                    message: format!(
                        "Twitch dropped the chat message: {}: {}",
                        result
                            .and_then(|row| row["drop_reason"]["code"].as_str())
                            .unwrap_or("unspecified"),
                        result
                            .and_then(|row| row["drop_reason"]["message"].as_str())
                            .unwrap_or("no rejection message provided")
                    ),
                }),
                None => Err(ChatError {
                    uncertain: true,
                    status: None,
                    message:
                        "Twitch chat response omitted the delivery acknowledgement; not replayed"
                            .into(),
                }),
            };
        }
    }

    /// No category filter: offline and a category change are distinct states.
    pub async fn live_streams(&self, ids: &[i64]) -> Result<Vec<LiveStream>> {
        let mut batches = ids.chunks(100);
        let mut result = Vec::new();
        while let Some(first) = batches.next() {
            let (a, b, c, d) = tokio::try_join!(
                self.live_batch(first),
                self.live_batch(batches.next().unwrap_or(&[])),
                self.live_batch(batches.next().unwrap_or(&[])),
                self.live_batch(batches.next().unwrap_or(&[])),
            )?;
            result.extend(a);
            result.extend(b);
            result.extend(c);
            result.extend(d);
        }
        Ok(result)
    }

    async fn live_batch(&self, batch: &[i64]) -> Result<Vec<LiveStream>> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = vec![("first".into(), "100".into())];
        query.extend(batch.iter().map(|id| ("user_id".into(), id.to_string())));
        let mut result = Vec::new();
        let mut seen_cursors = std::collections::HashSet::new();
        loop {
            let value = self
                .request(Scope::Twitch, Method::GET, "streams", &query, None, false)
                .await?;
            for row in value["data"]
                .as_array()
                .context("Helix omitted stream data")?
            {
                let id: i64 = row["user_id"]
                    .as_str()
                    .context("Helix omitted user_id")?
                    .parse()?;
                let login = row["user_login"]
                    .as_str()
                    .context("Helix omitted user_login")?;
                if !batch.contains(&id) || !valid_login(login) {
                    bail!("Helix returned unexpected user identity");
                }
                let category_id = row["game_id"].as_str().context("Helix omitted game_id")?;
                result.push(LiveStream {
                    id,
                    login: login.to_ascii_lowercase(),
                    category_id: category_id.to_owned(),
                });
            }
            let Some(cursor) = value["pagination"]["cursor"]
                .as_str()
                .filter(|s| !s.is_empty())
            else {
                break;
            };
            if seen_cursors.len() >= 10 || !seen_cursors.insert(cursor.to_owned()) {
                bail!("Helix pagination did not terminate");
            }
            query.retain(|(name, _)| name != "after");
            query.push(("after".into(), cursor.into()));
        }
        Ok(result)
    }
}

fn known_error(error: anyhow::Error) -> ChatError {
    ChatError {
        uncertain: false,
        status: None,
        message: format!("{error:#}"),
    }
}
fn retry_after(response: &Response, attempt: u32) -> Duration {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<u64>().ok())
        .or_else(|| {
            response
                .headers()
                .get("Ratelimit-Reset")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.parse::<i64>().ok())
                .map(|epoch| epoch.saturating_sub(chrono::Utc::now().timestamp()).max(1) as u64)
        })
        .unwrap_or(1u64 << attempt.min(5));
    Duration::from_secs(seconds.clamp(1, 60))
}

pub(crate) fn valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= 25
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_rejects_invalid_identity_characters() {
        assert!(valid_login("some_user123"));
        for bad in ["", "user\r\nother", "user name", "#user", "üser"] {
            assert!(!valid_login(bad));
        }
    }
    #[derive(Default)]
    struct MockState {
        scopes: std::sync::Mutex<Vec<String>>,
        map_calls: std::sync::atomic::AtomicUsize,
        chat_calls: std::sync::atomic::AtomicUsize,
    }
    async fn mock_token(
        axum::extract::State(state): axum::extract::State<Arc<MockState>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        assert_eq!(body["grant_type"], "client_credentials");
        let mut scopes = state.scopes.lock().unwrap();
        scopes.push(body["scope"].as_str().unwrap().to_owned());
        axum::Json(json!({"access_token":format!("token-{}",scopes.len()),"expires_in":3600}))
    }
    async fn mock_map(
        axum::extract::State(state): axum::extract::State<Arc<MockState>>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        if state
            .map_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"error":"expired"})),
            );
        }
        (
            axum::http::StatusCode::OK,
            axum::Json(
                json!({"id":42,"beatmapset_id":7,"beatmapset":{"id":7,"artist":"Artist","title":"Title"}}),
            ),
        )
    }
    async fn mock_chat(
        axum::extract::State(state): axum::extract::State<Arc<MockState>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::http::StatusCode {
        assert_eq!(body["target_id"], 7);
        assert_eq!(body["message"], "hello");
        assert_eq!(body["is_action"], false);
        state
            .chat_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    }
    #[tokio::test]
    async fn expired_public_token_refreshes_without_deadlock_and_ambiguous_chat_never_retries() {
        use axum::{
            Router,
            routing::{get, post},
        };
        let state = Arc::new(MockState::default());
        let router = Router::new()
            .route("/oauth/token", post(mock_token))
            .route("/api/v2/beatmaps/42", get(mock_map))
            .route("/api/v2/chat/new", post(mock_chat))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut config = Config::from_env().unwrap();
        config.osu_client_id = "test".into();
        config.osu_client_secret = "test".into();
        config.osu_api_base = format!("http://{address}/api/v2");
        config.osu_oauth_base = format!("http://{address}/oauth");
        let providers = Providers::new(Arc::new(config), CancellationToken::new()).unwrap();
        let (map, _) = tokio::time::timeout(
            Duration::from_secs(5),
            providers.get_beatmap(&Beatmap {
                id: 42,
                kind: BeatmapKind::Map,
                mods: String::new(),
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(map["id"], 42);
        let error = providers.send_message(7, "hello", false).await.unwrap_err();
        assert!(error.uncertain);
        assert_eq!(
            state.chat_calls.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            *state.scopes.lock().unwrap(),
            vec!["public", "public", "delegate chat.write"]
        );
        server.abort();
    }
}
