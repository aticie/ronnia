use super::{
    BOT_SCOPES, DEVICE_GRANT_TYPE, SCOPE_STRING, TwitchAuth, UNUSABLE,
    crypto::{self, Credentials},
};
use crate::{config::Config, storage::Store};
use axum::{
    Json, Router,
    extract::{Form, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use rusqlite::params;
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex as StdMutex},
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

const KEY: [u8; 32] = [42; 32];
const USER: &str = "999";
const LOGIN: &str = "smokebot";
const TEST_CLIENT: &str = "test-client";

#[derive(Clone, Copy, Debug)]
enum RefreshMode {
    Rotate,
    Invalid,
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
enum ValidateMode {
    Ok,
    Unauthorized,
    Unavailable,
    WrongClient,
    WrongUser,
    WrongLogin,
    MissingScope,
}

#[derive(Default)]
struct Mock {
    device_calls: usize,
    token_calls: usize,
    refresh_calls: usize,
    validate_calls: usize,
    device_polls: VecDeque<&'static str>,
    device_always_pending: bool,
    device_expires_in: i64,
    refresh_script: VecDeque<RefreshMode>,
    validate_script: VecDeque<ValidateMode>,
    refresh_http_pause: Option<(CancellationToken, CancellationToken)>,
    user_id: String,
    login: String,
}

impl Mock {
    fn new() -> Self {
        Self {
            device_expires_in: 60,
            user_id: USER.into(),
            login: LOGIN.into(),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
struct MockHandle(Arc<StdMutex<Mock>>);

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    device: usize,
    token: usize,
    refresh: usize,
    validate: usize,
}

async fn mock_device(
    State(handle): State<MockHandle>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let mut mock = handle.0.lock().unwrap();
    mock.device_calls += 1;
    assert_eq!(form.get("client_id").map(String::as_str), Some(TEST_CLIENT));
    assert_eq!(form.get("scopes").map(String::as_str), Some(SCOPE_STRING));
    Json(json!({
        "device_code": "device-code",
        "expires_in": mock.device_expires_in,
        "interval": 1,
        "user_code": "ABCDEFGH",
        "verification_uri": "https://www.twitch.tv/activate?public=true&device-code=ABCDEFGH",
    }))
    .into_response()
}

async fn mock_token(
    State(handle): State<MockHandle>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let (response, pause) = {
        let mut mock = handle.0.lock().unwrap();
        let pause = if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
            mock.refresh_http_pause.take()
        } else {
            None
        };
        (mock_token_response(&mut mock, form), pause)
    };
    if let Some((issued, respond)) = pause {
        issued.cancel();
        respond.cancelled().await;
    }
    response
}

fn mock_token_response(mock: &mut Mock, form: BTreeMap<String, String>) -> Response {
    mock.token_calls += 1;
    match form.get("grant_type").map(String::as_str) {
        Some(DEVICE_GRANT_TYPE) => {
            assert_eq!(
                form.get("device_code").map(String::as_str),
                Some("device-code")
            );
            let message = mock.device_polls.pop_front();
            if mock.device_always_pending || message.is_some() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "status": 400,
                        "message": message.unwrap_or("authorization_pending"),
                    })),
                )
                    .into_response();
            }
            Json(json!({
                "access_token": "device-access",
                "refresh_token": "device-refresh",
                "expires_in": 14400,
                "scope": BOT_SCOPES,
                "token_type": "bearer",
            }))
            .into_response()
        }
        Some("refresh_token") => {
            mock.refresh_calls += 1;
            assert_eq!(form.get("client_id").map(String::as_str), Some(TEST_CLIENT));
            assert_eq!(
                form.get("client_secret").map(String::as_str),
                Some("test-secret")
            );
            match mock
                .refresh_script
                .pop_front()
                .unwrap_or(RefreshMode::Rotate)
            {
                RefreshMode::Rotate => Json(json!({
                    "access_token": format!("rotated-access-{}", mock.refresh_calls),
                    "refresh_token": format!("rotated-refresh-{}", mock.refresh_calls),
                    "expires_in": 14400,
                    "token_type": "bearer",
                }))
                .into_response(),
                RefreshMode::Invalid => (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"status": 400, "message": "Invalid refresh token"})),
                )
                    .into_response(),
                RefreshMode::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}

