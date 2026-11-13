use super::{ApiError, ApiResult, ApiState, unauthorized};
use crate::{config::Config, domain::User};
use axum::{
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
};
use rand::RngCore;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

pub(super) fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
pub(super) fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .find_map(|(key, value)| (key == name && !value.is_empty()).then_some(value))
}
pub(super) fn set_cookie(
    response: &mut Response,
    config: &Config,
    name: &str,
    value: &str,
    max_age: i64,
) {
    // All values are generated hex or fixed provider names, never upstream input.
    let secure = if config.cookie_secure { "; Secure" } else { "" };
    let value =
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}");
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&value).expect("generated cookie"),
    );
}
/// One response may expire several cookies; keep their shared attributes central.
pub(super) fn set_cookies(response: &mut Response, config: &Config, cookies: &[(&str, &str, i64)]) {
    for &(name, value, max_age) in cookies {
        set_cookie(response, config, name, value, max_age);
    }
}
pub(super) fn clear_auth_cookies(response: &mut Response, config: &Config) {
    set_cookies(
        response,
        config,
        &[
            ("token", "", 0),
            ("signup", "", 0),
            ("signup_details", "", 0),
        ],
    );
}
fn permitted_url(config: &Config, value: &str) -> Option<Url> {
    let url = Url::parse(value).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let origin = url.origin().ascii_serialization();
    [config.public_origin.as_str(), config.api_origin.as_str()]
        .into_iter()
        .filter_map(|s| Url::parse(s).ok())
        .any(|allowed| allowed.origin().ascii_serialization() == origin)
        .then_some(url)
}
pub(super) fn require_origin(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    if let Some(origin) = headers.get(header::ORIGIN) {
        let trusted = origin.to_str().ok().and_then(|v| {
            let parsed = permitted_url(config, v)?;
            // Origin syntax has no path/query; do not accept origin-prefix tricks.
            (parsed.path() == "/" && parsed.query().is_none() && parsed.fragment().is_none())
                .then_some(())
        });
        return trusted.ok_or(ApiError(StatusCode::FORBIDDEN, "Untrusted request origin"));
    }
    headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| permitted_url(config, v))
        .map(|_| ())
        .ok_or(ApiError(
            StatusCode::FORBIDDEN,
            "Trusted Origin or Referer required",
        ))
}
pub(super) fn return_to(config: &Config, headers: &HeaderMap) -> String {
    headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|value| permitted_url(config, value))
        .map(String::from)
        .unwrap_or_else(|| config.public_origin.clone())
}
pub(super) fn redirect(target: &str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::SEE_OTHER;
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(target).expect("validated URL"),
    );
    response
}
/// A browser mutation that revokes the current login and returns to a trusted URL.
pub(super) fn redirect_cleared(config: &Config, headers: &HeaderMap) -> Response {
    let mut response = redirect(&return_to(config, headers));
    clear_auth_cookies(&mut response, config);
    response
}
/// Internal relay authentication. An unconfigured or absent token hides the
/// route entirely; a supplied but wrong or malformed token is rejected.
pub(super) fn require_internal(config: &Config, headers: &HeaderMap) -> ApiResult<()> {
    let Some(expected) = config
        .internal_api_token
        .as_deref()
        .filter(|token| !token.is_empty())
    else {
        return Err(ApiError(StatusCode::NOT_FOUND, "Not found"));
    };
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(ApiError(StatusCode::NOT_FOUND, "Not found"));
    };
    let Ok(value) = value.to_str() else {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Invalid internal token"));
    };
    let Some(token) = value
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
    else {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Invalid internal token"));
    };
    // Compare fixed-size digests without early exits to avoid leaking the
    // secret's length or a matching prefix through timing.
    let actual = Sha256::digest(token.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    let mut difference = 0u8;
    for (left, right) in actual.iter().zip(expected.iter()) {
        difference |= left ^ right;
    }
    if difference == 0 {
        Ok(())
    } else {
        Err(ApiError(StatusCode::UNAUTHORIZED, "Invalid internal token"))
    }
}

pub(super) async fn authenticate(state: &ApiState, headers: &HeaderMap) -> ApiResult<User> {
    let token = cookie(headers, "token").ok_or_else(unauthorized)?;
    let token_hash = hash(token);
    let id = state
        .store
        .call(move |db| {
            Ok(db
                .query_row(
                    "SELECT twitch_id FROM sessions WHERE token_hash=?1 AND expires_at>?2",
                    rusqlite::params![token_hash, chrono::Utc::now().timestamp()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?)
        })
        .await?;
    let id = id.ok_or_else(unauthorized)?;
    state
        .store
        .user_by_twitch(id)
        .await?
        .ok_or_else(unauthorized)
}
/// Whether the browser carries a currently valid session; expired or legacy
/// credentials are simply absent, while a database failure still propagates.
pub(super) async fn session_valid(state: &ApiState, headers: &HeaderMap) -> ApiResult<bool> {
    if cookie(headers, "token").is_none() {
        return Ok(false);
    }
    match authenticate(state, headers).await {
        Ok(_) => Ok(true),
        Err(ApiError(StatusCode::UNAUTHORIZED, _)) => Ok(false),
        Err(error) => Err(error),
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Identity {
    pub provider: String,
    pub id: i64,
    pub username: String,
    pub avatar_url: String,
    /// Present only after Twitch token validation confirms channel:bot consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_bot_granted_at: Option<i64>,
}
pub(super) async fn pending(state: &ApiState, headers: &HeaderMap) -> ApiResult<Option<Identity>> {
    let Some(token) = cookie(headers, "signup_details") else {
        return Ok(None);
    };
    let token_hash = hash(token);
    let identity = state.store.call(move |db| {
        Ok(db.query_row("SELECT identity_json FROM signup_attempts WHERE token_hash=?1 AND expires_at>?2", rusqlite::params![token_hash, chrono::Utc::now().timestamp()], |row| row.get::<_, String>(0)).optional()?)
    }).await?;
    identity
        .map(|s| {
            serde_json::from_str(&s)
                .map_err(anyhow::Error::from)
                .map_err(ApiError::from)
        })
        .transpose()
}
