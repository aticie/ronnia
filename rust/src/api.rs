//! Website API, opaque browser sessions, and provider authorization-code flows.
mod docs;
mod oauth;
mod security;
#[cfg(test)]
mod tests;

use crate::{
    config::Config,
    domain::{RequestEvent, Settings},
    storage::{ClaimOutcome, DeliveryOutcome, Store},
};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tower_http::cors::CorsLayer;

#[derive(Clone)]
struct ApiState {
    config: Arc<Config>,
    store: Store,
    http: reqwest::Client,
}

pub fn router(config: Arc<Config>, store: Store) -> Router {
    let origin = url::Url::parse(&config.public_origin)
        .expect("validated public origin")
        .origin()
        .ascii_serialization();
    let cors = CorsLayer::new()
        .allow_origin(
            origin
                .parse::<HeaderValue>()
                .expect("validated public origin"),
        )
        .allow_credentials(true)
        .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
        .allow_headers([header::CONTENT_TYPE]);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.http_timeout_secs))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client initialization");
    let app = Router::new()
        .route("/oauth2/osu-login", get(oauth::osu_login))
        .route("/oauth2/osu-redirect", get(oauth::osu_redirect))
        .route("/oauth2/twitch-login", get(oauth::twitch_login))
        .route("/oauth2/twitch-redirect", get(oauth::twitch_redirect))
        .route("/user/me", get(me).delete(delete_me))
        .route("/user/logout", get(logout))
        .route("/user/settings", get(settings).post(update_settings))
        .route(
            "/user/exclude",
            get(excluded).post(add_excluded).delete(remove_excluded),
        )
        .route("/live/users", get(live))
        .route("/requests/beatmaps/top/daily", get(daily))
        .route("/requests/beatmaps/top/weekly", get(weekly))
        .route("/requests/beatmaps/top/monthly", get(monthly))
        .route("/migration/status", get(migration_status))
        .route("/internal/legacy/channels", get(legacy_channels))
        .route(
            "/internal/legacy/channels/by-login/{login}",
            get(legacy_channel_by_login),
        )
        .route("/internal/legacy/channels/{twitch_id}", get(legacy_channel))
        .route("/internal/legacy/live", post(report_legacy_live))
        .route("/internal/legacy/claim", post(claim_legacy))
        .route(
            "/internal/legacy/deliveries/{twitch_message_id}",
            post(report_legacy_delivery),
        )
        .route("/health", get(health))
        .route("/ready", get(ready));
    let app = if config.debug_mode {
        app.merge(docs::router())
    } else {
        app
    };
    app.layer(cors).with_state(ApiState {
        config,
        store,
        http,
    })
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str);
type ApiResult<T> = Result<T, ApiError>;
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"detail":self.1}))).into_response()
    }
}
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        if let Some(fault) = error.downcast_ref::<oauth::AccountFault>() {
            return Self(StatusCode::CONFLICT, fault.message());
        }
        tracing::error!(error = %error, "API database operation failed");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Database operation failed",
        )
    }
}
fn unauthorized() -> ApiError {
    ApiError(StatusCode::UNAUTHORIZED, "Authentication required")
}