async fn mock_validate(State(handle): State<MockHandle>, headers: HeaderMap) -> Response {
    let mut mock = handle.0.lock().unwrap();
    mock.validate_calls += 1;
    let token = headers
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("OAuth "))
        .unwrap_or_default();
    assert!(!token.is_empty());
    match mock.validate_script.pop_front().unwrap_or(ValidateMode::Ok) {
        ValidateMode::Ok => Json(json!({
            "client_id": TEST_CLIENT,
            "login": mock.login.as_str(),
            "scopes": BOT_SCOPES,
            "user_id": mock.user_id.as_str(),
            "expires_in": 14400,
        }))
        .into_response(),
        ValidateMode::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status": 401, "message": "invalid access token"})),
        )
            .into_response(),
        ValidateMode::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        ValidateMode::WrongClient => Json(json!({
            "client_id": "other-client",
            "login": mock.login.as_str(),
            "scopes": BOT_SCOPES,
            "user_id": mock.user_id.as_str(),
            "expires_in": 14400,
        }))
        .into_response(),
        ValidateMode::WrongUser => Json(json!({
            "client_id": TEST_CLIENT,
            "login": mock.login.as_str(),
            "scopes": BOT_SCOPES,
            "user_id": "111",
            "expires_in": 14400,
        }))
        .into_response(),
        ValidateMode::WrongLogin => Json(json!({
            "client_id": TEST_CLIENT,
            "login": "otherbot",
            "scopes": BOT_SCOPES,
            "user_id": mock.user_id.as_str(),
            "expires_in": 14400,
        }))
        .into_response(),
        ValidateMode::MissingScope => Json(json!({
            "client_id": TEST_CLIENT,
            "login": mock.login.as_str(),
            "scopes": ["user:read:chat"],
            "user_id": mock.user_id.as_str(),
            "expires_in": 14400,
        }))
        .into_response(),
    }
}

