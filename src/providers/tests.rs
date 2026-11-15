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
    request_targets: std::sync::Mutex<Vec<String>>,
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

#[test]
fn provider_metric_labels_are_bounded() {
    for (endpoint, operation) in [
        ("beatmaps/42/attributes", "beatmaps"),
        ("beatmapsets/7", "beatmapsets"),
        ("chat/messages", "chat"),
        ("eventsub/conduits", "eventsub"),
        ("streams", "streams"),
        ("users/private-user-name/osu", "users"),
        ("unexpected/private-id", "other"),
    ] {
        assert_eq!(provider_operation(endpoint), operation);
    }
    for (status, outcome) in [
        (StatusCode::CONTINUE, "1xx"),
        (StatusCode::OK, "2xx"),
        (StatusCode::FOUND, "3xx"),
        (StatusCode::NOT_FOUND, "4xx"),
        (StatusCode::INTERNAL_SERVER_ERROR, "5xx"),
    ] {
        assert_eq!(status_outcome(status), outcome);
    }
}

#[tokio::test]
async fn missing_beatmaps_preserve_typed_404_without_retrying() {
    use axum::{
        Router,
        extract::State,
        routing::{get, post},
    };
    async fn missing(
        State(state): State<Arc<MockState>>,
    ) -> (axum::http::StatusCode, axum::Json<Value>) {
        state
            .map_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"message":"no such beatmap"})),
        )
    }
    let state = Arc::new(MockState::default());
    let router = Router::new()
        .route("/oauth/token", post(mock_token))
        .route("/api/v2/beatmaps/404", get(missing))
        .route("/api/v2/beatmapsets/404", get(missing))
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
    for (kind, endpoint) in [
        (BeatmapKind::Map, "beatmaps/404"),
        (BeatmapKind::Mapset, "beatmapsets/404"),
    ] {
        let error = providers
            .get_beatmap(&Beatmap {
                id: 404,
                kind,
                mods: String::new(),
            })
            .await
            .unwrap_err();
        let error = error.downcast_ref::<ChatError>().unwrap();
        assert_eq!(error.status, Some(404));
        assert!(!error.uncertain);
        assert!(error.message.contains("404 Not Found"));
        assert!(error.message.contains(endpoint));
        assert!(error.message.contains("no such beatmap"));
    }
    assert_eq!(state.map_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(*state.scopes.lock().unwrap(), vec!["public"]);
    server.abort();
}

#[test]
fn retry_after_accepts_http_dates_and_retains_bounds_and_fallbacks() {
    fn response(retry: Option<&str>, reset: Option<&str>) -> Response {
        let mut builder = axum::http::Response::builder();
        if let Some(retry) = retry {
            builder = builder.header(reqwest::header::RETRY_AFTER, retry);
        }
        if let Some(reset) = reset {
            builder = builder.header("Ratelimit-Reset", reset);
        }
        builder.body("").unwrap().into()
    }

    let future =
        httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_secs(4_102_444_800));
    assert_eq!(
        retry_after(&response(Some(&future), Some("1")), 3),
        Duration::from_secs(60)
    );
    for past in [
        "Sun, 06 Nov 1994 08:49:37 GMT",
        "Sunday, 06-Nov-94 08:49:37 GMT",
        "Sun Nov  6 08:49:37 1994",
    ] {
        assert_eq!(
            retry_after(&response(Some(past), None), 5),
            Duration::from_secs(1)
        );
    }
    for (retry, reset, attempt, seconds) in [
        (Some("12"), Some("1"), 3, 12),
        (Some("0"), None, 0, 1),
        (Some("999"), None, 0, 60),
        (Some("invalid"), Some("4102444800"), 0, 60),
        (Some("invalid"), None, 3, 8),
        (None, None, 100, 32),
    ] {
        assert_eq!(
            retry_after(&response(retry, reset), attempt),
            Duration::from_secs(seconds)
        );
    }
}

#[tokio::test]
async fn username_path_encoding_preserves_the_actual_request_target() {
    use axum::{
        Router,
        extract::{OriginalUri, State},
        routing::post,
    };
    async fn user(
        State(state): State<Arc<MockState>>,
        OriginalUri(uri): OriginalUri,
    ) -> axum::Json<Value> {
        state.request_targets.lock().unwrap().push(uri.to_string());
        axum::Json(json!({"id":7}))
    }
    let state = Arc::new(MockState::default());
    let router = Router::new()
        .route("/oauth/token", post(mock_token))
        .fallback(user)
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
    let user = providers
        .get_user_info(
            "name with+plus/slash?query#hash%percent&._~-é",
            Some("osu"),
            Some("username"),
        )
        .await
        .unwrap();
    assert_eq!(user["id"], 7);
    assert_eq!(
        *state.request_targets.lock().unwrap(),
        vec![
            "/api/v2/users/name%20with%2Bplus%2Fslash%3Fquery%23hash%25percent%26._~-%C3%A9/osu?key=username"
        ]
    );
    server.abort();
}

