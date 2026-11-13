//! Provider authorization-code flows: one flow shape, with per-provider endpoints,
//! scopes and claim mapping owned by `Provider`.

use super::{
    ApiError, ApiResult, ApiState,
    security::{self, Identity},
    unauthorized,
};
use crate::domain::Settings;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

/// Every provider-specific fact about the authorization-code exchange lives
/// here; the flow itself (`start`, `callback`, `complete_account`) is shared.
#[derive(Clone, Copy, PartialEq)]
enum Provider {
    Osu,
    Twitch,
}
impl Provider {
    fn name(self) -> &'static str {
        match self {
            Self::Osu => "osu",
            Self::Twitch => "twitch",
        }
    }
    fn cookie(self) -> &'static str {
        match self {
            Self::Osu => "oauth_state_osu",
            Self::Twitch => "oauth_state_twitch",
        }
    }
    fn credentials(self, state: &ApiState) -> (&str, &str, &str, &str) {
        match self {
            Self::Osu => (
                &state.config.osu_client_id,
                &state.config.osu_client_secret,
                &state.config.osu_redirect_uri,
                &state.config.osu_oauth_base,
            ),
            Self::Twitch => (
                &state.config.twitch_client_id,
                &state.config.twitch_client_secret,
                &state.config.twitch_redirect_uri,
                &state.config.twitch_oauth_base,
            ),
        }
    }
    fn scope(self) -> &'static str {
        match self {
            Self::Osu => "identify",
            Self::Twitch => "channel:bot",
        }
    }
    /// Twitch needs explicit consent for the chat bot; osu! has no such flag.
    fn extra_authorize(self) -> Option<(&'static str, &'static str)> {
        (self == Self::Twitch).then_some(("force_verify", "true"))
    }
    fn api_base(self, state: &ApiState) -> &str {
        match self {
            Self::Osu => &state.config.osu_api_base,
            Self::Twitch => &state.config.twitch_api_base,
        }
    }
    fn profile_path(self) -> &'static str {
        match self {
            Self::Osu => "me",
            Self::Twitch => "users",
        }
    }
    /// Helix wraps the account in a `data` array; osu! returns it directly.
    fn profile(self, value: &Value) -> Option<&Value> {
        match self {
            Self::Osu => Some(value),
            Self::Twitch => value
                .get("data")
                .and_then(Value::as_array)
                .and_then(|data| data.first()),
        }
    }
    fn name_key(self) -> &'static str {
        match self {
            Self::Osu => "username",
            Self::Twitch => "login",
        }
    }
    fn avatar_key(self) -> &'static str {
        match self {
            Self::Osu => "avatar_url",
            Self::Twitch => "profile_image_url",
        }
    }
}
fn not_configured() -> ApiError {
    ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "OAuth provider is not configured",
    )
}
#[utoipa::path(get, path = "/oauth2/osu-login", tag = "oauth2", summary = "Start osu! login or account signup", description = "Creates a short-lived browser-bound OAuth nonce and redirects to osu! with identify scope. Existing valid session/signup cookies are captured to prevent changing account context during authorization. Invalid legacy or expired cookies are cleared.", params(("Referer" = Option<String>, Header, description = "Trusted frontend/API return URL; untrusted or absent values use the configured frontend origin")), responses((status = 303, description = "Redirect to osu! authorization", headers(("Location" = String, description = "Provider authorize URL with opaque state"), ("Set-Cookie" = String, description = "HttpOnly oauth_state_osu cookie"))), (status = 500, description = "Database unavailable", body = super::docs::ApiErrorBody), (status = 503, description = "OAuth provider not configured", body = super::docs::ApiErrorBody)) )]
pub(super) async fn osu_login(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    start(state, headers, Provider::Osu).await
}
#[utoipa::path(get, path = "/oauth2/twitch-login", tag = "oauth2", summary = "Start Twitch login or account signup", description = "Creates a short-lived browser-bound OAuth nonce and redirects to Twitch with channel:bot scope and force_verify=true for explicit chat-bot consent. Existing valid session/signup cookies are captured to prevent changing account context during authorization. Signed-in users may reauthorize only the same Twitch identity. Invalid legacy or expired cookies are cleared.", params(("Referer" = Option<String>, Header, description = "Trusted frontend/API return URL; untrusted or absent values use the configured frontend origin")), responses((status = 303, description = "Redirect to Twitch authorization", headers(("Location" = String, description = "Provider authorize URL with opaque state"), ("Set-Cookie" = String, description = "HttpOnly oauth_state_twitch cookie"))), (status = 500, description = "Database unavailable", body = super::docs::ApiErrorBody), (status = 503, description = "OAuth provider not configured", body = super::docs::ApiErrorBody)) )]
pub(super) async fn twitch_login(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    start(state, headers, Provider::Twitch).await
}