struct Fixture {
    _dir: TempDir,
    config: Arc<Config>,
    store: Store,
    mock: MockHandle,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new() -> Self {
        let mock = MockHandle(Arc::new(StdMutex::new(Mock::new())));
        let app = Router::new()
            .route("/oauth2/device", post(mock_device))
            .route("/oauth2/token", post(mock_token))
            .route("/oauth2/validate", get(mock_validate))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = Arc::new(Config {
            database_path: ":memory:".into(),
            bind: "127.0.0.1".parse().unwrap(),
            bind_port: 0,
            public_origin: "http://website.test".into(),
            api_origin: "http://api.test".into(),
            cookie_secure: false,
            debug_mode: false,
            twitch_client_id: TEST_CLIENT.into(),
            twitch_client_secret: "test-secret".into(),
            twitch_redirect_uri: "http://api.test/oauth2/twitch-redirect".into(),
            osu_client_id: "test-client".into(),
            osu_client_secret: "test-secret".into(),
            osu_redirect_uri: "http://api.test/oauth2/osu-redirect".into(),
            twitch_eventsub_url: "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30"
                .into(),
            twitch_token_key: Some(KEY),
            twitch_api_base: format!("{base}/helix"),
            twitch_oauth_base: format!("{base}/oauth2"),
            osu_api_base: format!("{base}/osu"),
            osu_oauth_base: format!("{base}/osu-oauth"),
            dry_run: true,
            reconcile_interval_secs: 600,
            eventsub_channels_per_shard: 100,
            eventsub_max_shards: 16,
            request_workers: 8,
            request_queue_capacity: 128,
            http_timeout_secs: 5,
            session_ttl_secs: 86400,
            oauth_ttl_secs: 600,
            bot_enabled: false,
            migration_active: false,
            migration_cutoff_at: None,
            migration_grant_grace_secs: 0,
            internal_api_token: None,
        });
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("auth.sqlite3")).await.unwrap();
        Self {
            _dir: dir,
            config,
            store,
            mock,
            server,
        }
    }

    fn auth(&self) -> TwitchAuth {
        TwitchAuth::new(self.config.clone(), self.store.clone()).unwrap()
    }

    fn auth_with_key(&self, key: [u8; 32]) -> TwitchAuth {
        let mut config = (*self.config).clone();
        config.twitch_token_key = Some(key);
        TwitchAuth::new(Arc::new(config), self.store.clone()).unwrap()
    }

    fn queue_device_polls(&self, messages: &[&'static str]) {
        self.mock
            .0
            .lock()
            .unwrap()
            .device_polls
            .extend(messages.iter().copied());
    }

    fn queue_validate(&self, modes: &[ValidateMode]) {
        self.mock
            .0
            .lock()
            .unwrap()
            .validate_script
            .extend(modes.iter().copied());
    }

    fn queue_refresh(&self, modes: &[RefreshMode]) {
        self.mock
            .0
            .lock()
            .unwrap()
            .refresh_script
            .extend(modes.iter().copied());
    }

    fn set_device_pending(&self, pending: bool, expires_in: i64) {
        let mut mock = self.mock.0.lock().unwrap();
        mock.device_always_pending = pending;
        mock.device_expires_in = expires_in;
    }

    fn counts(&self) -> Counts {
        let mock = self.mock.0.lock().unwrap();
        Counts {
            device: mock.device_calls,
            token: mock.token_calls,
            refresh: mock.refresh_calls,
            validate: mock.validate_calls,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed(
        &self,
        key: [u8; 32],
        user_id: &str,
        login: &str,
        access: &str,
        refresh: &str,
        expires_at: i64,
        validated_at: i64,
    ) {
        let ciphertext = crypto::encrypt(
            &key,
            user_id,
            &Credentials {
                access_token: access.into(),
                refresh_token: refresh.into(),
            },
        )
        .unwrap();
        let scopes = serde_json::to_string(&BOT_SCOPES).unwrap();
        let (user_id, login) = (user_id.to_owned(), login.to_owned());
        self.store
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO twitch_bot_auth(singleton,user_id,login,ciphertext,expires_at,scopes_json,validated_at,revision) \
                     VALUES(1,?1,?2,?3,?4,?5,?6,1) \
                     ON CONFLICT(singleton) DO UPDATE SET user_id=excluded.user_id,login=excluded.login, \
                       ciphertext=excluded.ciphertext,expires_at=excluded.expires_at, \
                       scopes_json=excluded.scopes_json,validated_at=excluded.validated_at,revision=1",
                    params![user_id, login, ciphertext, expires_at, scopes, validated_at],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn device_grant_waits_for_pending_and_persists_only_ciphertext() {
    let fixture = Fixture::new().await;
    fixture.queue_device_polls(&["authorization_pending"]);
    let identity = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        "SmokeBot",
        false,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(identity.user_id, USER);
    assert_eq!(identity.login, LOGIN);
    let counts = fixture.counts();
    assert_eq!(counts.device, 1);
    assert_eq!(counts.token, 2);
    assert_eq!(counts.refresh, 0);
    assert_eq!(counts.validate, 1);

    let auth = fixture.auth();
    let grant = auth.load().await.unwrap().unwrap();
    assert_eq!(grant.user_id, USER);
    assert_eq!(grant.login, LOGIN);
    assert!(grant.validated_at > 0);
    assert!(grant.expires_at > Utc::now().timestamp());
    let scopes: Vec<String> = fixture
        .store
        .call(|connection| {
            let json: String = connection.query_row(
                "SELECT scopes_json FROM twitch_bot_auth WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str(&json)?)
        })
        .await
        .unwrap();
    assert_eq!(
        scopes,
        BOT_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>()
    );
    for secret in [b"device-access".as_slice(), b"device-refresh".as_slice()] {
        assert!(
            !grant
                .ciphertext
                .windows(secret.len())
                .any(|window| window == secret),
            "plaintext token leaked into the database"
        );
    }
    let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
    assert_eq!(credentials.access_token, "device-access");
    assert_eq!(credentials.refresh_token, "device-refresh");
}

#[tokio::test]
async fn device_grant_rejects_a_browser_account_that_does_not_match() {
    let fixture = Fixture::new().await;
    let error = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        "differentbot",
        false,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("different login"));
    assert!(fixture.auth().load().await.unwrap().is_none());
}

#[tokio::test]
async fn device_grant_requires_replace_for_a_different_existing_identity() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            "111",
            "oldbot",
            "old-access",
            "old-refresh",
            now + 3600,
            now,
        )
        .await;
    let error = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        LOGIN,
        false,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("--replace"));
    let unchanged = fixture.auth().load().await.unwrap().unwrap();
    assert_eq!(unchanged.user_id, "111");
    assert_eq!(unchanged.login, "oldbot");

    TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        LOGIN,
        true,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let replaced = fixture.auth().load().await.unwrap().unwrap();
    assert_eq!(replaced.user_id, USER);
    assert_eq!(replaced.login, LOGIN);
}

#[tokio::test]
async fn device_grant_honours_cancellation_and_expiry() {
    let fixture = Fixture::new().await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        LOGIN,
        false,
        cancelled,
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("cancelled"));
    assert_eq!(fixture.counts().token, 0);
    assert!(fixture.auth().load().await.unwrap().is_none());

    fixture.set_device_pending(true, 1);
    let error = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        LOGIN,
        false,
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("expired"));
    assert!(fixture.auth().load().await.unwrap().is_none());
}

