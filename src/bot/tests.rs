use super::*;
use axum::{
    Router,
    extract::{Form, Query, RawQuery, State},
    routing::{get, patch, post},
};
use futures_util::SinkExt;
use serde_json::json;
use std::{
    collections::HashMap,
    future::Future,
    net::SocketAddr,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message as WsMessage};

fn request_message() -> Message {
    Message {
        id: "evt-1".into(),
        channel: "streamer".into(),
        channel_id: 1,
        author: "viewer".into(),
        author_id: "77".into(),
        moderator: false,
        subscriber: false,
        vip: false,
        points: false,
    }
}

fn chat_event(text: &str) -> Value {
    json!({
        "broadcaster_user_id": "1",
        "broadcaster_user_login": "Streamer",
        "broadcaster_user_name": "Streamer",
        "chatter_user_id": "77",
        "chatter_user_login": "Viewer",
        "chatter_user_name": "Viewer",
        "message_id": "evt-1",
        "message": {"text": text},
        "badges": [{"set_id": "founder", "id": "0", "info": ""}, {"set_id": "vip", "id": "1", "info": ""}],
        "channel_points_custom_reward_id": "reward-1",
    })
}

#[test]
fn chat_event_maps_badges_points_and_filters_the_bot_itself() {
    let chat = chat_message(&chat_event("https://osu.ppy.sh/b/42 +HDDT"), "bot").unwrap();
    assert_eq!(chat.message.id, "evt-1");
    assert_eq!(chat.message.channel, "streamer");
    assert_eq!(chat.message.channel_id, 1);
    assert_eq!(chat.message.author, "viewer");
    assert_eq!(chat.message.author_id, "77");
    assert!(
        chat.message.subscriber,
        "founder badge counts as subscriber"
    );
    assert!(chat.message.vip);
    assert!(chat.message.points);
    assert!(!chat.message.moderator);
    assert_eq!(chat.text, "https://osu.ppy.sh/b/42 +HDDT");

    assert!(
        chat_message(&chat_event("x"), "77").is_none(),
        "own messages are echoed by EventSub"
    );
    let mut invalid = chat_event("x");
    invalid["broadcaster_user_login"] = json!("bad login");
    assert!(chat_message(&invalid, "bot").is_none());
    let mut anonymous = chat_event("x");
    anonymous["chatter_user_id"] = json!("");
    assert!(chat_message(&anonymous, "bot").is_none());
}

#[test]
fn notification_subscription_identity_is_validated_before_use() {
    let conduit = "conduit-1";
    let subscription = json!({
        "type": "channel.chat.message",
        "version": "1",
        "condition": {"broadcaster_user_id": "1", "user_id": "bot"},
        "transport": {"method": "conduit", "conduit_id": conduit},
    });
    let (kind, version, broadcaster) =
        subscription_identity(conduit, &subscription).expect("owned subscription");
    assert_eq!(
        (kind, version, broadcaster),
        ("channel.chat.message", "1", 1)
    );

    let mut foreign = subscription.clone();
    foreign["transport"]["conduit_id"] = json!("somebody-else");
    assert!(subscription_identity(conduit, &foreign).is_none());

    let mut old_version = subscription.clone();
    old_version["version"] = json!("2");
    assert!(subscription_identity(conduit, &old_version).is_none());

    let mut unrelated = subscription;
    unrelated["type"] = json!("channel.follow");
    assert!(subscription_identity(conduit, &unrelated).is_none());
}

#[test]
fn tags_use_boolean_subscriber_and_badge_precedence() {
    let mut message = request_message();
    message.vip = true;
    message.points = true;
    message.subscriber = false;
    let map = serde_json::json!({"id":42,"version":"Hard","status":"ranked","bpm":180,"difficulty_rating":5.0,"hit_length":125});
    let set = serde_json::json!({"artist":"Artist","title":"Title"});
    assert_eq!(
        prepare_message(&message, &map, &set, "+HDDT").unwrap(),
        "[VIP] viewer -> [Ranked] [https://osu.ppy.sh/b/42 Artist - Title [Hard]] (180 BPM, 5.00*, 2:05) +HDDT + USED POINTS"
    );
    message.moderator = true;
    assert!(
        prepare_message(&message, &map, &set, "")
            .unwrap()
            .starts_with("[MOD] ")
    );
}

