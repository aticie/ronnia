//! Debug-only API specification and browser documentation pages.
// Public response DTOs and schema projections deliberately exclude storage-only fields.
#![allow(dead_code)]

use super::ApiState;
use axum::{
    Router,
    body::Bytes,
    http::{HeaderValue, header},
    response::Html,
    routing::get,
};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use utoipa::{
    IntoParams, Modify, OpenApi, ToSchema,
    openapi::security::{ApiKey, ApiKeyValue, SecurityScheme},
};

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserResponse {
    osu_id: i64,
    osu_username: String,
    osu_avatar_url: String,
    twitch_id: i64,
    twitch_username: String,
    twitch_avatar_url: String,
    excluded_users: Vec<String>,
    is_live: bool,
    /// Whether this channel has verified, non-revoked channel:bot consent.
    twitch_bot_authorized: bool,
}
impl UserResponse {
    pub(super) fn new(user: crate::domain::User, twitch_bot_authorized: bool) -> Self {
        Self {
            osu_id: user.osu_id,
            osu_username: user.osu_username,
            osu_avatar_url: user.osu_avatar_url,
            twitch_id: user.twitch_id,
            twitch_username: user.twitch_username,
            twitch_avatar_url: user.twitch_avatar_url,
            excluded_users: user.excluded_users,
            is_live: user.is_live,
            twitch_bot_authorized,
        }
    }
}
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum SignupProvider {
    Osu,
    Twitch,
}
#[derive(Serialize, ToSchema)]
pub(super) struct SignupStatus {
    #[schema(value_type = SignupProvider)]
    pub(super) signup: String,
}
/// JSON null, not a schema accepting arbitrary JSON values.
#[derive(Serialize)]
pub(super) struct NullResponse;
impl utoipa::PartialSchema for NullResponse {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        utoipa::openapi::schema::ObjectBuilder::new()
            .schema_type(utoipa::openapi::schema::Type::Null)
            .into()
    }
}
impl ToSchema for NullResponse {}
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub(super) enum MeResponse {
    User(UserResponse),
    Signup(SignupStatus),
    Anonymous(NullResponse),
}
#[derive(Serialize)]
#[serde(untagged)]
enum SettingValue {
    Toggle(bool),
    Number(f64),
    Range([f64; 2]),
}
impl utoipa::PartialSchema for SettingValue {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{ArrayBuilder, OneOfBuilder};
        OneOfBuilder::new()
            .item(utoipa::schema!(bool))
            .item(utoipa::schema!(f64))
            .item(
                ArrayBuilder::new()
                    .items(utoipa::schema!(f64))
                    .min_items(Some(2))
                    .max_items(Some(2)),
            )
            .into()
    }
}
impl ToSchema for SettingValue {}
#[derive(Serialize, ToSchema)]
pub(super) struct SettingMetadata {
    name: String,
    value: SettingValue,
    description: String,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Serialize, ToSchema)]
pub(super) struct ApiErrorBody {
    detail: String,
}
/// Full cached upstream beatmap fields are flattened alongside integer _id and count.
#[derive(Serialize)]
pub(super) struct LeaderboardEntry {
    #[serde(rename = "_id")]
    beatmap_id: i64,
    count: i64,
    #[serde(flatten)]
    beatmap: BTreeMap<String, Value>,
}
impl utoipa::PartialSchema for LeaderboardEntry {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{AdditionalProperties, ObjectBuilder};
        ObjectBuilder::new()
            .description(Some("Full cached upstream beatmap fields are flattened alongside integer _id and count."))
            .property("_id", utoipa::schema!(i64))
            .property("count", utoipa::schema!(i64))
            .required("_id")
            .required("count")
            .additional_properties(Some(AdditionalProperties::FreeForm(true)))
            .into()
    }
}
impl ToSchema for LeaderboardEntry {}
#[derive(Serialize, ToSchema)]
pub(super) struct HealthStatus {
    status: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct MigrationStatus {
    /// Whether the coexistence migration banner should be shown.
    pub(super) active: bool,
    /// Optional RFC 3339 cutoff shown in the banner.
    pub(super) cutoff_at: Option<String>,
}
#[derive(IntoParams)]
#[into_params(parameter_in = Header)]
pub(super) struct BrowserOrigin {
    /// Exact trusted frontend/API origin. Supply this OR a trusted Referer for cookie mutations.
    #[param(rename = "Origin")]
    origin: Option<String>,
    /// Trusted frontend/API URL. Used only when Origin is absent; also selects a safe return URL.
    #[param(rename = "Referer")]
    referer: Option<String>,
}
struct BrowserSecurity;
impl Modify for BrowserSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = &mut openapi.components {
            for (scheme, cookie) in [
                ("session", "token"),
                ("signup", "signup_details"),
                ("osuState", "oauth_state_osu"),
                ("twitchState", "oauth_state_twitch"),
            ] {
                components.add_security_scheme(
                    scheme,
                    SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(cookie))),
                );
            }
        }
    }
}
#[derive(OpenApi)]
#[openapi(
    info(title = "Ronnia API", description = "Website API. Authentication uses fresh opaque HttpOnly cookies, not legacy JWTs. Browser cookie mutations (including GET logout) require a trusted Origin or Referer. OAuth callbacks require a browser-bound single-use state and preserve account ownership. Interactive docs are available only with DEBUG_MODE=true."),
    paths(
        super::oauth::osu_login, super::oauth::osu_redirect,
        super::oauth::twitch_login, super::oauth::twitch_redirect,
        super::me, super::logout, super::delete_me,
        super::settings, super::update_settings, super::excluded,
        super::add_excluded, super::remove_excluded, super::live,
        super::daily, super::weekly, super::monthly, super::migration_status,
        super::health, super::ready
    ),
    components(schemas(UserResponse, SignupProvider, SignupStatus, MeResponse, NullResponse, SettingValue, SettingMetadata, ApiErrorBody, LeaderboardEntry, HealthStatus, MigrationStatus, crate::domain::Settings)),
    modifiers(&BrowserSecurity),
    tags(
        (name = "oauth2", description = "OAuth login and two-provider account signup"),
        (name = "user", description = "Account, settings and excluded requesters"),
        (name = "live", description = "Currently live Twitch channels"),
        (name = "requests", description = "Requested beatmap leaderboards"),
        (name = "migration", description = "Public coexistence migration banner state"),
        (name = "operations", description = "Process and initialized database status")
    )
)]
struct ApiDoc;

pub(super) fn router() -> Router<ApiState> {
    // Serialize once at router construction; requests share immutable spec bytes.
    let document = Bytes::from(
        ApiDoc::openapi()
            .to_json()
            .expect("serializable OpenAPI document"),
    );
    Router::new()
        .route(
            "/openapi.json",
            get(move || {
                let document = document.clone();
                async move {
                    (
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        document,
                    )
                }
            }),
        )
        .route("/docs", get(|| async { Html(SWAGGER_UI) }))
        .route("/redoc", get(|| async { Html(REDOC) }))
}
const SWAGGER_UI: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Ronnia API - Swagger UI</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.30.1/swagger-ui.css"></head>
<body><div id="swagger-ui"></div><script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5.30.1/swagger-ui-bundle.js"></script>
<script>SwaggerUIBundle({url:'/openapi.json',dom_id:'#swagger-ui',deepLinking:true,withCredentials:true});</script></body></html>"#;
const REDOC: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Ronnia API - ReDoc</title></head>
<body><redoc spec-url="/openapi.json"></redoc><script src="https://cdn.jsdelivr.net/npm/redoc@2.5.1/bundles/redoc.standalone.js"></script></body></html>"#;