#[tokio::test]
async fn device_grant_applies_slow_down_to_polling() {
    let fixture = Fixture::new().await;
    fixture.queue_device_polls(&["slow_down"]);
    let shutdown = CancellationToken::new();
    let cancel = shutdown.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        cancel.cancel();
    });
    let error = TwitchAuth::authorize_bot(
        fixture.config.clone(),
        fixture.store.clone(),
        LOGIN,
        false,
        shutdown,
    )
    .await
    .unwrap_err();
    canceller.await.unwrap();
    assert!(format!("{error:#}").contains("cancelled"));
    assert_eq!(fixture.counts().token, 1);
    assert!(fixture.auth().load().await.unwrap().is_none());
}

#[tokio::test]
async fn expiring_grant_rotates_both_tokens_and_validates_once() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now + 10, now)
        .await;
    let auth = fixture.auth();
    let identity = auth
        .ready_bot(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity.user_id, USER);
    assert_eq!(identity.login, LOGIN);
    let counts = fixture.counts();
    assert_eq!(counts.refresh, 1);
    assert_eq!(counts.validate, 1);
    let grant = auth.load().await.unwrap().unwrap();
    assert!(grant.validated_at > 0);
    assert!(grant.expires_at > now + 3600);
    let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
    assert_eq!(credentials.access_token, "rotated-access-1");
    assert_eq!(credentials.refresh_token, "rotated-refresh-1");
}

#[tokio::test]
async fn concurrent_readiness_shares_one_refresh() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now - 1, now)
        .await;
    let auth = Arc::new(fixture.auth());
    let shutdown = CancellationToken::new();
    let (a, b, c, d) = tokio::join!(
        auth.ready_bot(&shutdown),
        auth.ready_bot(&shutdown),
        auth.ready_bot(&shutdown),
        auth.ready_bot(&shutdown),
    );
    for result in [a, b, c, d] {
        assert_eq!(result.unwrap().unwrap().login, LOGIN);
    }
    let counts = fixture.counts();
    assert_eq!(counts.refresh, 1, "refresh must be single-flight");
    assert_eq!(counts.validate, 1, "cached validation must avoid HTTP");
    let credentials =
        crypto::decrypt(&KEY, USER, &auth.load().await.unwrap().unwrap().ciphertext).unwrap();
    assert_eq!(credentials.access_token, "rotated-access-1");
}