async fn start(state: ApiState, headers: HeaderMap, provider: Provider) -> ApiResult<Response> {
    let (client_id, secret, callback, base) = provider.credentials(&state);
    if client_id.is_empty() || secret.is_empty() {
        return Err(not_configured());
    }
    let mut target =
        Url::parse(&format!("{}/authorize", base.trim_end_matches('/'))).map_err(|_| {
            ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "Invalid OAuth provider URL",
            )
        })?;
    let nonce = security::random_token();
    target.query_pairs_mut().extend_pairs([
        ("client_id", client_id),
        ("redirect_uri", callback),
        ("response_type", "code"),
        ("scope", provider.scope()),
        ("state", nonce.as_str()),
    ]);
    if let Some((key, value)) = provider.extra_authorize() {
        target.query_pairs_mut().append_pair(key, value);
    }
    // An old JWT or expired opaque cookie is never accepted as a new session.
    let valid_session = security::session_valid(&state, &headers).await?;
    let valid_signup = security::pending(&state, &headers).await?.is_some();
    let session_hash = valid_session
        .then(|| security::hash(security::cookie(&headers, "token").expect("session cookie")));
    let signup_hash = valid_signup.then(|| {
        security::hash(security::cookie(&headers, "signup_details").expect("signup cookie"))
    });
    let return_to = security::return_to(&state.config, &headers);
    let expires_at = chrono::Utc::now().timestamp() + state.config.oauth_ttl_secs;
    let state_hash = security::hash(&nonce);
    state.store.call(move |db| {
        let tx = db.transaction()?;
        let now = chrono::Utc::now().timestamp();
        tx.execute("DELETE FROM oauth_attempts WHERE expires_at<=?1", [now])?;
        tx.execute("DELETE FROM signup_attempts WHERE expires_at<=?1", [now])?;
        tx.execute("DELETE FROM sessions WHERE expires_at<=?1", [now])?;
        tx.execute("INSERT INTO oauth_attempts(state_hash,provider,return_to,expires_at,session_hash,signup_hash) VALUES(?1,?2,?3,?4,?5,?6)", params![state_hash, provider.name(), return_to, expires_at, session_hash, signup_hash])?;
        tx.commit()?; Ok(())
    }).await?;
    let mut response = security::redirect(target.as_str());
    security::set_cookie(
        &mut response,
        &state.config,
        provider.cookie(),
        &nonce,
        state.config.oauth_ttl_secs,
    );
    if !valid_session {
        security::set_cookie(&mut response, &state.config, "token", "", 0);
    }
    if !valid_signup {
        security::set_cookies(
            &mut response,
            &state.config,
            &[("signup", "", 0), ("signup_details", "", 0)],
        );
    }
    Ok(response)
}
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct Callback {
    /// Authorization code supplied on success. Either code or provider error must be present.
    code: Option<String>,
    /// Browser-bound nonce from the authorization URL; expires and may be consumed only once.
    #[param(value_type = String, required = true, min_length = 64, max_length = 64)]
    state: Option<String>,
    /// Provider denial/error; authorization does not complete when supplied.
    error: Option<String>,
}
#[utoipa::path(get, path = "/oauth2/osu-redirect", tag = "oauth2", summary = "Complete osu! authorization", description = "Exchanges the code with osu!, then returns to the stored trusted URL. A registered identity logs in immediately; an unknown first identity creates an opaque signup_details cookie and signup=osu marker. An opposite-provider pending signup is linked atomically without replacing existing ownership or resetting settings. The nonce and browser auth context must match the initiating request.", params(Callback), security(("osuState" = [])), responses((status = 303, description = "Login or partial signup completed", headers(("Location" = String, description = "Stored trusted return URL"), ("Set-Cookie" = String, description = "Opaque token session or signup_details and signup cookies"))), (status = 400, description = "Missing, expired, replayed or mismatched state; missing code or provider denial", body = super::docs::ApiErrorBody), (status = 401, description = "Initiating session/signup expired or account deleted", body = super::docs::ApiErrorBody), (status = 409, description = "Same-provider signup or conflicting linked identity", body = super::docs::ApiErrorBody), (status = 500, description = "Database unavailable", body = super::docs::ApiErrorBody), (status = 502, description = "Provider HTTP failure or malformed token/profile", body = super::docs::ApiErrorBody), (status = 503, description = "OAuth provider not configured", body = super::docs::ApiErrorBody)) )]
pub(super) async fn osu_redirect(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> ApiResult<Response> {
    callback(state, headers, query, Provider::Osu).await
}
#[utoipa::path(get, path = "/oauth2/twitch-redirect", tag = "oauth2", summary = "Complete Twitch authorization", description = "Exchanges the code with Twitch and validates the token's client ID, user ID and channel:bot scope before accepting its profile. Verified consent is stored with the linked account, never the broadcaster token. A registered identity logs in or reauthorizes with a rotated session; an unknown first identity creates an opaque signup_details cookie and signup=twitch marker. Either signup order links atomically without replacing ownership or resetting settings. A signed-in different Twitch identity is rejected. The nonce and browser auth context must match the initiating request.", params(Callback), security(("twitchState" = [])), responses((status = 303, description = "Login or partial signup completed", headers(("Location" = String, description = "Stored trusted return URL"), ("Set-Cookie" = String, description = "Opaque token session or signup_details and signup cookies"))), (status = 400, description = "Missing, expired, replayed or mismatched state; missing code or provider denial", body = super::docs::ApiErrorBody), (status = 401, description = "Initiating session/signup expired or account deleted", body = super::docs::ApiErrorBody), (status = 409, description = "Same-provider signup or a different Twitch identity than the signed-in account", body = super::docs::ApiErrorBody), (status = 500, description = "Database unavailable", body = super::docs::ApiErrorBody), (status = 502, description = "Provider HTTP failure, malformed token/profile, or token rejected by validation (client ID, user ID or channel:bot scope mismatch)", body = super::docs::ApiErrorBody), (status = 503, description = "OAuth provider not configured", body = super::docs::ApiErrorBody)) )]
pub(super) async fn twitch_redirect(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> ApiResult<Response> {
    callback(state, headers, query, Provider::Twitch).await
}
struct Attempt {
    provider: String,
    return_to: String,
    session_hash: Option<String>,
    signup_hash: Option<String>,
}
async fn callback(
    state: ApiState,
    headers: HeaderMap,
    query: Callback,
    provider: Provider,
) -> ApiResult<Response> {
    let nonce = query
        .state
        .as_deref()
        .ok_or(ApiError(StatusCode::BAD_REQUEST, "Missing OAuth state"))?;
    if nonce.len() != 64
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
        || security::cookie(&headers, provider.cookie()) != Some(nonce)
    {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Invalid OAuth state"));
    }
    let state_hash = security::hash(nonce);
    // DELETE RETURNING makes authorization attempts single-use even if token exchange fails.
    let attempt = state.store.call(move |db| {
        Ok(db.query_row("DELETE FROM oauth_attempts WHERE state_hash=?1 AND expires_at>?2 RETURNING provider,return_to,session_hash,signup_hash", params![state_hash, chrono::Utc::now().timestamp()], |row| Ok(Attempt { provider: row.get(0)?, return_to: row.get(1)?, session_hash: row.get(2)?, signup_hash: row.get(3)? })).optional()?)
    }).await?.ok_or(ApiError(StatusCode::BAD_REQUEST, "Expired or consumed OAuth state"))?;
    if attempt.provider != provider.name()
        || attempt.session_hash != security::cookie(&headers, "token").map(security::hash)
        || attempt.signup_hash != security::cookie(&headers, "signup_details").map(security::hash)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "OAuth browser context changed",
        ));
    }
    if query.error.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Provider authorization denied",
        ));
    }
    let code = query.code.filter(|c| !c.is_empty()).ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        "Missing authorization code",
    ))?;
    let identity = fetch_identity(&state, provider, &code).await?;
    let outcome =
        complete_account(&state, &identity, attempt.session_hash, attempt.signup_hash).await?;
    let mut response = security::redirect(&attempt.return_to);
    security::set_cookie(&mut response, &state.config, provider.cookie(), "", 0);
    match outcome {
        Outcome::Session(token) => {
            security::clear_auth_cookies(&mut response, &state.config);
            security::set_cookie(
                &mut response,
                &state.config,
                "token",
                &token,
                state.config.session_ttl_secs,
            );
        }
        Outcome::Signup(token) => security::set_cookies(
            &mut response,
            &state.config,
            &[
                ("token", "", 0),
                ("signup", provider.name(), state.config.oauth_ttl_secs),
                (
                    "signup_details",
                    token.as_str(),
                    state.config.oauth_ttl_secs,
                ),
            ],
        ),
    }
    Ok(response)
}
fn provider_error() -> ApiError {
    ApiError(StatusCode::BAD_GATEWAY, "OAuth provider request failed")
}
async fn fetch_identity(state: &ApiState, provider: Provider, code: &str) -> ApiResult<Identity> {
    let (client_id, secret, callback, base) = provider.credentials(state);
    if client_id.is_empty() || secret.is_empty() {
        return Err(not_configured());
    }
    let token: Value = state
        .http
        .post(format!("{}/token", base.trim_end_matches('/')))
        .form(&[
            ("client_id", client_id),
            ("client_secret", secret),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", callback),
        ])
        .send()
        .await
        .map_err(|_| provider_error())?
        .error_for_status()
        .map_err(|_| provider_error())?
        .json()
        .await
        .map_err(|_| provider_error())?;
    let token = token
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(provider_error)?;
    // The authorization response alone is not proof of permission. Bind the
    // exchanged token to this application and its actual Twitch user.
    let validated_twitch_id = match provider {
        Provider::Osu => None,
        Provider::Twitch => Some(validate_twitch(state, client_id, token).await?),
    };
    let mut request = state
        .http
        .get(format!(
            "{}/{}",
            provider.api_base(state).trim_end_matches('/'),
            provider.profile_path()
        ))
        .bearer_auth(token);
    if provider == Provider::Twitch {
        request = request.header("Client-Id", client_id);
    }
    let profile: Value = request
        .send()
        .await
        .map_err(|_| provider_error())?
        .error_for_status()
        .map_err(|_| provider_error())?
        .json()
        .await
        .map_err(|_| provider_error())?;
    let profile = provider.profile(&profile).ok_or_else(provider_error)?;
    let id = profile
        .get("id")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|id| *id > 0)
        .ok_or_else(provider_error)?;
    if validated_twitch_id.is_some_and(|validated| validated != id) {
        return Err(provider_error());
    }
    let username = profile
        .get(provider.name_key())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(provider_error)?
        .to_owned();
    let avatar_url = profile
        .get(provider.avatar_key())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Identity {
        provider: provider.name().into(),
        id,
        username,
        avatar_url,
        channel_bot_granted_at: validated_twitch_id.map(|_| chrono::Utc::now().timestamp()),
    })
}
/// Twitch tokens must be bound to this client ID, a positive user ID and the
/// channel:bot scope; the profile id is cross-checked by the caller.
async fn validate_twitch(state: &ApiState, client_id: &str, token: &str) -> ApiResult<i64> {
    #[derive(Deserialize)]
    struct Validation {
        client_id: String,
        user_id: String,
        scopes: Vec<String>,
    }
    let validation: Validation = state
        .http
        .get(format!(
            "{}/validate",
            state.config.twitch_oauth_base.trim_end_matches('/')
        ))
        .header("Authorization", format!("OAuth {token}"))
        .send()
        .await
        .map_err(|_| provider_error())?
        .error_for_status()
        .map_err(|_| provider_error())?
        .json()
        .await
        .map_err(|_| provider_error())?;
    if validation.client_id.as_str() != client_id
        || !validation.scopes.iter().any(|scope| scope == "channel:bot")
    {
        return Err(provider_error());
    }
    validation
        .user_id
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(provider_error)
}

