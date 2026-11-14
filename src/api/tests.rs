use super::*;
use crate::domain::{RequestEvent, StreamSnapshot, User};
use axum::{
    Router,
    body::Body,
    extract::{Form, State},
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tower::ServiceExt;

struct Fixture {
    state: ApiState,
    provider: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.provider.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        Self::build(0, false, None, None).await
    }
    async fn with_grace(grace: u64) -> Self {
        Self::build(grace, false, None, None).await
    }
    async fn with_internal(token: &str) -> Self {
        Self::build(0, false, None, Some(token.into())).await
    }
    async fn with_migration(active: bool, cutoff: Option<&str>) -> Self {
        Self::build(0, active, cutoff.map(str::to_owned), None).await
    }
    async fn build(
        grant_grace: u64,
        migration_active: bool,
        migration_cutoff_at: Option<String>,
        internal_api_token: Option<String>,
    ) -> Self {
        async fn token(
            State(provider): State<&'static str>,
            Form(form): Form<BTreeMap<String, String>>,
        ) -> Response {
            assert_eq!(
                form.get("grant_type").map(String::as_str),
                Some("authorization_code")
            );
            assert_eq!(
                form.get("client_id"),
                Some(&format!("test-{provider}-client"))
            );
            assert_eq!(
                form.get("client_secret"),
                Some(&format!("test-{provider}-secret"))
            );
            assert_eq!(
                form.get("redirect_uri"),
                Some(&format!("http://api.test/oauth2/{provider}-redirect"))
            );
            let code = form.get("code").unwrap();
            if code == "fail" {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if code == "malformed" {
                return Json(json!({"unexpected":true})).into_response();
            }
            Json(json!({"access_token":code})).into_response()
        }
        async fn validate(headers: HeaderMap) -> Response {
            let token = headers[header::AUTHORIZATION]
                .to_str()
                .unwrap()
                .strip_prefix("OAuth ")
                .unwrap();
            match token {
                "denied-validation" => StatusCode::UNAUTHORIZED.into_response(),
                "failed-validation" => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                "malformed-validation" => Json(json!({"unexpected":true})).into_response(),
                _ => {
                    let user_id = match token {
                        "wrong-client" | "missing-scope" | "wrong-user" => "21",
                        "invalid-user" => "",
                        id => id,
                    };
                    Json(json!({
                        "client_id": if token == "wrong-client" { "other-client" } else { "test-twitch-client" },
                        "user_id": user_id,
                        "scopes": if token == "missing-scope" { vec!["user:read:email"] } else { vec!["channel:bot"] },
                    })).into_response()
                }
            }
        }
        async fn profile(State(twitch): State<bool>, headers: HeaderMap) -> Json<Value> {
            let token = headers[header::AUTHORIZATION]
                .to_str()
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap();
            let id: i64 = if token == "wrong-user" {
                20
            } else {
                token.parse().unwrap()
            };
            if twitch {
                assert_eq!(&headers["Client-Id"], "test-twitch-client");
                Json(
                    json!({"data":[{"id":id.to_string(),"login":format!("streamer{id}"),"profile_image_url":"https://images.test/twitch"}]}),
                )
            } else {
                Json(
                    json!({"id":id,"username":format!("player{id}"),"avatar_url":"https://images.test/osu"}),
                )
            }
        }
        let providers = Router::new()
            .merge(
                Router::new()
                    .route("/twitch/oauth/token", post(token))
                    .with_state("twitch"),
            )
            .route("/twitch/oauth/validate", get(validate))
            .merge(
                Router::new()
                    .route("/osu/oauth/token", post(token))
                    .with_state("osu"),
            )
            .merge(
                Router::new()
                    .route("/twitch/api/users", get(profile))
                    .with_state(true),
            )
            .merge(
                Router::new()
                    .route("/osu/api/me", get(profile))
                    .with_state(false),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move {
            axum::serve(listener, providers).await.unwrap();
        });
        let config = Arc::new(Config {
            database_path: ":memory:".into(),
            bind: "127.0.0.1".parse().unwrap(),
            bind_port: 0,
            public_origin: "http://website.test".into(),
            api_origin: "http://api.test".into(),
            cookie_secure: false,
            debug_mode: false,
            twitch_client_id: "test-twitch-client".into(),
            twitch_client_secret: "test-twitch-secret".into(),
            twitch_redirect_uri: "http://api.test/oauth2/twitch-redirect".into(),
            osu_client_id: "test-osu-client".into(),
            osu_client_secret: "test-osu-secret".into(),
            osu_redirect_uri: "http://api.test/oauth2/osu-redirect".into(),
            twitch_eventsub_url: "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30"
                .into(),
            twitch_token_key: None,
            twitch_api_base: format!("{base}/twitch/api"),
            twitch_oauth_base: format!("{base}/twitch/oauth"),
            osu_api_base: format!("{base}/osu/api"),
            osu_oauth_base: format!("{base}/osu/oauth"),
            dry_run: true,
            reconcile_interval_secs: 600,
            eventsub_channels_per_shard: 100,
            eventsub_max_shards: 16,
            request_workers: 8,
            request_queue_capacity: 128,
            http_timeout_secs: 2,
            session_ttl_secs: 86400,
            oauth_ttl_secs: 600,
            bot_enabled: false,
            migration_active,
            migration_cutoff_at,
            migration_grant_grace_secs: grant_grace,
            internal_api_token,
        });
        let store = Store::open(":memory:").await.unwrap();
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap();
        Self {
            state: ApiState {
                config,
                store,
                http,
            },
            provider,
        }
    }
    fn browser(&self) -> Browser {
        Browser {
            app: router(self.state.config.clone(), self.state.store.clone()),
            jar: BTreeMap::new(),
        }
    }
}
struct Browser {
    app: Router,
    jar: BTreeMap<String, String>,
}
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}
impl Browser {
    async fn request(
        &mut self,
        method: Method,
        path: &str,
        payload: Option<Value>,
        origin: Option<&str>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path);
        if !self.jar.is_empty() {
            request = request.header(
                header::COOKIE,
                self.jar
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let body = if let Some(value) = payload {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        } else {
            Body::empty()
        };
        self.finish(request.body(body).unwrap()).await
    }
    async fn internal(
        &mut self,
        method: Method,
        path: &str,
        payload: Option<Value>,
        token: Option<&str>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(path);
        if !self.jar.is_empty() {
            request = request.header(
                header::COOKIE,
                self.jar
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = if let Some(value) = payload {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        } else {
            Body::empty()
        };
        self.finish(request.body(body).unwrap()).await
    }
    async fn finish(&mut self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for cookie in headers.get_all(header::SET_COOKIE) {
            let cookie = cookie.to_str().unwrap();
            assert!(cookie.contains("HttpOnly"));
            assert!(cookie.contains("SameSite=Lax"));
            let (key, value) = cookie.split(';').next().unwrap().split_once('=').unwrap();
            if cookie.contains("Max-Age=0;") || cookie.ends_with("Max-Age=0") {
                self.jar.remove(key);
            } else {
                self.jar.insert(key.to_owned(), value.to_owned());
            }
        }
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
        {
            serde_json::from_slice(&bytes).expect("valid JSON response body")
        } else {
            Value::Null
        };
        Reply {
            status,
            headers,
            json,
        }
    }
    async fn get(&mut self, path: &str) -> Reply {
        self.request(Method::GET, path, None, None).await
    }
    async fn start(&mut self, provider: &str) -> String {
        let reply = self.get(&format!("/oauth2/{provider}-login")).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        let location = url::Url::parse(reply.headers[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(location.path(), format!("/{provider}/oauth/authorize"));
        let query = location.query_pairs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            query.get("client_id").map(|v| &**v),
            Some(format!("test-{provider}-client").as_str())
        );
        assert_eq!(
            query.get("redirect_uri").map(|v| &**v),
            Some(format!("http://api.test/oauth2/{provider}-redirect").as_str())
        );
        assert_eq!(
            query.get("scope").map(|v| &**v),
            Some(if provider == "twitch" {
                "channel:bot"
            } else {
                "identify"
            })
        );
        if provider == "twitch" {
            assert_eq!(query.get("force_verify").map(|v| &**v), Some("true"));
        }
        location
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned()
    }
    async fn callback(&mut self, provider: &str, code: &str, state: &str) -> Reply {
        self.get(&format!(
            "/oauth2/{provider}-redirect?code={code}&state={state}"
        ))
        .await
    }
    async fn oauth(&mut self, provider: &str, id: i64) -> Reply {
        let state = self.start(provider).await;
        self.callback(provider, &id.to_string(), &state).await
    }
    async fn signup(&mut self, first: &str, osu_id: i64, twitch_id: i64) {
        let second = if first == "osu" { "twitch" } else { "osu" };
        assert_eq!(
            self.oauth(first, if first == "osu" { osu_id } else { twitch_id })
                .await
                .status,
            StatusCode::SEE_OTHER
        );
        assert_eq!(self.get("/user/me").await.json, json!({"signup":first}));
        assert_eq!(
            self.oauth(second, if second == "osu" { osu_id } else { twitch_id })
                .await
                .status,
            StatusCode::SEE_OTHER
        );
        assert!(self.jar.contains_key("token"));
        assert!(!self.jar.contains_key("signup_details"));
        let user = self.get("/user/me").await;
        assert_eq!(user.status, StatusCode::OK);
        assert_eq!(user.json["osuId"], osu_id);
        assert_eq!(user.json["twitchId"], twitch_id);
        assert_eq!(user.json["twitchBotAuthorized"], true);
        assert!(user.json.get("settings").is_none());
    }
}

#[tokio::test]
async fn both_signup_orders_issue_only_hashed_sessions() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    browser.signup("osu", 10, 20).await;
    let mut reverse = fixture.browser();
    reverse.signup("twitch", 11, 21).await;
    let raw = browser.jar["token"].clone();
    assert_eq!(raw.len(), 64);
    let digest = security::hash(&raw);
    fixture
        .state
        .store
        .call(move |db| {
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE token_hash=?1",
                    [digest],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM sessions WHERE token_hash=?1",
                    [raw],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM signup_attempts", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(db.query_row(
                "SELECT COUNT(*) FROM twitch_channel_grants WHERE twitch_id IN (20,21) AND status='granted' AND granted_at>0",
                [], |r| r.get::<_, i64>(0),
            )?, 2);
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn grant_settings_and_deletion_notify_the_channel_watch() {
    let fixture = Fixture::new().await;
    let mut changes = fixture.state.store.channel_changes();
    changes.borrow_and_update();
    let mut browser = fixture.browser();
    browser.signup("osu", 10, 20).await;
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert_eq!(
        browser
            .request(
                Method::POST,
                "/user/settings",
                Some(json!({"enable":false})),
                Some("http://website.test")
            )
            .await
            .status,
        StatusCode::OK
    );
    assert!(changes.has_changed().unwrap());
    changes.borrow_and_update();
    assert_eq!(
        browser
            .request(
                Method::DELETE,
                "/user/me",
                None,
                Some("http://website.test")
            )
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert!(changes.has_changed().unwrap());
    fixture
        .state
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM twitch_channel_grants", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn imported_accounts_need_verified_consent_before_bot_eligibility() {
    let fixture = Fixture::new().await;
    fixture
        .state
        .store
        .save_user(User {
            osu_id: 10,
            osu_username: "player10".into(),
            osu_avatar_url: String::new(),
            twitch_id: 20,
            twitch_username: "streamer20".into(),
            twitch_avatar_url: String::new(),
            excluded_users: Vec::new(),
            is_live: true,
            settings: crate::domain::Settings::default(),
        })
        .await
        .unwrap();
    let revision = fixture
        .state
        .store
        .snapshot_versions(vec![20])
        .await
        .unwrap();
    fixture
        .state
        .store
        .apply_stream_snapshot(vec![StreamSnapshot {
            twitch_id: 20,
            expected_revision: revision[0].1,
            online: true,
            category_id: "21465".into(),
            login: None,
        }])
        .await
        .unwrap();
    assert!(
        fixture
            .state
            .store
            .live_channels()
            .await
            .unwrap()
            .is_empty()
    );
    let raw = security::random_token();
    let digest = security::hash(&raw);
    fixture
        .state
        .store
        .call(move |db| {
            db.execute(
                "INSERT INTO sessions(token_hash,twitch_id,expires_at) VALUES(?1,20,?2)",
                rusqlite::params![digest, chrono::Utc::now().timestamp() + 60],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut browser = fixture.browser();
    browser.jar.insert("token".into(), raw);
    let user = browser.get("/user/me").await;
    assert_eq!(user.json["twitchBotAuthorized"], false);
    assert_eq!(user.json["isLive"], false);
    assert_eq!(browser.get("/live/users").await.json, json!([]));
    // An osu! login alone never fabricates Twitch chat consent.
    assert_eq!(browser.oauth("osu", 10).await.status, StatusCode::SEE_OTHER);
    assert_eq!(
        browser.get("/user/me").await.json["twitchBotAuthorized"],
        false
    );
    fixture
        .state
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM twitch_channel_grants", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        browser.oauth("twitch", 20).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser.get("/user/me").await.json["twitchBotAuthorized"],
        true
    );
    // Verified consent immediately makes the known online osu! stream eligible.
    assert_eq!(browser.get("/user/me").await.json["isLive"], true);
    assert_eq!(browser.get("/live/users").await.json, json!(["streamer20"]));
    assert_eq!(
        fixture.state.store.live_channels().await.unwrap(),
        vec![(20, "streamer20".into())]
    );
    fixture
        .state
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM twitch_bot_auth", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    // Logging out keeps consent; explicit revocation removes eligibility.
    fixture.state.store.revoke_twitch_channel(20).await.unwrap();
    assert_eq!(
        browser.get("/user/me").await.json["twitchBotAuthorized"],
        false
    );
    assert!(
        fixture
            .state
            .store
            .live_channels()
            .await
            .unwrap()
            .is_empty()
    );
    // Fresh consent through the same Twitch identity restores the grant.
    assert_eq!(
        browser.oauth("twitch", 20).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser.get("/user/me").await.json["twitchBotAuthorized"],
        true
    );
    assert_eq!(browser.get("/user/me").await.json["isLive"], true);
}

#[tokio::test]
async fn twitch_reauth_requires_the_same_identity_and_invalid_tokens_grant_nothing() {
    let fixture = Fixture::new().await;
    let mut owner = fixture.browser();
    owner.signup("osu", 10, 20).await;
    let old_token = owner.jar["token"].clone();
    let previous_grant_revision: i64 = fixture
        .state
        .store
        .call(|db| {
            Ok(db.query_row(
                "SELECT revision FROM twitch_channel_grants WHERE twitch_id=20",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    // Reauthorizing the same Twitch identity rotates the session.
    assert_eq!(
        owner.oauth("twitch", 20).await.status,
        StatusCode::SEE_OTHER
    );
    assert_ne!(owner.jar["token"], old_token);
    assert!(
        !fixture
            .state
            .store
            .revoke_twitch_channel_if_current(20, previous_grant_revision)
            .await
            .unwrap(),
        "an in-flight revocation must not undo a newer browser reauthorization"
    );
    // A different identity must not replace account ownership.
    assert_eq!(owner.oauth("twitch", 21).await.status, StatusCode::CONFLICT);
    assert_eq!(
        fixture
            .state
            .store
            .user_by_twitch(20)
            .await
            .unwrap()
            .unwrap()
            .osu_id,
        10
    );
    assert!(
        fixture
            .state
            .store
            .user_by_twitch(21)
            .await
            .unwrap()
            .is_none()
    );
    for code in [
        "wrong-client",
        "missing-scope",
        "wrong-user",
        "invalid-user",
        "denied-validation",
        "failed-validation",
        "malformed-validation",
    ] {
        let mut browser = fixture.browser();
        let nonce = browser.start("twitch").await;
        assert_eq!(
            browser.callback("twitch", code, &nonce).await.status,
            StatusCode::BAD_GATEWAY,
            "invalid Twitch authorization {code} was accepted"
        );
        assert!(!browser.jar.contains_key("token"));
        assert!(!browser.jar.contains_key("signup_details"));
    }
    fixture
        .state
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))?,
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM twitch_channel_grants WHERE status='granted'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn public_routes_and_bounded_pagination_preserve_shapes() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    let profile = browser.get("/user/me").await;
    assert_eq!(profile.status, StatusCode::OK);
    assert_eq!(profile.headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(profile.json, Value::Null);
    for path in ["/user/settings", "/user/exclude"] {
        assert_eq!(browser.get(path).await.status, StatusCode::UNAUTHORIZED);
    }
    for path in [
        "/live/users",
        "/requests/beatmaps/top/daily",
        "/requests/beatmaps/top/weekly",
        "/requests/beatmaps/top/monthly",
    ] {
        assert_eq!(browser.get(path).await.json, json!([]));
        for query in ["limit=0", "limit=101", "offset=-1"] {
            assert_eq!(
                browser.get(&format!("{path}?{query}")).await.status,
                StatusCode::UNPROCESSABLE_ENTITY
            );
        }
    }
    let cors = browser
        .request(
            Method::GET,
            "/live/users",
            None,
            Some("http://website.test"),
        )
        .await;
    assert_eq!(
        cors.headers[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "http://website.test"
    );
    assert_eq!(
        cors.headers[header::ACCESS_CONTROL_ALLOW_CREDENTIALS],
        "true"
    );
    let cors = browser
        .request(Method::GET, "/live/users", None, Some("http://evil.test"))
        .await;
    // A fixed trusted origin header also denies the hostile browser origin;
    // CORS does not require that header to be absent on denied requests.
    assert!(
        cors.headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none_or(|origin| origin != "http://evil.test" && origin != "*")
    );
    assert_eq!(browser.get("/ready").await.json, json!({"status":"ready"}));
}

#[tokio::test]
async fn oauth_nonce_is_browser_bound_expiring_and_single_use() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    let nonce = browser.start("osu").await;
    let mut attacker = fixture.browser();
    assert_eq!(
        attacker.callback("osu", "10", &nonce).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        browser.callback("osu", "10", &nonce).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser.callback("osu", "10", &nonce).await.status,
        StatusCode::BAD_REQUEST
    );
    let nonce = browser.start("twitch").await;
    fixture
        .state
        .store
        .call(|db| {
            db.execute("UPDATE oauth_attempts SET expires_at=0", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        browser.callback("twitch", "20", &nonce).await.status,
        StatusCode::BAD_REQUEST
    );
    let nonce = browser.start("twitch").await;
    browser.jar.remove("signup_details");
    assert_eq!(
        browser.callback("twitch", "20", &nonce).await.status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn cancelled_oauth_returns_to_frontend_without_changing_authentication() {
    let fixture = Fixture::new().await;
    let mut owner = fixture.browser();
    owner.signup("osu", 10, 20).await;
    for provider in ["osu", "twitch"] {
        for context in ["anonymous", "signup", "session"] {
            let mut browser = fixture.browser();
            if context == "signup" {
                let first = if provider == "osu" { "twitch" } else { "osu" };
                assert_eq!(browser.oauth(first, 30).await.status, StatusCode::SEE_OTHER);
            } else if context == "session" {
                browser.jar = owner.jar.clone();
            }
            let before = browser.get("/user/me").await.json;
            let nonce = browser.start(provider).await;
            let mut expected_cookies = browser.jar.clone();
            expected_cookies.remove(&format!("oauth_state_{provider}"));
            let path = format!(
                "/oauth2/{provider}-redirect?error=access_denied&error_description=Cancelled&state={nonce}"
            );

            // A denial still needs the initiating browser's state cookie.
            let mut attacker = fixture.browser();
            assert_eq!(attacker.get(&path).await.status, StatusCode::BAD_REQUEST);
            let reply = browser.get(&path).await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER);
            assert_eq!(
                reply.headers[header::LOCATION],
                "http://website.test/login?login_error=1"
            );
            assert_eq!(browser.jar, expected_cookies);
            assert_eq!(browser.get("/user/me").await.json, before);

            // Restoring the cookie cannot replay a consumed cancellation.
            browser.jar.insert(format!("oauth_state_{provider}"), nonce);
            assert_eq!(browser.get(&path).await.status, StatusCode::BAD_REQUEST);
        }
    }
}

#[tokio::test]
async fn provider_failures_do_not_crash_and_legacy_jwts_require_fresh_login() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    browser
        .jar
        .insert("token".into(), "old.jwt.must.not.work".into());
    assert_eq!(
        browser.get("/user/me").await.status,
        StatusCode::UNAUTHORIZED
    );
    for code in ["fail", "malformed"] {
        let nonce = browser.start("osu").await;
        assert!(!browser.jar.contains_key("token"));
        assert_eq!(
            browser.callback("osu", code, &nonce).await.status,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(browser.get("/health").await.status, StatusCode::OK);
        assert_eq!(
            browser.callback("osu", code, &nonce).await.status,
            StatusCode::BAD_REQUEST
        );
    }
    browser.signup("osu", 10, 20).await;
}

#[tokio::test]
async fn settings_exclusions_and_repeat_login_keep_account_customization() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    browser.signup("osu", 10, 20).await;
    assert_eq!(
        browser
            .request(
                Method::POST,
                "/user/settings",
                Some(json!({"cooldown":12,"sr":[4,10],"sub-only":true})),
                Some("http://website.test")
            )
            .await
            .status,
        StatusCode::OK
    );
    let settings = browser.get("/user/settings").await.json;
    assert_eq!(settings.as_array().unwrap().len(), 7);
    assert_eq!(
        settings
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "cooldown")
            .unwrap()["value"],
        12.0
    );
    assert_eq!(
        settings
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "sr")
            .unwrap()["value"],
        json!([4.0, -1.0])
    );
    assert_eq!(
        browser
            .request(
                Method::POST,
                "/user/settings",
                Some(json!({"cooldown":-1})),
                Some("http://website.test")
            )
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let reply = browser
        .request(
            Method::POST,
            "/user/exclude?excluded_user=Bad_User",
            None,
            Some("http://website.test"),
        )
        .await;
    assert_eq!(reply.json, "bad_user");
    assert_eq!(browser.get("/user/exclude").await.json, json!(["bad_user"]));
    let old_token = browser.jar["token"].clone();
    assert_eq!(browser.oauth("osu", 10).await.status, StatusCode::SEE_OTHER);
    assert_ne!(browser.jar["token"], old_token);
    assert_eq!(browser.get("/user/exclude").await.json, json!(["bad_user"]));
    let user = fixture
        .state
        .store
        .user_by_twitch(20)
        .await
        .unwrap()
        .unwrap();
    assert!(user.settings.sub_only);
    assert_eq!(user.settings.cooldown, 12.0);
    assert_eq!(
        browser
            .request(
                Method::DELETE,
                "/user/exclude?excluded_user=BAD_USER",
                None,
                Some("http://website.test")
            )
            .await
            .json,
        "bad_user"
    );
    assert_eq!(browser.get("/user/exclude").await.json, json!([]));
}

#[tokio::test]
async fn linking_rejects_same_provider_and_existing_identity_takeovers() {
    let fixture = Fixture::new().await;
    let mut owner = fixture.browser();
    owner.signup("osu", 10, 20).await;
    let mut same = fixture.browser();
    assert_eq!(same.oauth("osu", 11).await.status, StatusCode::SEE_OTHER);
    assert_eq!(same.oauth("osu", 12).await.status, StatusCode::CONFLICT);
    assert_eq!(same.oauth("twitch", 20).await.status, StatusCode::CONFLICT);
    assert_eq!(
        fixture
            .state
            .store
            .user_by_twitch(20)
            .await
            .unwrap()
            .unwrap()
            .osu_id,
        10
    );
    assert!(fixture.state.store.user_by_osu(11).await.unwrap().is_none());
    assert_eq!(owner.oauth("osu", 11).await.status, StatusCode::CONFLICT);
    assert_eq!(owner.get("/user/me").await.json["osuId"], 10);
}

#[tokio::test]
async fn recycled_twitch_login_does_not_conflict_with_distinct_verified_ids() {
    let fixture = Fixture::new().await;
    let mut first = fixture.browser();
    first.signup("osu", 10, 20).await;
    // A stale cached login may now be assigned by Twitch to a different ID.
    fixture
        .state
        .store
        .call(|db| {
            db.execute(
                "UPDATE users SET twitch_username='streamer21' WHERE twitch_id=20",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut second = fixture.browser();
    second.signup("twitch", 11, 21).await;
    let old_owner = fixture
        .state
        .store
        .user_by_twitch(20)
        .await
        .unwrap()
        .unwrap();
    let new_owner = fixture
        .state
        .store
        .user_by_twitch(21)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old_owner.twitch_username, new_owner.twitch_username);
    assert_eq!(old_owner.osu_id, 10);
    assert_eq!(new_owner.osu_id, 11);
    let mut returning = fixture.browser();
    assert_eq!(
        returning.oauth("twitch", 21).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(returning.get("/user/me").await.json["osuId"], 11);
    assert_eq!(first.get("/user/me").await.json["osuId"], 10);
}

#[tokio::test]
async fn cookie_mutations_require_exact_origin_and_expired_deleted_sessions_are_unauthorized() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    browser.signup("twitch", 10, 20).await;
    for origin in [
        None,
        Some("null"),
        Some("http://website.test.evil.test"),
        Some("http://evil.test"),
    ] {
        assert_eq!(
            browser
                .request(Method::GET, "/user/logout", None, origin)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            browser
                .request(Method::DELETE, "/user/me", None, origin)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            browser
                .request(
                    Method::POST,
                    "/user/exclude?excluded_user=foo",
                    None,
                    origin
                )
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    fixture
        .state
        .store
        .call(|db| {
            db.execute("UPDATE sessions SET expires_at=0", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        browser.get("/user/me").await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(browser.oauth("osu", 10).await.status, StatusCode::SEE_OTHER);
    let copy_token = browser.jar["token"].clone();
    assert_eq!(
        browser
            .request(
                Method::DELETE,
                "/user/me",
                None,
                Some("http://website.test")
            )
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    browser.jar.insert("token".into(), copy_token);
    assert_eq!(
        browser.get("/user/me").await.status,
        StatusCode::UNAUTHORIZED
    );
    fixture
        .state
        .store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn logout_revokes_server_session_and_rejects_open_redirects() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    browser.signup("osu", 10, 20).await;
    let token = browser.jar["token"].clone();
    let response = browser
        .request(
            Method::GET,
            "/user/logout",
            None,
            Some("http://website.test"),
        )
        .await;
    assert_eq!(response.headers[header::LOCATION], "http://website.test");
    assert!(!browser.jar.contains_key("token"));
    browser.jar.insert("token".into(), token);
    assert_eq!(
        browser.get("/user/settings").await.status,
        StatusCode::UNAUTHORIZED
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("https://evil.test/steal"),
    );
    assert_eq!(
        security::return_to(&fixture.state.config, &headers),
        "http://website.test"
    );
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("http://website.test/account?updated=1"),
    );
    assert_eq!(
        security::return_to(&fixture.state.config, &headers),
        "http://website.test/account?updated=1"
    );
    assert!(security::require_origin(&fixture.state.config, &headers).is_ok());
    headers.insert(header::ORIGIN, HeaderValue::from_static("http://evil.test"));
    assert!(security::require_origin(&fixture.state.config, &headers).is_err());
}

#[tokio::test]
async fn leaderboards_use_one_seven_and_thirty_day_utc_windows() {
    let fixture = Fixture::new().await;
    let now = chrono::Utc::now().timestamp();
    for (id, age) in [(1, 0), (2, 3), (3, 14), (4, 40)] {
        fixture
            .state
            .store
            .cache_put(json!({"id":id,"beatmapset_id":100,"version":format!("Map{id}")}))
            .await
            .unwrap();
        fixture
            .state
            .store
            .record_request(RequestEvent {
                requester_channel_name: "fan".into(),
                requested_beatmap_id: id,
                requested_channel_name: "streamer".into(),
                mods: None,
                timestamp: now - age * 86400,
            })
            .await
            .unwrap();
    }
    let mut browser = fixture.browser();
    for (period, count) in [("daily", 1), ("weekly", 2), ("monthly", 3)] {
        let rows = browser
            .get(&format!("/requests/beatmaps/top/{period}"))
            .await
            .json;
        assert_eq!(rows.as_array().unwrap().len(), count);
        for row in rows.as_array().unwrap() {
            assert!(row["_id"].is_number());
            assert_eq!(row["count"], 1);
            assert!(row.get("result").is_none());
            assert!(row["version"].is_string());
        }
    }
}

#[tokio::test]
async fn secure_cookies_include_secure_attribute() {
    let fixture = Fixture::new().await;
    let mut config = (*fixture.state.config).clone();
    config.cookie_secure = true;
    let mut response = security::redirect("http://website.test");
    security::set_cookie(
        &mut response,
        &config,
        "token",
        &security::random_token(),
        300,
    );
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("; Secure"));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Lax"));
}

#[tokio::test]
async fn actual_http_app_completes_oauth_against_local_provider_servers() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(fixture.state.config.clone(), fixture.state.store.clone());
    struct RunningServer(tokio::task::JoinHandle<()>);
    impl Drop for RunningServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = RunningServer(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(
        client
            .get(format!("{base}/ready"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let mut pending_cookie = String::new();
    let mut session_cookie = String::new();
    for (provider, id) in [("osu", 30), ("twitch", 40)] {
        let start = client
            .get(format!("{base}/oauth2/{provider}-login"))
            .header(header::COOKIE, &pending_cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), StatusCode::SEE_OTHER);
        let authorization =
            url::Url::parse(start.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let nonce = authorization
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        let cookies = format!("oauth_state_{provider}={nonce}; {pending_cookie}");
        let reply = client
            .get(format!(
                "{base}/oauth2/{provider}-redirect?code={id}&state={nonce}"
            ))
            .header(header::COOKIE, cookies)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::SEE_OTHER);
        for cookie in reply.headers().get_all(header::SET_COOKIE) {
            let cookie = cookie.to_str().unwrap();
            if cookie.contains("Max-Age=0") {
                continue;
            }
            if cookie.starts_with("signup_details=") {
                pending_cookie = cookie.split(';').next().unwrap().to_owned();
            }
            if cookie.starts_with("token=") {
                session_cookie = cookie.split(';').next().unwrap().to_owned();
            }
        }
    }
    let user: Value = client
        .get(format!("{base}/user/me"))
        .header(header::COOKIE, session_cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(user["osuId"], 30);
    assert_eq!(user["twitchId"], 40);
}

#[tokio::test]
async fn concurrent_callback_consumption_has_only_one_winner() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    let nonce = browser.start("osu").await;
    let path = format!("/oauth2/osu-redirect?code=10&state={nonce}");
    let request = || {
        Request::builder()
            .uri(&path)
            .header(header::COOKIE, format!("oauth_state_osu={nonce}"))
            .body(Body::empty())
            .unwrap()
    };
    let (first, second) = tokio::join!(
        browser.app.clone().oneshot(request()),
        browser.app.clone().oneshot(request())
    );
    let statuses = [first.unwrap().status(), second.unwrap().status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::SEE_OTHER)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::BAD_REQUEST)
            .count(),
        1
    );
}

#[tokio::test]
async fn documentation_endpoints_are_not_exposed_when_debug_is_disabled() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    for path in ["/docs", "/redoc", "/openapi.json"] {
        assert_eq!(browser.get(path).await.status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn debug_documentation_describes_real_routes_payloads_and_cookie_security() {
    let fixture = Fixture::new().await;
    let mut config = (*fixture.state.config).clone();
    config.debug_mode = true;
    let mut browser = Browser {
        app: router(Arc::new(config), fixture.state.store.clone()),
        jar: BTreeMap::new(),
    };
    for path in ["/docs", "/redoc"] {
        let reply = browser.get(path).await;
        assert_eq!(reply.status, StatusCode::OK);
        assert!(
            reply.headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
    }
    let reply = browser.get("/openapi.json").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let spec = reply.json;
    for (name, schema) in spec["components"]["schemas"].as_object().unwrap() {
        serde_json::from_value::<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>(
            schema.clone(),
        )
        .unwrap_or_else(|error| panic!("Invalid OpenAPI schema {name}: {error}: {schema}"));
    }
    let _: utoipa::openapi::OpenApi = serde_json::from_value(spec.clone()).unwrap();
    assert!(spec["openapi"].as_str().unwrap().starts_with("3."));
    for (path, method) in [
        ("/oauth2/osu-login", "get"),
        ("/oauth2/osu-redirect", "get"),
        ("/oauth2/twitch-login", "get"),
        ("/oauth2/twitch-redirect", "get"),
        ("/user/me", "get"),
        ("/user/me", "delete"),
        ("/user/logout", "get"),
        ("/user/settings", "get"),
        ("/user/settings", "post"),
        ("/user/exclude", "get"),
        ("/user/exclude", "post"),
        ("/user/exclude", "delete"),
        ("/live/users", "get"),
        ("/requests/beatmaps/top/daily", "get"),
        ("/requests/beatmaps/top/weekly", "get"),
        ("/requests/beatmaps/top/monthly", "get"),
    ] {
        let operation = &spec["paths"][path][method];
        assert!(
            operation["responses"].is_object(),
            "Missing operation {method} {path}"
        );
    }
    fn resolve_references(value: &Value, root: &Value) {
        match value {
            Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    let pointer = reference.strip_prefix('#').expect("local schema reference");
                    assert!(
                        root.pointer(pointer).is_some(),
                        "Unresolved schema {reference}"
                    );
                }
                for value in object.values() {
                    resolve_references(value, root);
                }
            }
            Value::Array(array) => {
                for value in array {
                    resolve_references(value, root);
                }
            }
            _ => {}
        }
    }
    resolve_references(&spec, &spec);
    let schemes = &spec["components"]["securitySchemes"];
    assert_eq!(schemes["session"]["in"], "cookie");
    assert_eq!(schemes["session"]["name"], "token");
    assert_eq!(schemes["signup"]["name"], "signup_details");
    assert_eq!(schemes["osuState"]["name"], "oauth_state_osu");
    assert_eq!(schemes["twitchState"]["name"], "oauth_state_twitch");
    for (path, method) in [
        ("/user/me", "delete"),
        ("/user/settings", "get"),
        ("/user/settings", "post"),
        ("/user/exclude", "get"),
        ("/user/exclude", "post"),
        ("/user/exclude", "delete"),
    ] {
        assert!(
            spec["paths"][path][method]["security"]
                .as_array()
                .unwrap()
                .iter()
                .any(|requirement| requirement.get("session").is_some())
        );
    }
    let settings = &spec["components"]["schemas"]["Settings"]["properties"];
    assert!(settings.get("sub-only").is_some());
    assert!(settings.get("points-only").is_some());
    assert!(settings.get("sub_only").is_none());
    let setting_values = spec["components"]["schemas"]["SettingValue"]["oneOf"]
        .as_array()
        .unwrap();
    assert!(
        setting_values
            .iter()
            .any(|value| value["type"] == "boolean")
    );
    assert!(setting_values.iter().any(|value| value["type"] == "number"));
    let range = setting_values
        .iter()
        .find(|value| value["type"] == "array")
        .unwrap();
    assert_eq!(range["items"]["type"], "number");
    assert_eq!(range["minItems"], 2);
    assert_eq!(range["maxItems"], 2);
    let leaderboard = &spec["components"]["schemas"]["LeaderboardEntry"];
    assert_eq!(leaderboard["additionalProperties"], true);
    assert_eq!(leaderboard["properties"]["_id"]["type"], "integer");
    assert_eq!(leaderboard["properties"]["count"]["type"], "integer");
    for method in ["post", "delete"] {
        let operation = &spec["paths"]["/user/exclude"][method];
        assert!(operation.get("requestBody").is_none());
        assert!(operation["parameters"].as_array().unwrap().iter()
            .any(|parameter| parameter["name"] == "excluded_user" && parameter["in"] == "query"));
    }
    let parameters = spec["paths"]["/live/users"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let limit = parameters
        .iter()
        .find(|parameter| parameter["name"] == "limit")
        .unwrap();
    assert_eq!(limit["schema"]["default"], 5);
    assert_eq!(limit["schema"]["minimum"].as_f64(), Some(1.0));
    assert_eq!(limit["schema"]["maximum"].as_f64(), Some(100.0));
    browser.signup("osu", 50, 60).await;
    let actual = browser.get("/user/me").await.json;
    let properties = spec["components"]["schemas"]["UserResponse"]["properties"]
        .as_object()
        .unwrap();
    assert_eq!(properties.len(), 9);
    assert_eq!(properties["twitchBotAuthorized"]["type"], "boolean");
    assert!(properties.get("settings").is_none());
    for field in actual.as_object().unwrap().keys() {
        assert!(
            properties.contains_key(field),
            "Undocumented user field {field}"
        );
    }
    let cors = browser
        .request(Method::GET, "/openapi.json", None, Some("http://evil.test"))
        .await;
    assert!(
        cors.headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none_or(|origin| origin != "http://evil.test" && origin != "*")
    );
}

#[tokio::test]
async fn migration_status_is_public_with_exact_camel_case() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    let reply = browser.get("/migration/status").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json, json!({"active":false,"cutoffAt":null}));

    let fixture = Fixture::with_migration(true, Some("2026-12-01T00:00:00Z")).await;
    let mut browser = fixture.browser();
    assert_eq!(
        browser.get("/migration/status").await.json,
        json!({"active":true,"cutoffAt":"2026-12-01T00:00:00Z"})
    );
    assert_eq!(browser.get("/health").await.status, StatusCode::OK);
}

#[tokio::test]
async fn internal_routes_are_hidden_without_a_configured_or_supplied_token() {
    let fixture = Fixture::new().await;
    let mut browser = fixture.browser();
    for (method, path, payload) in [
        (Method::GET, "/internal/legacy/channels", None),
        (
            Method::POST,
            "/internal/legacy/live",
            Some(json!({"channels":[]})),
        ),
        (
            Method::POST,
            "/internal/legacy/claim",
            Some(json!({"twitchMessageId":"m","twitchId":20,"targetOsuId":10,"message":"pm"})),
        ),
    ] {
        assert_eq!(
            browser
                .internal(method, path, payload, Some("any-token"))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }

    let fixture = Fixture::with_internal("internal-secret").await;
    let mut browser = fixture.browser();
    assert_eq!(
        browser
            .internal(Method::GET, "/internal/legacy/channels", None, None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    for token in ["wrong", "internal-secret "] {
        assert_eq!(
            browser
                .internal(Method::GET, "/internal/legacy/channels", None, Some(token))
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }
    let malformed = Request::builder()
        .uri("/internal/legacy/channels")
        .header(header::AUTHORIZATION, "Basic internal-secret")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        browser
            .app
            .clone()
            .oneshot(malformed)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        browser
            .internal(
                Method::GET,
                "/internal/legacy/channels",
                None,
                Some("internal-secret")
            )
            .await
            .json,
        json!([])
    );
}

#[tokio::test]
async fn legacy_channel_endpoints_only_expose_never_granted_enabled_accounts() {
    let fixture = Fixture::with_internal("internal-secret").await;
    for (osu, twitch, login) in [(10, 20, "alpha"), (11, 21, "beta"), (12, 22, "disabled")] {
        fixture
            .state
            .store
            .save_user(User {
                osu_id: osu,
                osu_username: format!("player{osu}"),
                osu_avatar_url: String::new(),
                twitch_id: twitch,
                twitch_username: login.into(),
                twitch_avatar_url: String::new(),
                excluded_users: vec!["viewer".into()],
                is_live: false,
                settings: crate::domain::Settings::default(),
            })
            .await
            .unwrap();
    }
    fixture
        .state
        .store
        .set_settings(
            22,
            crate::domain::Settings {
                enable: false,
                ..crate::domain::Settings::default()
            },
        )
        .await
        .unwrap();
    fixture
        .state
        .store
        .call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(20,0,0,'revoked')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut browser = fixture.browser();
    let token = Some("internal-secret");
    let channels = browser
        .internal(
            Method::GET,
            "/internal/legacy/channels?after=0&limit=100",
            None,
            token,
        )
        .await;
    assert_eq!(channels.status, StatusCode::OK);
    assert_eq!(channels.json.as_array().unwrap().len(), 1);
    let user = &channels.json[0];
    assert_eq!(user["osuId"], 11);
    assert_eq!(user["osuUsername"], "player11");
    assert_eq!(user["twitchId"], 21);
    assert_eq!(user["twitchUsername"], "beta");
    assert_eq!(user["excludedUsers"], json!(["viewer"]));
    assert_eq!(user["isLive"], false);
    assert_eq!(user["settings"]["sub-only"], false);
    assert_eq!(user["settings"]["points-only"], false);
    assert_eq!(user["settings"]["sr"], json!([0.0, -1.0]));
    assert_eq!(
        browser
            .internal(
                Method::GET,
                "/internal/legacy/channels?after=21",
                None,
                token
            )
            .await
            .json,
        json!([])
    );
    assert_eq!(
        browser
            .internal(
                Method::GET,
                "/internal/legacy/channels?limit=0",
                None,
                token
            )
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        browser
            .internal(
                Method::GET,
                "/internal/legacy/channels/by-login/BETA",
                None,
                token
            )
            .await
            .json["twitchId"],
        21
    );
    assert_eq!(
        browser
            .internal(
                Method::GET,
                "/internal/legacy/channels/by-login/alpha",
                None,
                token
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser
            .internal(Method::GET, "/internal/legacy/channels/21", None, token)
            .await
            .json["osuId"],
        11
    );
    for path in [
        "/internal/legacy/channels/20",
        "/internal/legacy/channels/22",
        "/internal/legacy/channels/999",
    ] {
        assert_eq!(
            browser
                .internal(Method::GET, path, None, token)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn legacy_live_endpoint_replaces_presence_and_unions_public_live() {
    let fixture = Fixture::with_internal("internal-secret").await;
    for (osu, twitch, login) in [
        (10, 20, "alpha"),
        (11, 21, "beta"),
        (12, 22, "disabled"),
        (13, 23, "new-live"),
    ] {
        fixture
            .state
            .store
            .save_user(User {
                osu_id: osu,
                osu_username: format!("player{osu}"),
                osu_avatar_url: String::new(),
                twitch_id: twitch,
                twitch_username: login.into(),
                twitch_avatar_url: String::new(),
                excluded_users: Vec::new(),
                is_live: false,
                settings: crate::domain::Settings::default(),
            })
            .await
            .unwrap();
    }
    fixture
        .state
        .store
        .set_settings(
            22,
            crate::domain::Settings {
                enable: false,
                ..crate::domain::Settings::default()
            },
        )
        .await
        .unwrap();
    fixture
        .state
        .store
        .call(|db| {
            db.execute("UPDATE users SET is_live=1 WHERE twitch_id=23", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let mut changes = fixture.state.store.channel_changes();
    changes.borrow_and_update();
    let mut browser = fixture.browser();
    let token = Some("internal-secret");
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/live",
                Some(json!({"channels":[]})),
                None
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/live",
                Some(json!({"channels":[]})),
                Some("wrong")
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let reply = browser
        .internal(
            Method::POST,
            "/internal/legacy/live",
            Some(json!({"channels":[
                {"twitchId":20,"login":"Alpha_New"},
                {"twitchId":21,"login":"BETA"},
                {"twitchId":22,"login":"disabled"},
                {"twitchId":999,"login":"ghost"}
            ]})),
            token,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json, json!({"updated":2}));
    assert!(
        !changes.has_changed().unwrap(),
        "legacy presence must not wake the new bot"
    );
    assert_eq!(
        fixture
            .state
            .store
            .user_by_twitch(20)
            .await
            .unwrap()
            .unwrap()
            .twitch_username,
        "alpha_new"
    );
    assert_eq!(
        browser.get("/live/users?limit=100").await.json,
        json!(["alpha_new", "beta", "new-live"])
    );
    assert_eq!(
        browser.get("/live/users?limit=2&offset=1").await.json,
        json!(["beta", "new-live"])
    );
    for channels in [
        json!([{"twitchId":20,"login":"alpha_new"},{"twitchId":20,"login":"again"}]),
        json!([{"twitchId":20,"login":" "}]),
        json!([{"twitchId":0,"login":"alpha"}]),
    ] {
        assert_eq!(
            browser
                .internal(
                    Method::POST,
                    "/internal/legacy/live",
                    Some(json!({"channels":channels})),
                    token
                )
                .await
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let too_many = (1..=10_001)
        .map(|id| json!({"twitchId":id,"login":format!("login{id}")}))
        .collect::<Vec<_>>();
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/live",
                Some(json!({"channels":too_many})),
                token
            )
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // A grant (or reauthorization) permanently removes the legacy presence row.
    assert_eq!(
        browser.oauth("twitch", 20).await.status,
        StatusCode::SEE_OTHER
    );
    let (grants, legacy): (i64, i64) = fixture
        .state
        .store
        .call(|db| {
            Ok((
                db.query_row(
                    "SELECT COUNT(*) FROM twitch_channel_grants WHERE twitch_id=20 AND status='granted'",
                    [],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT COUNT(*) FROM legacy_live WHERE twitch_id=20",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((grants, legacy), (1, 0));
    assert_eq!(
        browser.get("/live/users?limit=100").await.json,
        json!(["beta", "new-live"])
    );
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/live",
                Some(json!({"channels":[
                    {"twitchId":20,"login":"alpha_new"},
                    {"twitchId":21,"login":"beta"}
                ]})),
                token
            )
            .await
            .json,
        json!({"updated":1})
    );
}

#[tokio::test]
async fn legacy_claim_and_report_endpoints_are_exact_and_idempotent() {
    let fixture = Fixture::with_internal("internal-secret").await;
    fixture
        .state
        .store
        .save_user(User {
            osu_id: 10,
            osu_username: "player10".into(),
            osu_avatar_url: String::new(),
            twitch_id: 20,
            twitch_username: "alpha".into(),
            twitch_avatar_url: String::new(),
            excluded_users: Vec::new(),
            is_live: false,
            settings: crate::domain::Settings::default(),
        })
        .await
        .unwrap();
    let mut browser = fixture.browser();
    let token = Some("internal-secret");
    let claim = |message: &str, osu_id: i64| {
        json!({
            "twitchMessageId": message,
            "twitchId": 20,
            "targetOsuId": osu_id,
            "message": "!request 123"
        })
    };
    let reply = browser
        .internal(
            Method::POST,
            "/internal/legacy/claim",
            Some(claim("m1", 10)),
            token,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json, json!({"claimed":true}));
    let duplicate = browser
        .internal(
            Method::POST,
            "/internal/legacy/claim",
            Some(claim("m1", 10)),
            token,
        )
        .await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.json["claimed"], false);
    assert!(duplicate.json["reason"].is_string());
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/claim",
                Some(claim("m2", 999)),
                token
            )
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/claim",
                Some(json!({"twitchMessageId":"bad","twitchId":20,"targetOsuId":10,"message":""})),
                token
            )
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let statistics = || {
        json!({
            "requesterChannelName": "viewer",
            "requestedBeatmapId": 42,
            "requestedChannelName": "alpha",
            "mods": null,
            "timestamp": 1_700_000_000
        })
    };
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/deliveries/m1",
                Some(json!({"sent":true,"error":null,"statistics":null})),
                token
            )
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for _ in 0..2 {
        let reply = browser
            .internal(
                Method::POST,
                "/internal/legacy/deliveries/m1",
                Some(json!({"sent":true,"error":null,"statistics":statistics()})),
                token,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json, json!({"ok":true}));
    }
    let (status, source, stats): (String, String, i64) = fixture
        .state
        .store
        .call(|db| {
            Ok((
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='m1'",
                    [],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT source FROM deliveries WHERE twitch_message_id='m1'",
                    [],
                    |r| r.get(0),
                )?,
                db.query_row(
                    "SELECT COUNT(*) FROM statistics WHERE source_id='twitch:m1'",
                    [],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        (status.as_str(), source.as_str(), stats),
        ("sent", "legacy", 1)
    );

    fixture
        .state
        .store
        .call(|db| {
            db.execute(
                "INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-message',10,'pm','pending',0,'new')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let overlap = browser
        .internal(
            Method::POST,
            "/internal/legacy/claim",
            Some(claim("new-message", 10)),
            token,
        )
        .await;
    assert_eq!(overlap.status, StatusCode::CONFLICT);
    assert_eq!(overlap.json["claimed"], false);

    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/deliveries/missing",
                Some(json!({"sent":false,"error":"x","statistics":null})),
                token
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        browser
            .internal(
                Method::POST,
                "/internal/legacy/deliveries/new-message",
                Some(json!({"sent":false,"error":"x","statistics":null})),
                token
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    fixture
        .state
        .store
        .call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(20,0,0,'granted')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let migrated = browser
        .internal(
            Method::POST,
            "/internal/legacy/claim",
            Some(claim("m3", 10)),
            token,
        )
        .await;
    assert_eq!(migrated.status, StatusCode::CONFLICT);
    assert_eq!(migrated.json["claimed"], false);
}

#[tokio::test]
async fn new_grants_stay_private_to_the_new_bot_during_the_grace_window() {
    let fixture = Fixture::with_grace(3600).await;
    let mut changes = fixture.state.store.channel_changes();
    changes.borrow_and_update();
    let mut browser = fixture.browser();
    browser.signup("osu", 10, 20).await;
    // Public consent and the browser-facing profile are available immediately.
    assert_eq!(
        browser.get("/user/me").await.json["twitchBotAuthorized"],
        true
    );
    let (active_at, issued_at): (i64, i64) = fixture
        .state
        .store
        .call(|db| {
            Ok((
                db.query_row(
                    "SELECT active_at FROM twitch_channel_grants WHERE twitch_id=20",
                    [],
                    |r| r.get(0),
                )?,
                db.query_row("SELECT expires_at-86400 FROM sessions", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        active_at - issued_at,
        3600,
        "grant uses the configured grace"
    );
    assert!(
        fixture
            .state
            .store
            .granted_twitch_ids()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!changes.has_changed().unwrap());
    // SQLite uses wall time, not Tokio's test clock. Expire the stored deadline
    // explicitly, then advance the notification timer without a real-time race.
    fixture
        .state
        .store
        .call(|db| {
            db.execute("UPDATE twitch_channel_grants SET active_at=unixepoch()", [])?;
            Ok(())
        })
        .await
        .unwrap();
    tokio::time::pause();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(3600)).await;
    changes.changed().await.unwrap();
    assert_eq!(
        fixture.state.store.granted_twitch_ids().await.unwrap(),
        vec![20]
    );
}