#[tokio::test]
async fn revoked_grant_is_marked_unusable_and_never_hot_loops() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "revoked-access",
            "revoked-refresh",
            now + 3600,
            now - 3700,
        )
        .await;
    fixture.queue_validate(&[ValidateMode::Unauthorized]);
    fixture.queue_refresh(&[RefreshMode::Invalid]);
    let auth = fixture.auth();
    let before = auth.load().await.unwrap().unwrap().ciphertext;
    let error = auth.ready_bot(&CancellationToken::new()).await.unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("refresh token"));
    assert!(!text.contains("revoked-access"));
    assert!(!text.contains("revoked-refresh"));
    let grant = auth.load().await.unwrap().unwrap();
    assert_eq!(grant.validated_at, UNUSABLE);
    assert_eq!(
        grant.ciphertext, before,
        "revocation must preserve evidence"
    );
    let counts = fixture.counts();
    assert_eq!(counts.refresh, 1);
    assert_eq!(counts.validate, 1);

    assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
    assert_eq!(fixture.counts().refresh, 1);
    assert_eq!(fixture.counts().validate, 1);
}

#[tokio::test]
async fn transient_failures_do_not_revoke_or_rewrite_credentials() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "stable-access",
            "stable-refresh",
            now + 3600,
            now - 3700,
        )
        .await;
    fixture.queue_validate(&[ValidateMode::Unavailable]);
    let auth = fixture.auth();
    let before = auth.load().await.unwrap().unwrap().ciphertext;
    assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
    let grant = auth.load().await.unwrap().unwrap();
    assert_eq!(grant.validated_at, now - 3700);
    assert_eq!(grant.ciphertext, before);
    assert_eq!(fixture.counts().refresh, 0);

    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "stable-access",
            "stable-refresh",
            now + 10,
            now,
        )
        .await;
    fixture.queue_refresh(&[RefreshMode::Unavailable]);
    let before = auth.load().await.unwrap().unwrap().ciphertext;
    assert!(auth.ready_bot(&CancellationToken::new()).await.is_err());
    let grant = auth.load().await.unwrap().unwrap();
    assert_eq!(grant.validated_at, now);
    assert_eq!(grant.ciphertext, before);
    assert_eq!(fixture.counts().validate, 1);
}