#[test]
fn inclusive_star_range_and_test_bypass() {
    let mut user: User = serde_json::from_value(serde_json::json!({"osuId":1,"osuUsername":"a","osuAvatarUrl":"","twitchId":1,"twitchUsername":"streamer","twitchAvatarUrl":"","excludedUsers":[],"isLive":true,"settings":{"sr":[4.0,5.0]}})).unwrap();
    let message = request_message();
    assert!(policy(&user, &message, 4.0).is_ok());
    assert!(policy(&user, &message, 5.0).is_ok());
    assert!(policy(&user, &message, 5.1).is_err());
    user.settings.sr = [4.0, -1.0];
    assert!(policy(&user, &message, 9.0).is_ok());
    assert!(policy(&user, &message, 3.9).is_err());
    user.settings.sub_only = true;
    assert!(policy(&user, &message, 5.0).is_err());
    user.settings.sub_only = false;
    user.excluded_users = vec!["VIEWER".into()];
    assert!(policy(&user, &message, 5.0).is_err());
    user.excluded_users.clear();
    let mut broadcaster = message.clone();
    broadcaster.author = "StReAmEr".into();
    assert!(policy(&user, &broadcaster, 5.0).is_err());
    user.settings.test = true;
    user.excluded_users = vec!["VIEWER".into()];
    assert!(policy(&user, &message, 0.0).is_ok());
}

#[test]
fn outbound_chat_cannot_inject_or_split_unicode() {
    assert_eq!(sanitize_chat("a\r\nbé", 5), "a  b");
}

#[test]
fn cooldown_clock_advances_monotonically_with_fractional_seconds() {
    let clock = Clock {
        epoch: 1000.25,
        started: Instant::now() - Duration::from_millis(250),
    };
    let first = clock.now();
    assert!((1000.5..1001.0).contains(&first));
    assert!(clock.now() >= first);
}

