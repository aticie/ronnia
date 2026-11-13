use super::conduit::Subscriptions;
use super::eventsub::{Connection, Incoming};
use super::*;
use axum::{
    Router,
    extract::{Form, Query, RawQuery, State},
    routing::{get, patch, post},
};
use futures_util::{SinkExt, StreamExt};
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
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message as WsMessage,
        handshake::server::{Callback, ErrorResponse, Request, Response},
    },
};

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

pub(super) async fn seed_user(store: &Store, twitch_id: i64, login: &str) {
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
pub(super) struct Helix {
    conduit_posts: AtomicUsize,
    pub(super) creates: AtomicUsize,
    pub(super) deletes: AtomicUsize,
    pub(super) list_calls: AtomicUsize,
    pub(super) seeded: StdMutex<Option<Vec<Value>>>,
    deleted: StdMutex<Vec<String>>,
    shard_ok: AtomicBool,
    wrong_shard: AtomicBool,
    shard_count: AtomicUsize,
    resizes: StdMutex<Vec<u32>>,
    attachments: StdMutex<Vec<(u32, String)>>,
    shard_delays: StdMutex<HashMap<u32, Duration>>,
    shard_failures: StdMutex<HashMap<u32, usize>>,
    pub(super) stream_mode: AtomicU8,
    pub(super) stream_calls: AtomicUsize,
    last_stream_query: StdMutex<String>,
    hold_streams: AtomicBool,
    release_streams: tokio::sync::Notify,
}

pub(super) fn mock_config(address: SocketAddr) -> Arc<Config> {
    let mut config = Config::from_env().unwrap();
    config.twitch_client_id = "client".into();
    config.twitch_client_secret = "secret".into();
    config.twitch_api_base = format!("http://{address}/helix");
    config.twitch_oauth_base = format!("http://{address}/oauth");
    config.dry_run = false;
    Arc::new(config)
}

pub(super) async fn helix_server() -> (Arc<Helix>, SocketAddr, tokio::task::JoinHandle<()>) {
    async fn token(Form(body): Form<HashMap<String, String>>) -> axum::Json<Value> {
        assert_eq!(body["grant_type"], "client_credentials");
        axum::Json(json!({"access_token":"app-token","expires_in":3600}))
    }
    async fn conduits(State(state): State<Arc<Helix>>) -> axum::Json<Value> {
        axum::Json(
            json!({"data":[{"id":"conduit-1","shard_count":state.shard_count.load(Ordering::SeqCst)}],"pagination":{}}),
        )
    }
    async fn resize_conduit(
        State(state): State<Arc<Helix>>,
        axum::Json(body): axum::Json<Value>,
    ) -> axum::Json<Value> {
        assert_eq!(body["id"], "conduit-1");
        let count = body["shard_count"].as_u64().unwrap() as u32;
        assert!(count > 0);
        state.shard_count.store(count as usize, Ordering::SeqCst);
        state.resizes.lock().unwrap().push(count);
        axum::Json(json!({"data":[{"id":"conduit-1","shard_count":count}]}))
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
        let shard = body["shards"][0]["id"]
            .as_str()
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert!((shard as usize) < state.shard_count.load(Ordering::SeqCst));
        let returned_shard = if state.wrong_shard.load(Ordering::SeqCst) {
            (shard + 1).to_string()
        } else {
            shard.to_string()
        };
        state
            .attachments
            .lock()
            .unwrap()
            .push((shard, session.clone()));
        let delay = state.shard_delays.lock().unwrap().get(&shard).copied();
        if let Some(delay) = delay {
            sleep(delay).await;
        }
        let fail_once = state
            .shard_failures
            .lock()
            .unwrap()
            .get_mut(&shard)
            .is_some_and(|remaining| {
                if *remaining == 0 {
                    false
                } else {
                    *remaining -= 1;
                    true
                }
            });
        if state.shard_ok.load(Ordering::SeqCst) && !fail_once {
            axum::Json(
                json!({"data":[{"id":returned_shard,"status":"enabled","transport":{"method":"websocket","session_id":session}}]}),
            )
        } else {
            axum::Json(
                json!({"data":[{"id":returned_shard,"status":"websocket_disconnected","errors":[{"code":"websocket_disconnected"}]}]}),
            )
        }
    }
    async fn list_subscriptions(State(state): State<Arc<Helix>>) -> axum::Json<Value> {
        state.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(rows) = state.seeded.lock().unwrap().clone() {
            return axum::Json(json!({"data": rows, "pagination": {}}));
        }
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
        state.stream_calls.fetch_add(1, Ordering::SeqCst);
        *state.last_stream_query.lock().unwrap() = query.unwrap_or_default();
        if state.hold_streams.load(Ordering::SeqCst) {
            state.release_streams.notified().await;
        }
        let category = match state.stream_mode.load(Ordering::SeqCst) {
            0 => Some("21465"),
            2 => Some("295941293"),
            3 => Some("509659"),
            _ => None,
        };
        match category {
            Some(category) => axum::Json(
                json!({"data":[{"user_id":"1","user_login":"streamer","game_id":category}],"pagination":{}}),
            ),
            None => axum::Json(json!({"data":[],"pagination":{}})),
        }
    }

    let state = Arc::new(Helix::default());
    state.shard_ok.store(true, Ordering::SeqCst);
    state.shard_count.store(1, Ordering::SeqCst);
    let router = Router::new()
        .route("/oauth/token", post(token))
        .route(
            "/helix/eventsub/conduits",
            get(conduits).post(create_conduit).patch(resize_conduit),
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

    assert_eq!(conduit.shard_count, 1);
    super::conduit::resize(&providers, "conduit-1", 3)
        .await
        .unwrap();
    assert_eq!(
        super::conduit::observe(&providers, "conduit-1")
            .await
            .unwrap()
            .shard_count,
        3
    );
    assert_eq!(*state.resizes.lock().unwrap(), vec![3]);
    super::conduit::attach(&providers, "conduit-1", 2, "session-A")
        .await
        .unwrap();
    state.shard_ok.store(false, Ordering::SeqCst);
    assert!(
        super::conduit::attach(&providers, "conduit-1", 1, "session-B")
            .await
            .is_err(),
        "a shard that is not enabled must not be treated as attached"
    );
    state.shard_ok.store(true, Ordering::SeqCst);
    state.wrong_shard.store(true, Ordering::SeqCst);
    assert!(
        super::conduit::attach(&providers, "conduit-1", 2, "session-C")
            .await
            .is_err(),
        "an enabled response for a different shard must not acknowledge this attachment"
    );
    state.wrong_shard.store(false, Ordering::SeqCst);
    super::conduit::attach(&providers, "conduit-1", 1, "session-D")
        .await
        .unwrap();
    assert_eq!(
        *state.attachments.lock().unwrap(),
        vec![
            (2, "session-A".into()),
            (1, "session-B".into()),
            (2, "session-C".into()),
            (1, "session-D".into())
        ]
    );
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
    let desired = conduit::desired(&[1], "bot").unwrap();
    let summary = subs
        .reconcile(&providers, &desired, true, &token)
        .await
        .unwrap();
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
async fn incomplete_topology_defers_creations_but_still_lists_and_removes_owned_rows() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let providers = Providers::new(config, CancellationToken::new()).unwrap();
    let token = CancellationToken::new();
    let mut subs = Subscriptions::new("conduit-1".into(), "bot".into());
    let desired = conduit::desired(&[1], "bot").unwrap();
    assert!(!subs.is_warm());
    let summary = subs
        .reconcile(&providers, &desired, false, &token)
        .await
        .unwrap();
    assert!(
        subs.is_warm(),
        "an incomplete topology must not block the remote cache refresh"
    );
    assert_eq!(
        summary.kept, 1,
        "the matching chat subscription is retained"
    );
    assert_eq!(
        summary.removed, 2,
        "obsolete and wrong-bot owned rows must still be deleted"
    );
    assert_eq!(summary.created, 0);
    assert_eq!(
        summary.deferred, 3,
        "only missing desired subscriptions await attachment"
    );
    assert_eq!(
        state.creates.load(Ordering::SeqCst),
        0,
        "the closed creation gate must send no subscription POSTs"
    );
    assert_eq!(state.deletes.load(Ordering::SeqCst), 2);
    let mut deleted = state.deleted.lock().unwrap().clone();
    deleted.sort();
    assert_eq!(
        deleted,
        vec!["obsolete", "wrong-bot"],
        "foreign conduit subscriptions must remain untouched"
    );

    let summary = subs
        .reconcile(&providers, &desired, true, &token)
        .await
        .unwrap();
    assert_eq!(
        summary.created, 3,
        "opening the gate resumes the deferred creations"
    );
    assert_eq!(summary.deferred, 0);
    assert_eq!(state.creates.load(Ordering::SeqCst), 3);
    assert_eq!(
        state.deletes.load(Ordering::SeqCst),
        2,
        "cache retention avoids repeating the deletions"
    );
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
async fn lazer_streams_are_discovered_and_eligible_but_just_chatting_is_not() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    let providers = Providers::new(config, CancellationToken::new()).unwrap();

    // A Lazer stream is discovered like an osu! stream and stays eligible.
    state.stream_mode.store(2, Ordering::SeqCst);
    super::streams::full(&store, &providers).await.unwrap();
    assert!(
        store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "Osu!Lazer restores eligibility"
    );

    // Just Chatting is observed too, but never grants eligibility.
    state.stream_mode.store(3, Ordering::SeqCst);
    super::streams::full(&store, &providers).await.unwrap();
    assert!(
        !store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "Just Chatting removes eligibility"
    );
    server.abort();
}

fn maintenance_for_test(
    config: Arc<Config>,
    store: Store,
    providers: Providers,
) -> (
    super::maintenance::Maintenance,
    watch::Receiver<u32>,
    mpsc::Sender<super::maintenance::Action>,
) {
    let (_, signals) = watch::channel(super::maintenance::Signal::default());
    let (action_tx, actions) = super::maintenance::actions();
    let (targets, target_rx) = watch::channel(1);
    let (_, topology) = watch::channel(super::shards::Topology {
        remote_count: 1,
        attached_count: 1,
    });
    let channels = store.channel_changes();
    (
        super::maintenance::Maintenance::new(
            config,
            store,
            providers,
            "conduit-1".into(),
            "bot".into(),
            super::maintenance::Channels {
                membership: channels,
                signals,
                actions,
                targets,
                topology,
            },
        ),
        target_rx,
        action_tx,
    )
}

async fn assert_existing_channel_grant_refresh(grace_secs: u64) {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (mut maintenance, _targets, _actions) =
        maintenance_for_test(config, store.clone(), providers.clone());
    // Startup already knows the enabled, ungranted imported/legacy account.
    // Its initial snapshot is offline; Twitch goes live without a state event.
    state.stream_mode.store(1, Ordering::SeqCst);
    maintenance.refresh_membership().await.unwrap();
    maintenance.run_pending().await.unwrap();
    super::streams::full(&store, &providers).await.unwrap();
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    let initial_calls = state.stream_calls.load(Ordering::SeqCst);
    state.stream_mode.store(0, Ordering::SeqCst);
    store
        .call(move |db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(1,unixepoch(),unixepoch()+?1,'granted')",
                [grace_secs],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    if grace_secs != 0 {
        // An unrelated settings notification during grace must not consume
        // the later activation transition or poll an already-known channel.
        store
            .set_settings(1, crate::domain::Settings::default())
            .await
            .unwrap();
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        assert_eq!(state.stream_calls.load(Ordering::SeqCst), initial_calls);
        assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        sleep(Duration::from_secs(grace_secs)).await;
    }
    store.notify_channels_changed();
    maintenance.refresh_membership().await.unwrap();
    maintenance.run_pending().await.unwrap();
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        initial_calls + 1,
        "activating an existing enabled account must trigger one targeted lookup"
    );
    assert_eq!(
        *state.last_stream_query.lock().unwrap(),
        "first=100&user_id=1",
        "activation must look up only the newly granted channel"
    );
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    assert_eq!(store.live_users(100, 0).await.unwrap(), vec!["streamer"]);

    let settings = crate::domain::Settings {
        echo: false,
        ..Default::default()
    };
    store.set_settings(1, settings).await.unwrap();
    maintenance.refresh_membership().await.unwrap();
    maintenance.run_pending().await.unwrap();
    maintenance.refresh_membership().await.unwrap();
    maintenance.run_pending().await.unwrap();
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        initial_calls + 1,
        "unrelated settings/state notifications must not repeatedly poll Helix"
    );
    server.abort();
}

#[tokio::test]
async fn membership_refreshes_existing_enabled_channel_on_grant_activation() {
    assert_existing_channel_grant_refresh(0).await;
}

#[tokio::test]
async fn membership_refreshes_existing_enabled_channel_after_grant_grace() {
    assert_existing_channel_grant_refresh(1).await;
}

#[tokio::test]
async fn failed_membership_reads_preserve_the_grant_activation_edge() {
    for failed_table in ["users", "twitch_channel_grants"] {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (mut maintenance, _targets, _actions) =
            maintenance_for_test(config, store.clone(), providers.clone());
        state.stream_mode.store(1, Ordering::SeqCst);
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        super::streams::full(&store, &providers).await.unwrap();
        let initial_calls = state.stream_calls.load(Ordering::SeqCst);
        state.stream_mode.store(0, Ordering::SeqCst);
        store.call(|db| {
            db.execute("INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(1,0,0,'granted')", [])?;
            Ok(())
        }).await.unwrap();
        store
            .call(move |db| {
                db.execute_batch(&format!(
                    "ALTER TABLE {failed_table} RENAME TO unavailable_membership"
                ))?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            maintenance.refresh_membership().await.is_err(),
            "the {failed_table} read must fail before advancing either known set"
        );
        assert_eq!(state.stream_calls.load(Ordering::SeqCst), initial_calls);
        store
            .call(move |db| {
                db.execute_batch(&format!(
                    "ALTER TABLE unavailable_membership RENAME TO {failed_table}"
                ))?;
                Ok(())
            })
            .await
            .unwrap();
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        assert_eq!(
            state.stream_calls.load(Ordering::SeqCst),
            initial_calls + 1,
            "recovering the {failed_table} read must retain the grant activation lookup"
        );
        assert_eq!(
            *state.last_stream_query.lock().unwrap(),
            "first=100&user_id=1"
        );
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        maintenance.refresh_membership().await.unwrap();
        maintenance.run_pending().await.unwrap();
        assert_eq!(state.stream_calls.load(Ordering::SeqCst), initial_calls + 1);
        server.abort();
    }
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
    production.id = "production-within-cooldown".into();
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
    assert_eq!(
        sends.load(Ordering::SeqCst),
        2,
        "the successful production transaction must gate the next same-requester request"
    );
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
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tracing_subscriber::util::SubscriberInitExt;

    #[derive(Clone)]
    struct LogWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let logs = Arc::new(StdMutex::new(Vec::new()));
    let writer = LogWriter(logs.clone());
    // Keep one dispatcher registered for the test process: concurrent requests
    // can otherwise cache this shared callsite while no scoped subscriber exists.
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .finish()
        .try_init()
        .unwrap();
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
        mods: "HDDT".into(),
    };
    let clock = Clock::new();
    let mut request = request_message();
    request.id = "readable-logs-lookup".into();
    process_one(
        &config, &store, &providers, &echoes, &request, &beatmap, clock,
    )
    .await
    .unwrap();
    request.id = "readable-logs-cache".into();
    process_one(
        &config, &store, &providers, &echoes, &request, &beatmap, clock,
    )
    .await
    .unwrap();

    let output = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    let completions: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["fields"]["message"] == "osu request delivered and recorded")
        .filter(|event| {
            event["fields"]["message_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("readable-logs-"))
        })
        .collect();
    assert_eq!(
        completions.len(),
        2,
        "lookup and cache-hit requests must both log completion"
    );
    for (event, message_id) in completions
        .iter()
        .zip(["readable-logs-lookup", "readable-logs-cache"])
    {
        let fields = &event["fields"];
        assert_eq!(fields["channel"], "streamer");
        assert_eq!(fields["channel_id"], 1);
        assert_eq!(fields["requester"], "viewer");
        assert_eq!(fields["requester_id"], "77");
        assert_eq!(fields["osu_username"], "a");
        assert_eq!(fields["osu_id"], 1);
        assert_eq!(
            fields["map_id"], 99,
            "log the selected map, not input set ID 7 or cached map 42"
        );
        assert_eq!(fields["beatmapset_id"], 7);
        assert_eq!(fields["artist"], "Artist");
        assert_eq!(fields["title"], "Title");
        assert_eq!(fields["difficulty"], "First");
        assert_eq!(fields["stars"], 5.0);
        assert_eq!(fields["mods"], "HDDT");
        assert_eq!(fields["message_id"], message_id);
    }
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

struct RecordSocketPath(Arc<StdMutex<Vec<String>>>);

impl Callback for RecordSocketPath {
    fn on_request(
        self,
        request: &Request,
        response: Response,
    ) -> std::result::Result<Response, ErrorResponse> {
        self.0.lock().unwrap().push(request.uri().to_string());
        Ok(response)
    }
}

struct PoolSockets {
    url: String,
    commands: Arc<StdMutex<HashMap<String, mpsc::Sender<WsMessage>>>>,
    paths: Arc<StdMutex<Vec<String>>>,
    accepted: Arc<AtomicUsize>,
    closed: Arc<AtomicUsize>,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl PoolSockets {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let commands = Arc::new(StdMutex::new(HashMap::new()));
        let paths = Arc::new(StdMutex::new(Vec::new()));
        let accepted = Arc::new(AtomicUsize::new(0));
        let closed = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let (all_commands, all_accepted, all_closed, stopped) = (
            commands.clone(),
            accepted.clone(),
            closed.clone(),
            stop.clone(),
        );
        let all_paths = paths.clone();
        let server = tokio::spawn(async move {
            let mut sockets = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = stopped.cancelled() => break,
                    result = listener.accept() => {
                        let (stream, _) = result.unwrap();
                        let (commands, accepted, closed) =
                            (all_commands.clone(), all_accepted.clone(), all_closed.clone());
                        let paths = all_paths.clone();
                        sockets.spawn(async move {
                            let handshake = tokio_tungstenite::accept_hdr_async(
                                stream,
                                RecordSocketPath(paths),
                            );
                            let mut socket = match handshake.await {
                                Ok(socket) => socket,
                                // Pool cancellation may release an attempt permit
                                // while another queued attempt is opening its socket.
                                // Its TCP connection can close before the upgrade.
                                Err(tokio_tungstenite::tungstenite::Error::Protocol(
                                    tokio_tungstenite::tungstenite::error::ProtocolError::HandshakeIncomplete,
                                )) => return,
                                Err(error) => panic!("unexpected fixture handshake failure: {error}"),
                            };
                            let session = format!("pool-{}", accepted.fetch_add(1, Ordering::SeqCst));
                            let (tx, mut rx) = mpsc::channel::<WsMessage>(16);
                            commands.lock().unwrap().insert(session.clone(), tx);
                            socket.send(WsMessage::text(welcome_json(&session, 60))).await.unwrap();
                            loop {
                                tokio::select! {
                                    command = rx.recv() => {
                                        let Some(command) = command else { break; };
                                        if socket.send(command).await.is_err() { break; }
                                    }
                                    frame = socket.next() => match frame {
                                        Some(Ok(WsMessage::Close(_))) => {
                                            let _ = socket.flush().await;
                                            break;
                                        }
                                        Some(Ok(_)) => {}
                                        _ => break,
                                    }
                                }
                            }
                            commands.lock().unwrap().remove(&session);
                            closed.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                    result = sockets.join_next(), if !sockets.is_empty() => {
                        result.unwrap().unwrap();
                    }
                }
            }
            while let Some(result) = sockets.join_next().await {
                result.unwrap();
            }
        });
        Self {
            url,
            commands,
            paths,
            accepted,
            closed,
            stop,
            server,
        }
    }

    async fn send(&self, session: &str, message: WsMessage) {
        let command = self.commands.lock().unwrap().get(session).unwrap().clone();
        command.send(message).await.unwrap();
    }

    async fn finish(self) {
        eventually(|| self.closed.load(Ordering::SeqCst) == self.accepted.load(Ordering::SeqCst))
            .await;
        assert!(
            self.commands.lock().unwrap().is_empty(),
            "all reader sockets must close"
        );
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap();
    }
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture condition did not converge");
}

fn start_test_pool(
    address: SocketAddr,
    sockets: &PoolSockets,
    remote_count: u32,
    cap: u32,
) -> (super::shards::Pool, watch::Sender<u32>) {
    let mut config = mock_config(address);
    let settings = Arc::get_mut(&mut config).unwrap();
    settings.twitch_eventsub_url = sockets.url.clone();
    settings.eventsub_max_shards = cap;
    settings.http_timeout_secs = 30;
    let shutdown = CancellationToken::new();
    let providers = Providers::new(config.clone(), shutdown.clone()).unwrap();
    let (target, targets) = watch::channel(remote_count);
    let pool = super::shards::Pool::start(
        config,
        providers,
        conduit::Conduit {
            id: "conduit-1".into(),
            shard_count: remote_count,
        },
        targets,
        shutdown,
    );
    (pool, target)
}

#[tokio::test]
async fn persisted_shards_above_the_growth_cap_recover_without_resetting_healthy_peers() {
    let (state, address, helix) = helix_server().await;
    state.shard_count.store(2, Ordering::SeqCst);
    let sockets = PoolSockets::start().await;
    let (mut pool, _target) = start_test_pool(address, &sockets, 2, 1);
    eventually(|| pool.topology.borrow().attached_count == 2).await;
    assert_eq!(pool.topology.borrow().remote_count, 2);
    assert!(
        state.resizes.lock().unwrap().is_empty(),
        "configured cap mismatch must not shrink or replace the conduit"
    );
    let attached = state.attachments.lock().unwrap().clone();
    let failed = attached.iter().find(|(id, _)| *id == 0).unwrap().1.clone();
    let healthy = attached.iter().find(|(id, _)| *id == 1).unwrap().1.clone();
    sockets.send(&failed, WsMessage::Close(None)).await;
    sockets
        .send(
            &healthy,
            WsMessage::text(notification_json(
                "healthy-before-recovery",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    let event = tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event.shard_id, 1,
        "the healthy peer must keep dispatching during recovery"
    );
    assert!(matches!(event.incoming, Incoming::Notification { .. }));
    eventually(|| {
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 0)
            .count()
            >= 2
    })
    .await;
    eventually(|| pool.topology.borrow().attached_count == 2).await;
    assert_eq!(
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 1)
            .count(),
        1
    );
    assert!(state.resizes.lock().unwrap().is_empty());
    pool.shutdown().await;
    sockets.finish().await;
    helix.abort();
}

#[tokio::test]
async fn one_shards_attach_deadline_does_not_delay_or_disconnect_its_peer() {
    let (state, address, helix) = helix_server().await;
    state.shard_count.store(2, Ordering::SeqCst);
    state
        .shard_delays
        .lock()
        .unwrap()
        .insert(0, Duration::from_secs(12));
    let sockets = PoolSockets::start().await;
    let (mut pool, _target) = start_test_pool(address, &sockets, 2, 2);
    eventually(|| pool.topology.borrow().attached_count == 1).await;
    let healthy = state
        .attachments
        .lock()
        .unwrap()
        .iter()
        .find(|(id, _)| *id == 1)
        .unwrap()
        .1
        .clone();
    sockets
        .send(
            &healthy,
            WsMessage::text(notification_json(
                "during-attach",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    let event = tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.shard_id, 1);
    // The first shard's nine-second welcome budget must expire before the
    // fixture's twelve-second HTTP response; the peer remains attached.
    tokio::time::timeout(Duration::from_secs(11), async {
        while sockets.closed.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the independent attach deadline did not close its timed-out socket");
    assert_eq!(pool.topology.borrow().attached_count, 1);
    sockets
        .send(
            &healthy,
            WsMessage::text(notification_json(
                "after-attach-timeout",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    let event = tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.shard_id, 1);
    state.shard_delays.lock().unwrap().clear();
    eventually(|| pool.topology.borrow().attached_count == 2).await;
    assert_eq!(
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 1)
            .count(),
        1
    );
    pool.shutdown().await;
    sockets.finish().await;
    helix.abort();
}

#[tokio::test]
async fn persisted_shard_startup_limits_connection_attempts_to_four() {
    let (state, address, helix) = helix_server().await;
    state.shard_count.store(8, Ordering::SeqCst);
    for id in 0..8 {
        state
            .shard_delays
            .lock()
            .unwrap()
            .insert(id, Duration::from_secs(30));
    }
    let sockets = PoolSockets::start().await;
    let (pool, _target) = start_test_pool(address, &sockets, 8, 8);
    eventually(|| state.attachments.lock().unwrap().len() == 4).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        sockets.accepted.load(Ordering::SeqCst),
        4,
        "waiting welcome/attach attempts must share the four-connection budget"
    );
    assert_eq!(pool.topology.borrow().attached_count, 0);
    pool.shutdown().await;
    sockets.finish().await;
    helix.abort();
}

#[tokio::test]
async fn a_remote_conduit_above_the_hard_bound_is_left_untouched() {
    let (state, address, helix) = helix_server().await;
    state.shard_count.store(65, Ordering::SeqCst);
    let sockets = PoolSockets::start().await;
    let (pool, target) = start_test_pool(address, &sockets, 65, 16);
    target.send_replace(16);
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        *pool.topology.borrow(),
        super::shards::Topology {
            remote_count: 65,
            attached_count: 0
        }
    );
    assert_eq!(sockets.accepted.load(Ordering::SeqCst), 0);
    assert!(state.attachments.lock().unwrap().is_empty());
    assert!(state.resizes.lock().unwrap().is_empty());
    assert_eq!(state.conduit_posts.load(Ordering::SeqCst), 0);
    pool.shutdown().await;
    sockets.finish().await;
    helix.abort();
}

#[tokio::test]
async fn stale_online_actions_do_not_reactivate_after_a_dropped_offline_cancel() {
    let (state, address, helix) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let revision = store.snapshot_versions(vec![1]).await.unwrap()[0].1;
    let (mut maintenance, _targets, actions) =
        maintenance_for_test(config, store.clone(), providers.clone());
    for _ in 0..1024 {
        assert!(
            actions
                .try_send(super::maintenance::Action::Schedule {
                    id: 1,
                    expected_revision: revision,
                })
                .is_ok()
        );
    }
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 3_000)
        .await
        .unwrap();
    assert!(
        matches!(
            actions.try_send(super::maintenance::Action::Cancel { id: 1 }),
            Err(mpsc::error::TrySendError::Full(_))
        ),
        "the bounded action queue must be full so cancellation cannot be delivered"
    );
    maintenance.run_pending().await.unwrap();
    sleep(Duration::from_millis(350)).await;
    maintenance.run_pending().await.unwrap();
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        0,
        "a stale event revision must be discarded before HTTP"
    );
    let expected = [(1, revision)].into_iter().collect();
    assert!(
        super::streams::targeted_at(&store, &providers, &expected)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(state.stream_calls.load(Ordering::SeqCst), 0);
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    helix.abort();
}

#[tokio::test]
async fn late_targeted_http_results_cannot_overwrite_a_newer_offline_event() {
    let (state, address, helix) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    let providers = Providers::new(config, CancellationToken::new()).unwrap();
    let expected = store
        .snapshot_versions(vec![1])
        .await
        .unwrap()
        .into_iter()
        .collect();
    state.hold_streams.store(true, Ordering::SeqCst);
    let lookup_store = store.clone();
    let lookup = tokio::spawn(async move {
        super::streams::targeted_at(&lookup_store, &providers, &expected).await
    });
    eventually(|| state.stream_calls.load(Ordering::SeqCst) == 1).await;
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 3_000)
        .await
        .unwrap();
    state.hold_streams.store(false, Ordering::SeqCst);
    state.release_streams.notify_one();
    assert_eq!(lookup.await.unwrap().unwrap(), vec![1]);
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    helix.abort();
}

async fn delivery_fixture(
    status: axum::http::StatusCode,
) -> (
    Config,
    Store,
    Providers,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let sends = Arc::new(AtomicUsize::new(0));
    let observed = sends.clone();
    let router = Router::new()
        .route(
            "/oauth/token",
            post(|| async { axum::Json(json!({"access_token":"delegate","expires_in":3600})) }),
        )
        .route(
            "/api/v2/chat/new",
            post(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, Ordering::SeqCst);
                    (status, axum::Json(json!({"message_id": 1})))
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
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    store
        .cache_put(json!({
            "id":42,"beatmapset_id":7,"version":"Hard","status":"ranked","bpm":180,
            "difficulty_rating":5.0,"hit_length":125,
            "beatmapset":{"id":7,"artist":"Artist","title":"Title"}
        }))
        .await
        .unwrap();
    (config, store, providers, sends, server)
}

#[tokio::test]
async fn missing_beatmap_is_rejected_without_delivery_or_processing_error() {
    let (config, store, providers, sends, server) =
        delivery_fixture(axum::http::StatusCode::OK).await;
    let (echoes, mut echo_rx) = mpsc::channel(4);
    let beatmap = Beatmap {
        id: 1,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    process_one(
        &config,
        &store,
        &providers,
        &echoes,
        &request_message(),
        &beatmap,
        Clock::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        echo_rx.recv().await.unwrap().text,
        "@viewer Beatmap not found."
    );
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    store
        .call(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM deliveries", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn failed_or_ambiguous_deliveries_never_replay_or_consume_success_bookkeeping() {
    for (response, expected_status) in [
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "uncertain"),
        (axum::http::StatusCode::BAD_REQUEST, "failed"),
    ] {
        let (config, store, providers, sends, server) = delivery_fixture(response).await;
        let (echoes, _echo_rx) = mpsc::channel(4);
        let beatmap = Beatmap {
            id: 42,
            kind: BeatmapKind::Map,
            mods: String::new(),
        };
        let message = request_message();
        let clock = Clock::new();
        assert!(
            process_one(
                &config, &store, &providers, &echoes, &message, &beatmap, clock
            )
            .await
            .is_err()
        );
        process_one(
            &config, &store, &providers, &echoes, &message, &beatmap, clock,
        )
        .await
        .unwrap();
        assert_eq!(
            sends.load(Ordering::SeqCst),
            1,
            "a possibly transmitted request must not replay"
        );
        store
            .call(move |db| {
                assert_eq!(
                    db.query_row(
                        "SELECT status FROM deliveries WHERE twitch_message_id='evt-1'",
                        [],
                        |row| row.get::<_, String>(0),
                    )?,
                    expected_status
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM statistics", [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
                assert_eq!(
                    db.query_row("SELECT count(*) FROM request_cooldowns", [], |row| row
                        .get::<_, i64>(0))?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
        server.abort();
    }
}

#[tokio::test]
async fn success_bookkeeping_is_atomic_when_the_cooldown_write_fails() {
    let (config, store, providers, sends, server) =
        delivery_fixture(axum::http::StatusCode::OK).await;
    store.call(|db| {
        db.execute_batch("CREATE TRIGGER fail_cooldown BEFORE INSERT ON request_cooldowns BEGIN SELECT RAISE(ABORT, 'injected cooldown write failure'); END")?;
        Ok(())
    }).await.unwrap();
    let (echoes, _echo_rx) = mpsc::channel(4);
    let beatmap = Beatmap {
        id: 42,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    let message = request_message();
    let clock = Clock::new();
    assert!(
        process_one(
            &config, &store, &providers, &echoes, &message, &beatmap, clock
        )
        .await
        .is_err()
    );
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='evt-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )?,
                "sending",
                "failed bookkeeping must roll back the sent transition too"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_cooldowns", [], |row| row
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    process_one(
        &config, &store, &providers, &echoes, &message, &beatmap, clock,
    )
    .await
    .unwrap();
    assert_eq!(
        sends.load(Ordering::SeqCst),
        1,
        "a sent request must not replay after its local commit failed"
    );
    server.abort();
}

fn reconnect_json(url: &str) -> String {
    json!({
        "metadata":{"message_type":"session_reconnect"},
        "payload":{"session":{"reconnect_url":url}}
    })
    .to_string()
}

#[tokio::test]
async fn pool_rejects_foreign_reconnects_and_hands_off_without_interrupting_old_or_peer_events() {
    let (state, address, helix) = helix_server().await;
    state.shard_count.store(2, Ordering::SeqCst);
    let sockets = PoolSockets::start().await;
    let foreign = PoolSockets::start().await;
    let (mut pool, _target) = start_test_pool(address, &sockets, 2, 2);
    eventually(|| pool.topology.borrow().attached_count == 2).await;
    let attached = state.attachments.lock().unwrap().clone();
    let old = attached.iter().find(|(id, _)| *id == 0).unwrap().1.clone();
    let peer = attached.iter().find(|(id, _)| *id == 1).unwrap().1.clone();
    sockets
        .send(&old, WsMessage::text(reconnect_json(&foreign.url)))
        .await;
    // Processing the following notification proves the reconnect frame has
    // already been considered, rather than relying only on a short sleep.
    sockets
        .send(
            &old,
            WsMessage::text(notification_json(
                "after-rejected-url",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
            .await
            .unwrap()
            .unwrap()
            .shard_id,
        0
    );
    assert_eq!(foreign.accepted.load(Ordering::SeqCst), 0);
    state
        .shard_delays
        .lock()
        .unwrap()
        .insert(0, Duration::from_millis(500));
    state.shard_failures.lock().unwrap().insert(0, 1);
    sockets
        .send(
            &old,
            WsMessage::text(reconnect_json(&format!("{}?handoff=1", sockets.url))),
        )
        .await;
    eventually(|| {
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 0)
            .count()
            == 2
    })
    .await;
    assert!(
        sockets.commands.lock().unwrap().contains_key(&old),
        "the old socket must remain until the replacement attaches"
    );
    sockets
        .send(
            &old,
            WsMessage::text(notification_json(
                "old-during-handoff",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    sockets
        .send(
            &peer,
            WsMessage::text(notification_json(
                "peer-during-handoff",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    let mut during_handoff = Vec::new();
    for _ in 0..2 {
        during_handoff.push(
            tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
                .await
                .unwrap()
                .unwrap()
                .shard_id,
        );
    }
    during_handoff.sort_unstable();
    assert_eq!(during_handoff, vec![0, 1]);
    eventually(|| sockets.closed.load(Ordering::SeqCst) == 1).await;
    assert!(
        sockets.commands.lock().unwrap().contains_key(&old),
        "a failed replacement must not close the old generation"
    );
    assert_eq!(pool.topology.borrow().attached_count, 2);
    sockets
        .send(
            &old,
            WsMessage::text(notification_json(
                "old-after-failed-handoff",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    sockets
        .send(
            &peer,
            WsMessage::text(notification_json(
                "peer-after-failed-handoff",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    let mut after_failure = Vec::new();
    for _ in 0..2 {
        after_failure.push(
            tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
                .await
                .unwrap()
                .unwrap()
                .shard_id,
        );
    }
    after_failure.sort_unstable();
    assert_eq!(after_failure, vec![0, 1]);
    eventually(|| !sockets.commands.lock().unwrap().contains_key(&old)).await;
    let new = state
        .attachments
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|(id, _)| *id == 0)
        .unwrap()
        .1
        .clone();
    sockets
        .send(
            &new,
            WsMessage::text(notification_json(
                "new-after-handoff",
                "https://osu.ppy.sh/b/42",
                "conduit-1",
            )),
        )
        .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), pool.events.recv())
            .await
            .unwrap()
            .unwrap()
            .shard_id,
        0
    );
    assert_eq!(
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 1)
            .count(),
        1
    );
    assert_eq!(
        state
            .attachments
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| *id == 0)
            .count(),
        3,
        "the failed handoff must retry without waiting for the old socket to close"
    );
    assert_eq!(
        sockets
            .paths
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() == "/ws?handoff=1")
            .count(),
        2,
        "the replacement retry must retain the validated reconnect URL instead of falling back to the base URL"
    );
    pool.shutdown().await;
    sockets.finish().await;
    foreign.finish().await;
    helix.abort();
}

#[tokio::test]
async fn pool_grows_the_owned_topology_one_shard_at_a_time_and_never_shrinks() {
    let (state, address, helix) = helix_server().await;
    let sockets = PoolSockets::start().await;
    let (pool, target) = start_test_pool(address, &sockets, 1, 3);
    eventually(|| pool.topology.borrow().attached_count == 1).await;
    target.send_replace(3);
    eventually(|| {
        *pool.topology.borrow()
            == super::shards::Topology {
                remote_count: 3,
                attached_count: 3,
            }
    })
    .await;
    assert_eq!(*state.resizes.lock().unwrap(), vec![2, 3]);
    let mut attached: Vec<u32> = state
        .attachments
        .lock()
        .unwrap()
        .iter()
        .map(|(id, _)| *id)
        .collect();
    attached.sort_unstable();
    assert_eq!(attached, vec![0, 1, 2]);
    assert_eq!(state.conduit_posts.load(Ordering::SeqCst), 0);
    target.send_replace(1);
    sleep(Duration::from_millis(100)).await;
    assert_eq!(pool.topology.borrow().remote_count, 3);
    assert_eq!(
        *state.resizes.lock().unwrap(),
        vec![2, 3],
        "removing grants must not shrink the remote topology"
    );
    pool.shutdown().await;
    sockets.finish().await;
    helix.abort();
}