#[tokio::test]
async fn wrong_key_and_tampered_ciphertext_fail_closed() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "secure-access",
            "secure-refresh",
            now + 3600,
            now - 3700,
        )
        .await;
    let before = fixture.auth().load().await.unwrap().unwrap().ciphertext;
    let error = fixture
        .auth_with_key([9; 32])
        .ready_bot(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("TWITCH_TOKEN_KEY"));
    let unchanged = fixture.auth().load().await.unwrap().unwrap();
    assert_eq!(unchanged.ciphertext, before);
    assert_eq!(unchanged.validated_at, now - 3700);

    fixture
        .store
        .call(|connection| {
            let mut ciphertext: Vec<u8> = connection.query_row(
                "SELECT ciphertext FROM twitch_bot_auth WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            ciphertext[0] ^= 0xff;
            connection.execute(
                "UPDATE twitch_bot_auth SET ciphertext=?1 WHERE singleton=1",
                [ciphertext],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let error = fixture
        .auth()
        .ready_bot(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("decrypt"));
    let tampered = fixture.auth().load().await.unwrap().unwrap();
    assert_ne!(tampered.validated_at, UNUSABLE);
    assert_eq!(fixture.counts().refresh + fixture.counts().validate, 0);
}

#[tokio::test]
async fn validation_identity_client_and_scope_mismatches_fail_closed() {
    let fixture = Fixture::new().await;
    let auth = fixture.auth();
    let cases = [
        ValidateMode::WrongClient,
        ValidateMode::WrongUser,
        ValidateMode::WrongLogin,
        ValidateMode::MissingScope,
    ];
    for mode in cases {
        let now = Utc::now().timestamp();
        fixture
            .seed(
                KEY,
                USER,
                LOGIN,
                "mismatch-access",
                "mismatch-refresh",
                now + 3600,
                now - 3700,
            )
            .await;
        fixture.queue_validate(&[mode]);
        let error = auth.ready_bot(&CancellationToken::new()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("Twitch bot"),
            "unexpected error for {mode:?}: {error:#}"
        );
        let grant = auth.load().await.unwrap().unwrap();
        assert_eq!(grant.validated_at, UNUSABLE, "{mode:?} must fail closed");
        assert!(grant.ciphertext.len() > 12);
    }
}

#[tokio::test]
async fn bot_token_access_refreshes_expiring_tokens_and_reacts_to_rejection() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "expired-access",
            "expired-refresh",
            now - 1,
            now,
        )
        .await;
    let auth = fixture.auth();
    let shutdown = CancellationToken::new();
    assert_eq!(
        auth.bot_access_token(&shutdown).await.unwrap(),
        "rotated-access-1"
    );
    assert_eq!(
        auth.bot_access_token(&shutdown).await.unwrap(),
        "rotated-access-1"
    );
    assert_eq!(fixture.counts().refresh, 1);
    assert_eq!(fixture.counts().validate, 0);

    let rejected = auth.bot_access_token(&shutdown).await.unwrap();
    assert_eq!(
        auth.bot_access_token_after_rejection(&rejected, &shutdown)
            .await
            .unwrap(),
        "rotated-access-2"
    );
    assert_eq!(
        auth.bot_access_token_after_rejection(&rejected, &shutdown)
            .await
            .unwrap(),
        "rotated-access-2",
        "a concurrent rotation must not trigger a second refresh"
    );
    assert_eq!(fixture.counts().refresh, 2);
}

#[tokio::test]
async fn rejected_access_token_refreshes_only_after_a_401() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(
            KEY,
            USER,
            LOGIN,
            "rejected-access",
            "live-refresh",
            now + 3600,
            now - 3700,
        )
        .await;
    fixture.queue_validate(&[ValidateMode::Unauthorized]);
    let auth = fixture.auth();
    let identity = auth
        .ready_bot(&CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity.login, LOGIN);
    let counts = fixture.counts();
    assert_eq!(counts.refresh, 1);
    assert_eq!(counts.validate, 2);
    let grant = auth.load().await.unwrap().unwrap();
    let credentials = crypto::decrypt(&KEY, USER, &grant.ciphertext).unwrap();
    assert_eq!(credentials.access_token, "rotated-access-1");
}

async fn pause_refresh_persistence(auth: &TwitchAuth) -> (CancellationToken, CancellationToken) {
    let received = CancellationToken::new();
    let resume = CancellationToken::new();
    *auth.before_refresh_persist.lock().await = Some((received.clone(), resume.clone()));
    (received, resume)
}