struct ForbiddenState {
    tokens: Arc<MockState>,
    failures: usize,
    calls: std::sync::atomic::AtomicUsize,
    bodies: std::sync::Mutex<Vec<Value>>,
    successes: std::sync::atomic::AtomicUsize,
}

async fn forbidden_response(
    state: &ForbiddenState,
) -> (
    axum::http::StatusCode,
    [(&'static str, &'static str); 1],
    axum::Json<Value>,
) {
    let call = state
        .calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if call < state.failures {
        (
            axum::http::StatusCode::FORBIDDEN,
            [("retry-after", "1")],
            axum::Json(json!({"message":"temporarily forbidden"})),
        )
    } else {
        state
            .successes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (
            axum::http::StatusCode::OK,
            [("retry-after", "1")],
            axum::Json(json!({"id":42,"beatmapset_id":7,"beatmapset":{"id":7}})),
        )
    }
}

async fn forbidden_scenario(chat: bool, failures: usize) {
    use axum::{
        Router,
        extract::State,
        routing::{get, post},
    };
    async fn token(
        State(state): State<Arc<ForbiddenState>>,
        body: axum::Json<Value>,
    ) -> axum::Json<Value> {
        mock_token(State(state.tokens.clone()), body).await
    }
    async fn metadata(
        State(state): State<Arc<ForbiddenState>>,
    ) -> impl axum::response::IntoResponse {
        forbidden_response(&state).await
    }
    async fn message(
        State(state): State<Arc<ForbiddenState>>,
        axum::Json(body): axum::Json<Value>,
    ) -> impl axum::response::IntoResponse {
        state.bodies.lock().unwrap().push(body);
        forbidden_response(&state).await
    }
    let state = Arc::new(ForbiddenState {
        tokens: Arc::new(MockState::default()),
        failures,
        calls: Default::default(),
        bodies: Default::default(),
        successes: Default::default(),
    });
    let router = Router::new()
        .route("/oauth/token", post(token))
        .route("/api/v2/beatmaps/42", get(metadata))
        .route("/api/v2/chat/new", post(message))
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
    let result = if chat {
        providers.send_message(7, "hello", false).await
    } else {
        providers
            .get_beatmap(&Beatmap {
                id: 42,
                kind: BeatmapKind::Map,
                mods: String::new(),
            })
            .await
            .map(|(map, _)| assert_eq!(map["id"], 42))
            .map_err(|error| error.downcast::<ChatError>().unwrap())
    };
    let exhausted = failures >= 3;
    if exhausted {
        let error = result.unwrap_err();
        assert_eq!(error.status, Some(403));
        assert!(!error.uncertain);
        assert!(error.message.contains("temporarily forbidden"));
    } else {
        result.unwrap();
    }
    let calls = if exhausted { 3 } else { failures + 1 };
    assert_eq!(state.calls.load(std::sync::atomic::Ordering::SeqCst), calls);
    assert_eq!(
        state.successes.load(std::sync::atomic::Ordering::SeqCst),
        usize::from(!exhausted)
    );
    let bodies = state.bodies.lock().unwrap();
    if chat {
        assert_eq!(
            *bodies,
            vec![json!({"target_id":7,"message":"hello","is_action":false}); calls]
        );
    } else {
        assert!(bodies.is_empty());
    }
    assert_eq!(
        *state.tokens.scopes.lock().unwrap(),
        vec![if chat {
            "delegate chat.write"
        } else {
            "public"
        }]
    );
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn temporary_osu_forbidden_metadata_recovers() {
    forbidden_scenario(false, 2).await;
}

#[tokio::test(start_paused = true)]
async fn temporary_osu_forbidden_pm_recovers_without_duplicate_success() {
    forbidden_scenario(true, 2).await;
}

#[tokio::test(start_paused = true)]
async fn osu_forbidden_metadata_exhausts_three_attempts_with_known_status() {
    forbidden_scenario(false, 3).await;
}

#[tokio::test(start_paused = true)]
async fn osu_forbidden_pm_exhausts_three_attempts_with_known_status() {
    forbidden_scenario(true, 3).await;
}