#[utoipa::path(
    get, path = "/user/me", tag = "user",
    summary = "Get the signed-in user or pending signup",
    description = "Returns the public user profile without settings, a signup provider marker, or JSON null when no login is present. twitchBotAuthorized reports verified channel:bot consent; imported accounts must authorize through Twitch login before the bot can become active. An expired, deleted-account or legacy JWT session is rejected.",
    security((), ("session" = []), ("signup" = [])),
    responses(
        (status = 200, description = "User, signup marker, or null", body = docs::MeResponse),
        (status = 401, description = "Invalid or expired session", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn me(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Json<docs::MeResponse>> {
    if security::cookie(&headers, "token").is_some() {
        let user = security::authenticate(&state, &headers).await?;
        let twitch_id = user.twitch_id;
        let authorized = state.store.call(move |db| {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM twitch_channel_grants WHERE twitch_id=?1 AND status='granted')",
                [twitch_id], |row| row.get::<_, bool>(0),
            )?)
        }).await?;
        return Ok(Json(docs::MeResponse::User(docs::UserResponse::new(
            user, authorized,
        ))));
    }
    if let Some(pending) = security::pending(&state, &headers).await? {
        return Ok(Json(docs::MeResponse::Signup(docs::SignupStatus {
            signup: pending.provider,
        })));
    }
    Ok(Json(docs::MeResponse::Anonymous(docs::NullResponse)))
}
#[utoipa::path(
    get, path = "/user/logout", tag = "user",
    summary = "Revoke browser login and signup state",
    description = "Requires a trusted Origin or Referer even though this legacy mutation uses GET. Revokes server-side browser sessions and clears authentication cookies. Redirects only to a trusted Referer or the configured frontend origin.",
    params(docs::BrowserOrigin),
    responses(
        (status = 303, description = "Browser state revoked", headers(("Location" = String, description = "Safe frontend/API return URL"), ("Set-Cookie" = String, description = "Expired authentication cookies"))),
        (status = 403, description = "Untrusted or missing browser origin", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn logout(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Response> {
    security::require_origin(&state.config, &headers)?;
    let session = security::cookie(&headers, "token").map(security::hash);
    let signup = security::cookie(&headers, "signup_details").map(security::hash);
    let osu_state = security::cookie(&headers, "oauth_state_osu").map(security::hash);
    let twitch_state = security::cookie(&headers, "oauth_state_twitch").map(security::hash);
    state
        .store
        .call(move |db| {
            let tx = db.transaction()?;
            if let Some(hash) = session {
                tx.execute("DELETE FROM sessions WHERE token_hash=?1", [hash])?;
            }
            if let Some(hash) = signup {
                tx.execute("DELETE FROM signup_attempts WHERE token_hash=?1", [hash])?;
            }
            for hash in [osu_state, twitch_state].into_iter().flatten() {
                tx.execute("DELETE FROM oauth_attempts WHERE state_hash=?1", [hash])?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    let mut response = security::redirect(&security::return_to(&state.config, &headers));
    security::clear_auth_cookies(&mut response, &state.config);
    for name in ["oauth_state_osu", "oauth_state_twitch"] {
        security::set_cookie(&mut response, &state.config, name, "", 0);
    }
    Ok(response)
}
#[utoipa::path(
    delete, path = "/user/me", tag = "user",
    summary = "Delete the signed-in account",
    params(docs::BrowserOrigin), security(("session" = [])),
    responses(
        (status = 303, description = "Account deleted and sessions revoked", headers(("Location" = String, description = "Safe frontend/API return URL"), ("Set-Cookie" = String, description = "Expired authentication cookies"))),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 403, description = "Untrusted or missing browser origin", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn delete_me(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Response> {
    security::require_origin(&state.config, &headers)?;
    let user = security::authenticate(&state, &headers).await?;
    state.store.delete_user(user.twitch_id).await?;
    let mut response = security::redirect(&security::return_to(&state.config, &headers));
    security::clear_auth_cookies(&mut response, &state.config);
    Ok(response)
}
#[utoipa::path(
    get, path = "/user/settings", tag = "user",
    summary = "Get setting metadata and effective values",
    description = "Returns the initialized seven settings (enable, echo, sub-only, points-only, test, cooldown, sr), with imported metadata customizations preserved and user values overlaid.",
    security(("session" = [])),
    responses(
        (status = 200, description = "Setting definitions with effective values", body = [docs::SettingMetadata]),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn settings(State(state): State<ApiState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let user = security::authenticate(&state, &headers).await?;
    let effective = serde_json::to_value(user.settings).map_err(anyhow::Error::from)?;
    let mut metadata = state.store.settings_metadata().await?;
    for setting in &mut metadata {
        if let Some(value) = setting
            .get("name")
            .and_then(Value::as_str)
            .and_then(|name| effective.get(name))
            .cloned()
        {
            setting["value"] = value;
        }
    }
    Ok(Json(Value::Array(metadata)))
}
#[utoipa::path(
    post, path = "/user/settings", tag = "user",
    summary = "Replace the known user settings",
    description = "Omitted known settings take their documented defaults. Cooldown must be finite and nonnegative. Star bounds require lower>=0 and upper=-1 or upper>=lower; upper>=10 is normalized to -1 (unlimited). Unknown imported customization keys are preserved.",
    params(docs::BrowserOrigin), security(("session" = [])),
    request_body = Settings,
    responses(
        (status = 200, description = "Settings saved; JSON null", body = docs::NullResponse, content_type = "application/json"),
        (status = 400, description = "Malformed JSON request"),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 403, description = "Untrusted or missing browser origin", body = docs::ApiErrorBody),
        (status = 415, description = "JSON content type required"),
        (status = 422, description = "Invalid settings or field types"),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn update_settings(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(mut settings): Json<Settings>,
) -> ApiResult<Json<Value>> {
    security::require_origin(&state.config, &headers)?;
    let user = security::authenticate(&state, &headers).await?;
    settings
        .validate()
        .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid settings"))?;
    state.store.set_settings(user.twitch_id, settings).await?;
    Ok(Json(Value::Null))
}
#[utoipa::path(
    get, path = "/user/exclude", tag = "user",
    summary = "Get excluded Twitch logins", security(("session" = [])),
    responses(
        (status = 200, description = "Normalized lowercase exclusion list", body = [String]),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn excluded(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<String>>> {
    Ok(Json(
        security::authenticate(&state, &headers)
            .await?
            .excluded_users,
    ))
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct Exclusion {
    /// Twitch login to normalize to lowercase; this is a query parameter, not a JSON body.
    #[param(min_length = 1, max_length = 25, pattern = "^[A-Za-z0-9_]+$")]
    excluded_user: String,
}
#[utoipa::path(
    post, path = "/user/exclude", tag = "user",
    summary = "Add an excluded Twitch login",
    params(Exclusion, docs::BrowserOrigin), security(("session" = [])),
    responses(
        (status = 200, description = "Normalized excluded login", body = String, content_type = "application/json"),
        (status = 400, description = "Missing or malformed query parameter"),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 403, description = "Untrusted or missing browser origin", body = docs::ApiErrorBody),
        (status = 422, description = "Invalid Twitch login", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn add_excluded(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<Exclusion>,
) -> ApiResult<Json<String>> {
    change_excluded(state, headers, query, true).await
}
#[utoipa::path(
    delete, path = "/user/exclude", tag = "user",
    summary = "Remove an excluded Twitch login",
    params(Exclusion, docs::BrowserOrigin), security(("session" = [])),
    responses(
        (status = 200, description = "Normalized excluded login", body = String, content_type = "application/json"),
        (status = 400, description = "Missing or malformed query parameter"),
        (status = 401, description = "Authentication required", body = docs::ApiErrorBody),
        (status = 403, description = "Untrusted or missing browser origin", body = docs::ApiErrorBody),
        (status = 422, description = "Invalid Twitch login", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn remove_excluded(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<Exclusion>,
) -> ApiResult<Json<String>> {
    change_excluded(state, headers, query, false).await
}
async fn change_excluded(
    state: ApiState,
    headers: HeaderMap,
    query: Exclusion,
    excluded: bool,
) -> ApiResult<Json<String>> {
    security::require_origin(&state.config, &headers)?;
    let user = security::authenticate(&state, &headers).await?;
    let login = query.excluded_user.to_ascii_lowercase();
    if login.is_empty()
        || login.len() > 25
        || !login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Invalid Twitch login",
        ));
    }
    state
        .store
        .set_excluded(user.twitch_id, login.clone(), excluded)
        .await?;
    Ok(Json(login))
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct Pagination {
    #[serde(default = "default_limit")]
    #[param(minimum = 1, maximum = 100, default = 5)]
    limit: i64,
    #[serde(default)]
    #[param(minimum = 0, default = 0)]
    offset: i64,
}
fn default_limit() -> i64 {
    5
}
impl Pagination {
    fn validate(&self) -> ApiResult<()> {
        if !(1..=100).contains(&self.limit) || self.offset < 0 {
            Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "limit must be 1..100 and offset nonnegative",
            ))
        } else {
            Ok(())
        }
    }
}
#[utoipa::path(
    get, path = "/live/users", tag = "live",
    summary = "Get currently streaming Twitch logins", params(Pagination),
    responses(
        (status = 200, description = "Live Twitch logins", body = [String]),
        (status = 400, description = "Malformed pagination"),
        (status = 422, description = "Pagination outside allowed bounds", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn live(
    State(state): State<ApiState>,
    Query(page): Query<Pagination>,
) -> ApiResult<Json<Vec<String>>> {
    page.validate()?;
    Ok(Json(state.store.live_users(page.limit, page.offset).await?))
}
async fn top(state: ApiState, page: Pagination, days: i64) -> ApiResult<Json<Vec<Value>>> {
    page.validate()?;
    Ok(Json(
        state
            .store
            .leaderboard(
                chrono::Utc::now().timestamp() - days * 86400,
                page.limit,
                page.offset,
            )
            .await?,
    ))
}
#[utoipa::path(
    get, path = "/requests/beatmaps/top/daily", tag = "requests",
    summary = "Get top requested beatmaps over the last 24 hours", params(Pagination),
    responses(
        (status = 200, description = "Beatmap metadata with integer _id and request count", body = [docs::LeaderboardEntry]),
        (status = 400, description = "Malformed pagination"),
        (status = 422, description = "Pagination outside allowed bounds", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn daily(
    State(state): State<ApiState>,
    Query(page): Query<Pagination>,
) -> ApiResult<Json<Vec<Value>>> {
    top(state, page, 1).await
}
#[utoipa::path(
    get, path = "/requests/beatmaps/top/weekly", tag = "requests",
    summary = "Get top requested beatmaps over the last seven UTC days", params(Pagination),
    responses(
        (status = 200, description = "Beatmap metadata with integer _id and request count", body = [docs::LeaderboardEntry]),
        (status = 400, description = "Malformed pagination"),
        (status = 422, description = "Pagination outside allowed bounds", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn weekly(
    State(state): State<ApiState>,
    Query(page): Query<Pagination>,
) -> ApiResult<Json<Vec<Value>>> {
    top(state, page, 7).await
}
#[utoipa::path(
    get, path = "/requests/beatmaps/top/monthly", tag = "requests",
    summary = "Get top requested beatmaps over the last thirty UTC days", params(Pagination),
    responses(
        (status = 200, description = "Beatmap metadata with integer _id and request count", body = [docs::LeaderboardEntry]),
        (status = 400, description = "Malformed pagination"),
        (status = 422, description = "Pagination outside allowed bounds", body = docs::ApiErrorBody),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn monthly(
    State(state): State<ApiState>,
    Query(page): Query<Pagination>,
) -> ApiResult<Json<Vec<Value>>> {
    top(state, page, 30).await
}
#[utoipa::path(
    get, path = "/migration/status", tag = "migration",
    summary = "Get the public migration banner state",
    responses((status = 200, description = "Migration flag and optional cutoff", body = docs::MigrationStatus))
)]
async fn migration_status(State(state): State<ApiState>) -> Json<docs::MigrationStatus> {
    Json(docs::MigrationStatus {
        active: state.config.migration_active,
        cutoff_at: state.config.migration_cutoff_at.clone(),
    })
}

#[derive(Deserialize)]
struct LegacyChannelsQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_legacy_limit")]
    limit: i64,
}
fn default_legacy_limit() -> i64 {
    100
}

async fn legacy_channels(
    State(state): State<ApiState>,
    headers: HeaderMap,
    query: Result<Query<LegacyChannelsQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<crate::domain::User>>> {
    security::require_internal(&state.config, &headers)?;
    let Query(query) =
        query.map_err(|_| ApiError(StatusCode::BAD_REQUEST, "Invalid pagination"))?;
    if !(1..=100).contains(&query.limit) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "limit must be between 1 and 100",
        ));
    }
    Ok(Json(
        state
            .store
            .legacy_channels(query.after, query.limit)
            .await?,
    ))
}

async fn legacy_channel_by_login(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(login): Path<String>,
) -> ApiResult<Json<crate::domain::User>> {
    security::require_internal(&state.config, &headers)?;
    state
        .store
        .legacy_user_by_login(&login)
        .await?
        .map(Json)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "Channel not found"))
}

async fn legacy_channel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(twitch_id): Path<i64>,
) -> ApiResult<Json<crate::domain::User>> {
    security::require_internal(&state.config, &headers)?;
    state
        .store
        .legacy_user_by_twitch(twitch_id)
        .await?
        .map(Json)
        .ok_or(ApiError(StatusCode::NOT_FOUND, "Channel not found"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyLiveChannel {
    twitch_id: i64,
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyLiveReport {
    channels: Vec<LegacyLiveChannel>,
}

async fn report_legacy_live(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<LegacyLiveReport>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    security::require_internal(&state.config, &headers)?;
    let Json(report) =
        body.map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid request body"))?;
    if report.channels.len() > 10_000 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Too many live channels",
        ));
    }
    let mut seen = std::collections::HashSet::with_capacity(report.channels.len());
    for channel in &report.channels {
        if channel.twitch_id <= 0
            || channel.login.trim().is_empty()
            || !seen.insert(channel.twitch_id)
        {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid live channels",
            ));
        }
    }
    let updated = state
        .store
        .replace_legacy_live(
            report
                .channels
                .into_iter()
                .map(|channel| (channel.twitch_id, channel.login))
                .collect(),
        )
        .await?;
    Ok(Json(json!({"updated": updated})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyClaim {
    twitch_message_id: String,
    twitch_id: i64,
    target_osu_id: i64,
    message: String,
}

async fn claim_legacy(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Json<LegacyClaim>, JsonRejection>,
) -> ApiResult<Response> {
    security::require_internal(&state.config, &headers)?;
    let Json(claim) =
        body.map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid request body"))?;
    if claim.twitch_message_id.trim().is_empty()
        || claim.message.trim().is_empty()
        || claim.twitch_id <= 0
        || claim.target_osu_id <= 0
    {
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid claim"));
    }
    let outcome = state
        .store
        .claim_legacy_delivery(
            claim.twitch_message_id,
            claim.twitch_id,
            claim.target_osu_id,
            claim.message,
        )
        .await?;
    Ok(match outcome {
        ClaimOutcome::Claimed => (StatusCode::OK, Json(json!({"claimed": true}))).into_response(),
        ClaimOutcome::Rejected(reason) => (
            StatusCode::CONFLICT,
            Json(json!({"claimed": false, "reason": reason})),
        )
            .into_response(),
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStatistics {
    requester_channel_name: String,
    requested_beatmap_id: i64,
    requested_channel_name: String,
    #[serde(default)]
    mods: Option<String>,
    timestamp: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyDeliveryReport {
    sent: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    statistics: Option<LegacyStatistics>,
}

async fn report_legacy_delivery(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(twitch_message_id): Path<String>,
    body: Result<Json<LegacyDeliveryReport>, JsonRejection>,
) -> ApiResult<Json<Value>> {
    security::require_internal(&state.config, &headers)?;
    let Json(report) =
        body.map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid request body"))?;
    let statistics = if report.sent {
        let stats = report.statistics.ok_or(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Statistics are required for a sent delivery",
        ))?;
        if stats.timestamp < 0 {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid statistics",
            ));
        }
        let event = RequestEvent {
            requester_channel_name: stats.requester_channel_name,
            requested_beatmap_id: stats.requested_beatmap_id,
            requested_channel_name: stats.requested_channel_name,
            mods: stats.mods,
            timestamp: stats.timestamp,
        };
        event
            .validate()
            .map_err(|_| ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Invalid statistics"))?;
        Some(event)
    } else {
        None
    };
    match state
        .store
        .complete_legacy_delivery(twitch_message_id, report.sent, report.error, statistics)
        .await?
    {
        DeliveryOutcome::Recorded => Ok(Json(json!({"ok": true}))),
        DeliveryOutcome::NotFound => {
            Err(ApiError(StatusCode::NOT_FOUND, "Legacy delivery not found"))
        }
    }
}

#[utoipa::path(
    get, path = "/health", tag = "operations", summary = "Check process liveness",
    responses((status = 200, description = "Process is serving HTTP", body = docs::HealthStatus))
)]
async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

#[utoipa::path(
    get, path = "/ready", tag = "operations", summary = "Check initialized database readiness",
    responses(
        (status = 200, description = "Database actor accepts queries", body = docs::HealthStatus),
        (status = 500, description = "Database unavailable", body = docs::ApiErrorBody)
    )
)]
async fn ready(State(state): State<ApiState>) -> ApiResult<Json<Value>> {
    state
        .store
        .call(|db| {
            db.query_row("SELECT 1", [], |_| Ok(()))?;
            Ok(())
        })
        .await?;
    Ok(Json(json!({"status":"ready"})))
}