#[tokio::test]
async fn delivery_uniqueness_and_crash_state_are_durable() {
    let store = Store::open(":memory:").await.unwrap();
    store
        .call(|db| {
            db.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('duplicate',1,'hello','sending',0)", [])?;
            assert_eq!(db.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at) VALUES('duplicate',1,'hello','pending',0) ON CONFLICT(twitch_message_id) DO NOTHING", [])?, 0);
            db.execute("UPDATE deliveries SET status='uncertain' WHERE status='sending'", [])?;
            assert_eq!(db.query_row("SELECT status FROM deliveries WHERE twitch_message_id='duplicate'", [], |row| row.get::<_, String>(0))?, "uncertain");
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn pending_lookups_are_debounced_bounded_and_backed_off() {
    let mut pending = Pending::default();
    pending.schedule(1);
    pending.schedule(2);
    assert!(pending.take_due(Instant::now()).is_empty());
    let due = pending.take_due(Instant::now() + TARGET_DEBOUNCE + Duration::from_millis(10));
    assert_eq!(due, vec![(1, 0), (2, 0)]);
    pending.retry(1, 0);
    pending.cancel(2);
    let deadline = pending.next_deadline().expect("a retry remains scheduled");
    assert!(
        deadline > Instant::now(),
        "a retry is never due immediately"
    );
    assert!(pending.take_due(Instant::now()).is_empty());
    assert_eq!(
        pending.take_due(Instant::now() + Duration::from_secs(2)),
        vec![(1, 1)]
    );
    assert!(pending.next_deadline().is_none());
}

fn welcome_json(session: &str, keepalive: u64) -> String {
    serde_json::json!({
        "metadata": {"message_id": "welcome", "message_type": "session_welcome", "message_timestamp": "2024-01-01T00:00:00.000000001Z"},
        "payload": {"session": {"id": session, "status": "connected", "keepalive_timeout_seconds": keepalive, "connected_at": "2024-01-01T00:00:00Z", "reconnect_url": null}}
    })
    .to_string()
}

fn keepalive_json() -> String {
    serde_json::json!({
        "metadata": {"message_id": "keepalive", "message_type": "session_keepalive", "message_timestamp": "2024-01-01T00:00:01.000000001Z"},
        "payload": {}
    })
    .to_string()
}

fn notification_json(message_id: &str, text: &str, conduit: &str) -> String {
    serde_json::json!({
        "metadata": {
            "message_id": message_id,
            "message_type": "notification",
            "message_timestamp": "2024-01-01T00:00:02.000000001Z",
            "subscription_type": "channel.chat.message",
            "subscription_version": "1"
        },
        "payload": {
            "subscription": {
                "id": "sub-1",
                "status": "enabled",
                "type": "channel.chat.message",
                "version": "1",
                "condition": {"broadcaster_user_id": "1", "user_id": "bot"},
                "transport": {"method": "conduit", "conduit_id": conduit},
                "created_at": "2024-01-01T00:00:00Z"
            },
            "event": {
                "broadcaster_user_id": "1",
                "broadcaster_user_login": "streamer",
                "broadcaster_user_name": "Streamer",
                "chatter_user_id": "77",
                "chatter_user_login": "viewer",
                "chatter_user_name": "Viewer",
                "message_id": message_id,
                "message": {"text": text},
                "badges": [],
                "channel_points_custom_reward_id": null
            }
        }
    })
    .to_string()
}

fn ws_server<F, Fut>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: FnOnce(WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        handler(socket).await;
    });
    (format!("ws://{address}/ws"), handle)
}

#[tokio::test]
async fn websocket_transport_decodes_welcome_keepalive_and_notification() {
    let notification = notification_json("note-1", "https://osu.ppy.sh/b/42", "conduit-1");
    let (url, server) = ws_server(move |mut socket| async move {
        socket
            .send(WsMessage::text(welcome_json("session-1", 10)))
            .await
            .unwrap();
        socket
            .send(WsMessage::text(keepalive_json()))
            .await
            .unwrap();
        socket.send(WsMessage::text(notification)).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let mut connection = Connection::open(&url).await.unwrap();
    let Incoming::Welcome(welcome) = connection
        .read(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("expected a welcome");
    };
    assert_eq!(welcome.session_id, "session-1");
    assert_eq!(welcome.keepalive, Duration::from_secs(10));
    assert!(matches!(
        connection
            .read(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap(),
        Incoming::Keepalive
    ));
    let Incoming::Notification {
        message_id,
        event,
        subscription,
        ..
    } = connection
        .read(Instant::now() + Duration::from_secs(2))
        .await
        .unwrap()
    else {
        panic!("expected a notification");
    };
    assert_eq!(message_id, "note-1");
    assert_eq!(event["message"]["text"], "https://osu.ppy.sh/b/42");
    assert_eq!(subscription["transport"]["conduit_id"], "conduit-1");
    let chat = chat_message(&event, "bot").unwrap();
    assert_eq!(chat.message.channel_id, 1);
    assert!(parser::parse_message(&chat.text).is_some());
    server.abort();
}

#[tokio::test]
async fn keepalive_deadline_fires_without_protocol_traffic() {
    let (url, server) = ws_server(move |mut socket| async move {
        socket
            .send(WsMessage::text(welcome_json("session-2", 10)))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let mut connection = Connection::open(&url).await.unwrap();
    assert!(matches!(
        connection
            .read(Instant::now() + Duration::from_secs(2))
            .await
            .unwrap(),
        Incoming::Welcome(_)
    ));
    assert!(
        connection
            .read(Instant::now() + Duration::from_millis(150))
            .await
            .is_err(),
        "silence longer than the keepalive window must be treated as a lost connection"
    );
    server.abort();
}

#[tokio::test]
async fn handoff_keeps_the_old_connection_until_the_new_session_is_welcomed() {
    let (old_url, old_server) = ws_server(move |mut socket| async move {
        socket
            .send(WsMessage::text(welcome_json("old", 10)))
            .await
            .unwrap();
        for index in 0..40 {
            let _ = socket
                .send(WsMessage::text(notification_json(
                    &format!("old-{index}"),
                    "https://osu.ppy.sh/b/42",
                    "conduit-1",
                )))
                .await;
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    });
    let (new_url, new_server) = ws_server(move |mut socket| async move {
        socket
            .send(WsMessage::text(welcome_json("new", 10)))
            .await
            .unwrap();
        socket
            .send(WsMessage::text(notification_json(
                "new-1",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    let token = CancellationToken::new();
    let mut link = Link::new();
    let old_generation = link.open(&old_url, &token).await.unwrap();
    link.current = old_generation;
    let welcome = link.welcome(old_generation).await.unwrap();
    assert_eq!(welcome.session_id, "old");
    // Events from the old connection keep flowing while the replacement opens.
    loop {
        let (generation, incoming) = link.rx.recv().await.unwrap();
        if generation == old_generation && matches!(incoming, Incoming::Notification { .. }) {
            break;
        }
    }
    let new_generation = link.open(&new_url, &token).await.unwrap();
    link.pending = Some(new_generation);
    let welcome = loop {
        let (generation, incoming) = link.rx.recv().await.unwrap();
        if generation == new_generation
            && let Incoming::Welcome(welcome) = incoming
        {
            break welcome;
        }
    };
    assert_eq!(welcome.session_id, "new");
    link.complete(new_generation).await;
    assert_eq!(link.current, new_generation);
    assert!(!link.readers.contains_key(&old_generation));
    loop {
        let (generation, incoming) = tokio::time::timeout(Duration::from_secs(2), link.rx.recv())
            .await
            .unwrap()
            .unwrap();
        if generation == new_generation && matches!(incoming, Incoming::Notification { .. }) {
            break;
        }
    }
    link.shutdown().await;
    old_server.abort();
    new_server.abort();
}

async fn seed_user(store: &Store, twitch_id: i64, login: &str) {
    let user: User = serde_json::from_value(json!({
        "osuId": 1,
        "osuUsername": "a",
        "osuAvatarUrl": "",
        "twitchId": twitch_id,
        "twitchUsername": login,
        "twitchAvatarUrl": "",
        "excludedUsers": [],
        "settings": {"enable": true}
    }))
    .unwrap();
    store.save_user(user).await.unwrap();
}

async fn grant_and_go_live(store: &Store, twitch_id: i64, login: &str) {
    let id = twitch_id;
    store
        .call(move |db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(?1,0,'granted')",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .apply_stream_event(twitch_id, login.to_owned(), StreamEvent::Online, 1_000)
        .await
        .unwrap();
    store
        .apply_stream_event(
            twitch_id,
            login.to_owned(),
            StreamEvent::Category("21465".into()),
            2_000,
        )
        .await
        .unwrap();
}

#[derive(Default)]
struct Helix {
    conduit_posts: AtomicUsize,
    creates: AtomicUsize,
    deletes: AtomicUsize,
    deleted: StdMutex<Vec<String>>,
    shard_ok: AtomicBool,
    stream_mode: AtomicU8,
    last_stream_query: StdMutex<String>,
}

fn mock_config(address: SocketAddr) -> Arc<Config> {
    let mut config = Config::from_env().unwrap();
    config.twitch_client_id = "client".into();
    config.twitch_client_secret = "secret".into();
    config.twitch_api_base = format!("http://{address}/helix");
    config.twitch_oauth_base = format!("http://{address}/oauth");
    config.dry_run = false;
    Arc::new(config)
}

async fn helix_server() -> (Arc<Helix>, SocketAddr, tokio::task::JoinHandle<()>) {
    async fn token(Form(body): Form<HashMap<String, String>>) -> axum::Json<Value> {
        assert_eq!(body["grant_type"], "client_credentials");
        axum::Json(json!({"access_token":"app-token","expires_in":3600}))
    }
    async fn conduits(State(_): State<Arc<Helix>>) -> axum::Json<Value> {
        axum::Json(json!({"data":[{"id":"conduit-1","shard_count":1}],"pagination":{}}))
    }
    async fn create_conduit(State(state): State<Arc<Helix>>) -> axum::Json<Value> {
        state.conduit_posts.fetch_add(1, Ordering::SeqCst);
        axum::Json(json!({"data":[{"id":"conduit-new","shard_count":1}]}))
    }
    async fn update_shards(
        State(state): State<Arc<Helix>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        assert_eq!(body["conduit_id"], "conduit-1");
        let session = body["shards"][0]["transport"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(body["shards"][0]["id"], "0");
        if state.shard_ok.load(Ordering::SeqCst) {
            axum::Json(
                json!({"data":[{"id":"0","status":"enabled","transport":{"method":"websocket","session_id":session}}]}),
            )
        } else {
            axum::Json(
                json!({"data":[{"id":"0","status":"websocket_disconnected","errors":[{"code":"websocket_disconnected"}]}]}),
            )
        }
    }
    async fn list_subscriptions(State(_): State<Arc<Helix>>) -> axum::Json<Value> {
        axum::Json(json!({
            "data": [
                {"id":"obsolete","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"999"},"transport":{"method":"conduit","conduit_id":"conduit-1"}},
                {"id":"chat-1","status":"enabled","type":"channel.chat.message","version":"1","condition":{"broadcaster_user_id":"1","user_id":"bot"},"transport":{"method":"conduit","conduit_id":"conduit-1"}},
                {"id":"wrong-bot","status":"enabled","type":"channel.chat.message","version":"1","condition":{"broadcaster_user_id":"1","user_id":"other"},"transport":{"method":"conduit","conduit_id":"conduit-1"}},
                {"id":"foreign","status":"enabled","type":"channel.chat.message","version":"1","condition":{"broadcaster_user_id":"1","user_id":"bot"},"transport":{"method":"conduit","conduit_id":"conduit-9"}}
            ],
            "pagination": {}
        }))
    }
    async fn create_subscription(
        State(state): State<Arc<Helix>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        assert_eq!(body["transport"]["method"], "conduit");
        assert_eq!(body["transport"]["conduit_id"], "conduit-1");
        assert!(body["condition"]["broadcaster_user_id"].is_string());
        let count = state.creates.fetch_add(1, Ordering::SeqCst) + 1;
        axum::Json(
            json!({"data":[{"id":format!("new-{count}")}],"total":1,"total_cost":1,"max_total_cost":10,"pagination":{}}),
        )
    }
    async fn delete_subscription(
        State(state): State<Arc<Helix>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> axum::http::StatusCode {
        state.deletes.fetch_add(1, Ordering::SeqCst);
        state
            .deleted
            .lock()
            .unwrap()
            .push(query.get("id").cloned().unwrap_or_default());
        axum::http::StatusCode::NO_CONTENT
    }
    async fn streams(
        State(state): State<Arc<Helix>>,
        RawQuery(query): RawQuery,
    ) -> axum::Json<Value> {
        *state.last_stream_query.lock().unwrap() = query.unwrap_or_default();
        if state.stream_mode.load(Ordering::SeqCst) == 0 {
            axum::Json(
                json!({"data":[{"user_id":"1","user_login":"streamer","game_id":"21465"}],"pagination":{}}),
            )
        } else {
            axum::Json(json!({"data":[],"pagination":{}}))
        }
    }

    let state = Arc::new(Helix::default());
    state.shard_ok.store(true, Ordering::SeqCst);
    let router = Router::new()
        .route("/oauth/token", post(token))
        .route(
            "/helix/eventsub/conduits",
            get(conduits).post(create_conduit),
        )
        .route("/helix/eventsub/conduits/shards", patch(update_shards))
        .route(
            "/helix/eventsub/subscriptions",
            get(list_subscriptions)
                .post(create_subscription)
                .delete(delete_subscription),
        )
        .route("/helix/streams", get(streams))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (state, address, server)
}

#[tokio::test]
async fn conduit_is_reused_persisted_and_shard_errors_are_rejected() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    store
        .call(|db| {
            db.execute("INSERT INTO twitch_conduit(singleton,client_id,conduit_id,updated_at) VALUES(1,'client','conduit-1',0)", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let conduit = super::conduit::ensure(&config, &store, &providers)
        .await
        .unwrap();
    assert_eq!(
        conduit.id, "conduit-1",
        "the persisted owned conduit is reused"
    );
    assert_eq!(state.conduit_posts.load(Ordering::SeqCst), 0);

    super::conduit::attach(&providers, "conduit-1", "session-A")
        .await
        .unwrap();
    state.shard_ok.store(false, Ordering::SeqCst);
    assert!(
        super::conduit::attach(&providers, "conduit-1", "session-B")
            .await
            .is_err(),
        "a shard that is not enabled must not be treated as attached"
    );
    state.shard_ok.store(true, Ordering::SeqCst);
    server.abort();
}

#[tokio::test]
async fn subscription_reconcile_only_touches_owned_rows() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let providers = Providers::new(config, CancellationToken::new()).unwrap();
    let token = CancellationToken::new();
    let mut subs = Subscriptions::new("conduit-1".into(), "bot".into());
    assert_eq!(subs.refresh_remote(&providers, &token).await.unwrap(), 3);
    assert!(subs.is_warm());
    let desired = conduit::desired(&[1], "bot");
    let summary = subs.reconcile(&providers, &desired, &token).await.unwrap();
    assert_eq!(summary.kept, 1, "the matching chat subscription is reused");
    assert_eq!(
        summary.removed, 2,
        "obsolete and wrong-bot rows are removed"
    );
    assert_eq!(
        summary.created, 3,
        "the three state subscriptions are created"
    );
    assert_eq!(state.creates.load(Ordering::SeqCst), 3);
    assert_eq!(state.deletes.load(Ordering::SeqCst), 2);
    let deleted = state.deleted.lock().unwrap().clone();
    assert!(deleted.contains(&"obsolete".to_owned()));
    assert!(deleted.contains(&"wrong-bot".to_owned()));
    assert!(!deleted.contains(&"foreign".to_owned()));
    server.abort();
}

#[tokio::test]
async fn hybrid_snapshots_see_offline_states_and_never_filter_by_category() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    let providers = Providers::new(config, CancellationToken::new()).unwrap();

    // Going offline via the API must be observable: no game_id filter.
    state.stream_mode.store(1, Ordering::SeqCst);
    let skipped = super::streams::targeted(&store, &providers, &[1])
        .await
        .unwrap();
    assert!(skipped.is_empty());
    let query = state.last_stream_query.lock().unwrap().clone();
    assert!(
        !query.contains("game_id"),
        "query {query:?} must not filter a category"
    );
    assert!(query.contains("user_id=1"));
    assert!(
        !store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "an offline snapshot removes eligibility"
    );

    // Back online on osu! restores eligibility.
    state.stream_mode.store(0, Ordering::SeqCst);
    super::streams::full(&store, &providers).await.unwrap();
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    server.abort();
}

#[tokio::test]
async fn test_mode_receipts_do_not_consume_production_cooldowns_and_dryrun_never_chats() {
    use axum::{Json, Router, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let sends = Arc::new(AtomicUsize::new(0));
    let observed = sends.clone();
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["scope"], "delegate chat.write");
                Json(serde_json::json!({"access_token":"delegate","expires_in":3600}))
            }),
        )
        .route(
            "/api/v2/chat/new",
            post(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"message_id":1}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut config = Config::from_env().unwrap();
    config.dry_run = false;
    config.osu_client_id = "test".into();
    config.osu_client_secret = "test".into();
    config.osu_oauth_base = format!("http://{address}/oauth");
    config.osu_api_base = format!("http://{address}/api/v2");
    let providers = Providers::new(Arc::new(config.clone()), CancellationToken::new()).unwrap();
    let store = Store::open(":memory:").await.unwrap();
    let mut user: User = serde_json::from_value(serde_json::json!({"osuId":1,"osuUsername":"a","osuAvatarUrl":"","twitchId":1,"twitchUsername":"streamer","twitchAvatarUrl":"","excludedUsers":[],"settings":{"test":true,"cooldown":30,"enable":true}})).unwrap();
    user.settings.validate().unwrap();
    store.save_user(user.clone()).await.unwrap();
    grant_and_go_live(&store, 1, "streamer").await;
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 3_000)
        .await
        .unwrap();
    assert!(
        !store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "fixture must start offline"
    );
    store.cache_put(serde_json::json!({"id":42,"beatmapset_id":7,"version":"Hard","status":"ranked","bpm":180,"difficulty_rating":5.0,"hit_length":125,"beatmapset":{"id":7,"artist":"Artist","title":"Title"}})).await.unwrap();
    let (echoes, mut echo_rx) = mpsc::channel(4);
    let beatmap = Beatmap {
        id: 42,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    let clock = Clock::new();
    process_one(
        &config,
        &store,
        &providers,
        &echoes,
        &request_message(),
        &beatmap,
        clock,
    )
    .await
    .unwrap();
    store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_cooldowns", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    user.settings.test = false;
    store.set_settings(1, user.settings.clone()).await.unwrap();
    let mut offline = request_message();
    offline.id = "production-offline".into();
    process_one(
        &config, &store, &providers, &echoes, &offline, &beatmap, clock,
    )
    .await
    .unwrap();
    assert_eq!(
        sends.load(Ordering::SeqCst),
        1,
        "non-test offline request must not be delivered"
    );
    store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |row| row
                    .get::<_, i64>(0))?,
                1,
                "offline production rejection must not be recorded"
            );
            Ok(())
        })
        .await
        .unwrap();
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Online, 4_000)
        .await
        .unwrap();
    store
        .apply_stream_event(
            1,
            "streamer".into(),
            StreamEvent::Category("21465".into()),
            5_000,
        )
        .await
        .unwrap();
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    let mut production = request_message();
    production.id = "production".into();
    process_one(
        &config,
        &store,
        &providers,
        &echoes,
        &production,
        &beatmap,
        clock,
    )
    .await
    .unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_cooldowns", [], |row| row
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    while echo_rx.try_recv().is_ok() {}
    config.dry_run = true;
    user.settings.test = true;
    store.set_settings(1, user.settings.clone()).await.unwrap();
    let mut dry = request_message();
    dry.id = "dry-accepted".into();
    process_one(&config, &store, &providers, &echoes, &dry, &beatmap, clock)
        .await
        .unwrap();
    user.settings.test = false;
    user.settings.sr = [6.0, 9.0];
    store.set_settings(1, user.settings).await.unwrap();
    dry.id = "dry-rejected".into();
    process_one(&config, &store, &providers, &echoes, &dry, &beatmap, clock)
        .await
        .unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    assert!(echo_rx.try_recv().is_err());
    store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |row| row
                    .get::<_, i64>(0))?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn repeated_set_requests_cache_the_first_api_map_not_an_arbitrary_cached_map() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let lookups = Arc::new(AtomicUsize::new(0));
    let observed_lookups = lookups.clone();
    let sends = Arc::new(AtomicUsize::new(0));
    let observed_sends = sends.clone();
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|| async { Json(serde_json::json!({"access_token":"test","expires_in":3600})) }),
        )
        .route(
            "/api/v2/beatmapsets/7",
            get(move || {
                let observed = observed_lookups.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "id":7,"artist":"Artist","title":"Title","beatmaps":[
                            {"id":99,"beatmapset_id":7,"version":"First","status":"ranked","bpm":180,"difficulty_rating":5.0,"hit_length":125},
                            {"id":42,"beatmapset_id":7,"version":"Second","status":"ranked","bpm":180,"difficulty_rating":5.0,"hit_length":125}
                        ]
                    }))
                }
            }),
        )
        .route(
            "/api/v2/chat/new",
            post(move |Json(body): Json<Value>| {
                let observed = observed_sends.clone();
                async move {
                    assert!(body["message"].as_str().unwrap().contains("https://osu.ppy.sh/b/99 "));
                    observed.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"message_id":1}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut config = Config::from_env().unwrap();
    config.dry_run = false;
    config.osu_client_id = "test".into();
    config.osu_client_secret = "test".into();
    config.osu_oauth_base = format!("http://{address}/oauth");
    config.osu_api_base = format!("http://{address}/api/v2");
    let providers = Providers::new(Arc::new(config.clone()), CancellationToken::new()).unwrap();
    let store = Store::open(":memory:").await.unwrap();
    let user: User = serde_json::from_value(serde_json::json!({"osuId":1,"osuUsername":"a","osuAvatarUrl":"","twitchId":1,"twitchUsername":"streamer","twitchAvatarUrl":"","excludedUsers":[],"settings":{"cooldown":0,"echo":false,"enable":true}})).unwrap();
    store.save_user(user).await.unwrap();
    grant_and_go_live(&store, 1, "streamer").await;
    store.cache_put(serde_json::json!({"id":42,"beatmapset_id":7,"beatmapset":{"id":7,"artist":"Artist","title":"Title"}})).await.unwrap();
    let (echoes, _echo_rx) = mpsc::channel(4);
    let beatmap = Beatmap {
        id: 7,
        kind: BeatmapKind::Mapset,
        mods: String::new(),
    };
    let clock = Clock::new();
    let mut request = request_message();
    process_one(
        &config, &store, &providers, &echoes, &request, &beatmap, clock,
    )
    .await
    .unwrap();
    request.id = "second-set-request".into();
    process_one(
        &config, &store, &providers, &echoes, &request, &beatmap, clock,
    )
    .await
    .unwrap();
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
    assert_eq!(sends.load(Ordering::SeqCst), 2);
    let cached = store.cache_get_set(7).await.unwrap().unwrap();
    assert_eq!(cached["id"], 99);
    assert!(cached["beatmapset"].get("beatmaps").is_none());
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM statistics WHERE requested_beatmap_id=99",
                    [],
                    |row| row.get::<_, i64>(0)
                )?,
                2
            );
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}