/// Hold the writer and fill its bounded mailbox before the refresh can enqueue.
async fn block_sqlite_mailbox(
    store: &Store,
) -> (
    std::sync::mpsc::Sender<()>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let writer = store.clone();
    let blocker = tokio::spawn(async move {
        writer
            .call(move |_| {
                let _ = entered.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await
    });
    blocked.await.unwrap();
    // Store's mailbox has 128 slots. Polling once enqueues each job while its
    // response remains blocked; dropping the waiter does not remove that job.
    for _ in 0..128 {
        let job = store.call(|_| Ok(()));
        tokio::pin!(job);
        assert!(futures_util::poll!(&mut job).is_pending());
    }
    (release, blocker)
}

#[tokio::test]
async fn dropped_readiness_persists_successful_refresh_through_full_mailbox() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now - 1, now)
        .await;
    let auth = fixture.auth();
    let (received, resume) = pause_refresh_persistence(&auth).await;
    let caller = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.ready_bot(&CancellationToken::new()).await })
    };
    received.cancelled().await;
    let (release, blocker) = block_sqlite_mailbox(&fixture.store).await;
    // Task abort drops the same future that an outer timeout or select drops.
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    resume.cancel();

    let drain = auth.finish_refresh();
    tokio::pin!(drain);
    assert!(futures_util::poll!(&mut drain).is_pending());
    let shutdown = CancellationToken::new();
    let next = auth.bot_access_token(&shutdown);
    tokio::pin!(next);
    assert!(futures_util::poll!(&mut next).is_pending());
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    drain.await;
    assert_eq!(next.await.unwrap(), "rotated-access-1");
    let grant = auth.load().await.unwrap().unwrap();
    let credentials = auth.decrypt(&grant).unwrap();
    assert_eq!(credentials.refresh_token, "rotated-refresh-1");
    assert_eq!(
        grant.validated_at, 0,
        "cancelled readiness did not validate"
    );
    assert_eq!(
        fixture.counts().refresh,
        1,
        "next caller must not rotate again"
    );
}

#[tokio::test]
async fn shutdown_during_refresh_http_finishes_rotation_before_drain() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now - 1, now)
        .await;
    let issued = CancellationToken::new();
    let respond = CancellationToken::new();
    fixture.mock.0.lock().unwrap().refresh_http_pause = Some((issued.clone(), respond.clone()));
    let auth = fixture.auth();
    let shutdown = CancellationToken::new();
    let caller = {
        let auth = auth.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { auth.bot_access_token(&shutdown).await })
    };
    issued.cancelled().await;
    shutdown.cancel();
    assert!(caller.await.unwrap().is_err());
    let drain = auth.finish_refresh();
    tokio::pin!(drain);
    assert!(futures_util::poll!(&mut drain).is_pending());
    respond.cancel();
    drain.await;

    assert_eq!(
        auth.bot_access_token(&CancellationToken::new())
            .await
            .unwrap(),
        "rotated-access-1"
    );
    let grant = auth.load().await.unwrap().unwrap();
    assert_eq!(
        auth.decrypt(&grant).unwrap().refresh_token,
        "rotated-refresh-1"
    );
    assert_eq!(fixture.counts().refresh, 1);
}

#[tokio::test]
async fn detached_refresh_cas_preserves_concurrent_reauthorization() {
    let fixture = Fixture::new().await;
    let now = Utc::now().timestamp();
    fixture
        .seed(KEY, USER, LOGIN, "old-access", "old-refresh", now - 1, now)
        .await;
    let auth = fixture.auth();
    let (received, resume) = pause_refresh_persistence(&auth).await;
    let caller = {
        let auth = auth.clone();
        tokio::spawn(async move { auth.bot_access_token(&CancellationToken::new()).await })
    };
    received.cancelled().await;
    auth.store_grant(
        USER,
        LOGIN,
        &BOT_SCOPES.map(str::to_owned),
        now + 14400,
        &Credentials {
            access_token: "reauthorized-access".into(),
            refresh_token: "reauthorized-refresh".into(),
        },
        false,
    )
    .await
    .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    resume.cancel();
    auth.finish_refresh().await;

    let grant = auth.load().await.unwrap().unwrap();
    let credentials = auth.decrypt(&grant).unwrap();
    assert_eq!(credentials.access_token, "reauthorized-access");
    assert_eq!(credentials.refresh_token, "reauthorized-refresh");
    assert_eq!(
        grant.revision, 2,
        "stale refresh CAS must not advance the row"
    );
    assert!(grant.validated_at > 0);
    assert_eq!(
        auth.bot_access_token(&CancellationToken::new())
            .await
            .unwrap(),
        "reauthorized-access"
    );
    assert_eq!(fixture.counts().refresh, 1);
}