#[derive(Debug)]
pub(super) enum AccountFault {
    Conflict,
    SameProvider,
    Expired,
}
impl AccountFault {
    pub(super) fn message(&self) -> &'static str {
        match self {
            Self::Conflict => "Account is already linked to another identity",
            Self::SameProvider => "Linking requires two different providers",
            Self::Expired => "Signup or session expired",
        }
    }
}
impl std::fmt::Display for AccountFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}
impl std::error::Error for AccountFault {}
enum Outcome {
    Session(String),
    Signup(String),
}
fn constraint(error: rusqlite::Error) -> anyhow::Error {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        AccountFault::Conflict.into()
    } else {
        error.into()
    }
}
async fn complete_account(
    state: &ApiState,
    identity: &Identity,
    session_hash: Option<String>,
    signup_hash: Option<String>,
) -> ApiResult<Outcome> {
    let identity = identity.clone();
    let token = security::random_token();
    let token_hash = security::hash(&token);
    let session_expiry = chrono::Utc::now().timestamp() + state.config.session_ttl_secs;
    let signup_expiry = chrono::Utc::now().timestamp() + state.config.oauth_ttl_secs;
    let grant_grace = state.config.migration_grant_grace_secs;
    let result = state.store.call(move |db| {
        let tx = db.transaction()?;
        let now = chrono::Utc::now().timestamp();
        let anchor = if let Some(hash) = &session_hash {
            Some(tx.query_row("SELECT twitch_id FROM sessions WHERE token_hash=?1 AND expires_at>?2", params![hash, now], |r| r.get::<_, i64>(0)).optional()?.ok_or(AccountFault::Expired)?)
        } else { None };
        let pending = if let Some(hash) = &signup_hash {
            let raw = tx.query_row("SELECT identity_json FROM signup_attempts WHERE token_hash=?1 AND expires_at>?2", params![hash, now], |r| r.get::<_, String>(0)).optional()?.ok_or(AccountFault::Expired)?;
            Some(serde_json::from_str::<Identity>(&raw)?)
        } else { None };
        if pending.as_ref().is_some_and(|p| p.provider == identity.provider) { return Err(AccountFault::SameProvider.into()); }
        let is_osu = identity.provider == "osu";
        let existing = tx.query_row(if is_osu { "SELECT twitch_id,osu_id FROM users WHERE osu_id=?1" } else { "SELECT twitch_id,osu_id FROM users WHERE twitch_id=?1" }, [identity.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).optional()?;
        let pair = if let Some(pending) = &pending {
            let (osu, twitch) = if is_osu { (&identity, pending) } else { (pending, &identity) };
            if existing.is_some_and(|(t,o)| t != twitch.id || o != osu.id) { return Err(AccountFault::Conflict.into()); }
            let other = tx.query_row("SELECT twitch_id,osu_id FROM users WHERE osu_id=?1 OR twitch_id=?2", params![osu.id, twitch.id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).optional()?;
            if other.is_some_and(|(t,o)| t != twitch.id || o != osu.id) { return Err(AccountFault::Conflict.into()); }
            if anchor.is_some_and(|a| a != twitch.id) { return Err(AccountFault::Conflict.into()); }
            if existing.is_none() && other.is_none() {
                tx.execute("INSERT INTO users(osu_id,osu_username,osu_avatar_url,twitch_id,twitch_username,twitch_avatar_url,settings_json) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![osu.id, osu.username, osu.avatar_url, twitch.id, twitch.username.to_ascii_lowercase(), twitch.avatar_url, serde_json::to_string(&Settings::default())?]).map_err(constraint)?;
            }
            // Only verified profile data is refreshed; effective settings/exclusions stay untouched.
            tx.execute("UPDATE users SET osu_username=?1,osu_avatar_url=?2,twitch_username=?3,twitch_avatar_url=?4 WHERE osu_id=?5 AND twitch_id=?6", params![osu.username, osu.avatar_url, twitch.username.to_ascii_lowercase(), twitch.avatar_url, osu.id, twitch.id]).map_err(constraint)?;
            Some((twitch.id, osu.id))
        } else { existing };
        if let Some((twitch_id, osu_id)) = pair {
            if anchor.is_some_and(|a| a != twitch_id) { return Err(AccountFault::Conflict.into()); }
            if is_osu {
                tx.execute("UPDATE users SET osu_username=?1,osu_avatar_url=?2 WHERE osu_id=?3", params![identity.username, identity.avatar_url, osu_id]).map_err(constraint)?;
            } else {
                tx.execute("UPDATE users SET twitch_username=?1,twitch_avatar_url=?2 WHERE twitch_id=?3", params![identity.username.to_ascii_lowercase(), identity.avatar_url, twitch_id]).map_err(constraint)?;
            }
            let grant = if is_osu {
                pending.as_ref().filter(|p| p.provider == "twitch")
                    .and_then(|p| p.channel_bot_granted_at)
            } else {
                identity.channel_bot_granted_at
            };
            if let Some(granted_at) = grant {
                let active_at = now + grant_grace as i64;
                tx.execute(
                    "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(?1,?2,?3,'granted') ON CONFLICT(twitch_id) DO UPDATE SET granted_at=excluded.granted_at,active_at=excluded.active_at,status='granted'",
                    params![twitch_id, granted_at, active_at],
                )?;
                tx.execute("DELETE FROM legacy_live WHERE twitch_id=?1", [twitch_id])?;
            }
            crate::storage::refresh_eligibility(&tx, twitch_id)?;
            if let Some(hash) = session_hash { tx.execute("DELETE FROM sessions WHERE token_hash=?1", [hash])?; }
            if let Some(hash) = signup_hash { tx.execute("DELETE FROM signup_attempts WHERE token_hash=?1", [hash])?; }
            tx.execute("INSERT INTO sessions(token_hash,twitch_id,expires_at) VALUES(?1,?2,?3)", params![token_hash, twitch_id, session_expiry])?;
            tx.commit()?;
            Ok(Outcome::Session(token))
        } else {
            if anchor.is_some() { return Err(AccountFault::Conflict.into()); }
            tx.execute("INSERT INTO signup_attempts(token_hash,provider,identity_json,expires_at) VALUES(?1,?2,?3,?4)", params![token_hash, identity.provider, serde_json::to_string(&identity)?, signup_expiry])?;
            tx.commit()?;
            Ok(Outcome::Signup(token))
        }
    }).await;
    if matches!(&result, Ok(Outcome::Session(_))) {
        // The new bot must not see the grant during the grace window, but it
        // should not have to wait for a later audit once activation elapses.
        if grant_grace == 0 {
            state.store.notify_channels_changed();
        } else {
            let store = state.store.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(grant_grace)).await;
                store.notify_channels_changed();
            });
        }
    }
    match result {
        Err(error)
            if error
                .downcast_ref::<AccountFault>()
                .is_some_and(|e| matches!(e, AccountFault::Expired)) =>
        {
            Err(unauthorized())
        }
        other => other.map_err(ApiError::from),
    }
}
