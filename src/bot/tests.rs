//! Behaviour tests for bot orchestration, dispatch, delivery, maintenance,
//! request scheduling and the EventSub transport.

use super::backoff::Backoff;
use super::conduit::{self, Subscriptions};
use super::delivery::{
    Clock, difficulty_policy, policy, prepare_message, process_one, sanitize_chat,
};
use super::dispatch::{Message, chat_message};
use super::maintenance::{self, Action, Channels, Lookup, Maintenance, Pending, Signal};
use super::requests::{RequestEnvelope, Scheduler};
use super::transport::{self, Connection, Incoming, Link, Reader, Topology};
use super::*;
use crate::{
    domain::{StreamEvent, User},
    parser::{self, Beatmap, BeatmapKind},
    providers::Providers,
    storage::Store,
};
use axum::{
    Router,
    extract::{Form, Query, RawQuery, State},
    response::IntoResponse,
    routing::{get, patch, post},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
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
    let conduit_id = "conduit-1";
    let subscription = json!({
        "type": "channel.chat.message",
        "version": "1",
        "condition": {"broadcaster_user_id": "1", "user_id": "bot"},
        "status": "authorization_revoked",
        "transport": {"method": "conduit", "conduit_id": conduit_id},
    });
    let owned = conduit::owned(&subscription, conduit_id, Some("bot")).expect("owned subscription");
    assert_eq!(owned.kind, "channel.chat.message");
    assert_eq!(owned.broadcaster, 1);
    assert_eq!(owned.user, Some("bot"));

    let mut foreign = subscription.clone();
    foreign["transport"]["conduit_id"] = json!("somebody-else");
    assert!(conduit::owned(&foreign, conduit_id, Some("bot")).is_none());

    let mut old_version = subscription.clone();
    old_version["version"] = json!("2");
    assert!(conduit::owned(&old_version, conduit_id, Some("bot")).is_none());

    let mut wrong_bot = subscription.clone();
    wrong_bot["condition"]["user_id"] = json!("other");
    assert!(conduit::owned(&wrong_bot, conduit_id, Some("bot")).is_none());

    let mut unrelated = subscription;
    unrelated["type"] = json!("channel.follow");
    assert!(conduit::owned(&unrelated, conduit_id, Some("bot")).is_none());
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
    assert!(difficulty_policy(&user, &message, 4.0).is_ok());
    assert!(difficulty_policy(&user, &message, 5.0).is_ok());
    assert!(difficulty_policy(&user, &message, 5.1).is_err());
    user.settings.sr = [4.0, -1.0];
    assert!(difficulty_policy(&user, &message, 9.0).is_ok());
    assert!(difficulty_policy(&user, &message, 3.9).is_err());
    user.settings.sub_only = true;
    assert!(policy(&user, &message).is_err());
    user.settings.sub_only = false;
    user.excluded_users = vec!["VIEWER".into()];
    assert!(policy(&user, &message).is_err());
    user.excluded_users.clear();
    let mut broadcaster = message.clone();
    broadcaster.author = "StReAmEr".into();
    assert!(policy(&user, &broadcaster).is_err());
    user.settings.test = true;
    user.excluded_users = vec!["VIEWER".into()];
    assert!(policy(&user, &message).is_ok());
    assert!(difficulty_policy(&user, &message, 0.0).is_ok());
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
    list_page_size: AtomicUsize,
    pub(super) seeded: StdMutex<Option<Vec<Value>>>,
    deleted: StdMutex<Vec<String>>,
    shard_ok: AtomicBool,
    wrong_shard: AtomicBool,
    validate_mode: AtomicU8,
    validate_calls: AtomicUsize,
    consent_mode: AtomicU8,
    consent_calls: AtomicUsize,
    consent_queries: StdMutex<Vec<Vec<String>>>,
    hold_consent: AtomicBool,
    release_consent: tokio::sync::Notify,
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
    config.twitch_token_key = Some([42; 32]);
    config.twitch_client_secret = "secret".into();
    config.twitch_api_base = format!("http://{address}/helix");
    config.twitch_oauth_base = format!("http://{address}/oauth");
    config.dry_run = false;
    Arc::new(config)
}

pub(super) async fn helix_server() -> (Arc<Helix>, SocketAddr, tokio::task::JoinHandle<()>) {
    async fn token(Form(body): Form<HashMap<String, String>>) -> axum::response::Response {
        match body["grant_type"].as_str() {
            "client_credentials" => {
                axum::Json(json!({"access_token":"app-token","expires_in":3600})).into_response()
            }
            "urn:ietf:params:oauth:grant-type:device_code" => axum::Json(json!({
                "access_token":"bot-token","refresh_token":"refresh","expires_in":14400,
                "scope":crate::twitch_auth::BOT_SCOPES,"token_type":"bearer"
            }))
            .into_response(),
            "refresh_token" => (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(json!({"message":"Invalid refresh token"})),
            )
                .into_response(),
            _ => panic!("unexpected grant type"),
        }
    }
    async fn validate(State(state): State<Arc<Helix>>) -> axum::response::Response {
        state.validate_calls.fetch_add(1, Ordering::SeqCst);
        match state.validate_mode.load(Ordering::SeqCst) {
            0 => axum::Json(json!({
                "client_id":"client","login":"smokebot","user_id":"bot",
                "scopes":crate::twitch_auth::BOT_SCOPES,"expires_in":14400
            }))
            .into_response(),
            1 => (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({"message":"invalid access token"})),
            )
                .into_response(),
            _ => axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response(),
        }
    }
    async fn consent(
        State(state): State<Arc<Helix>>,
        RawQuery(query): RawQuery,
        headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        assert_eq!(headers["authorization"], "Bearer app-token");
        assert_eq!(headers["client-id"], "client");
        let ids = url::form_urlencoded::parse(query.as_deref().unwrap_or_default().as_bytes())
            .map(|(key, value)| {
                assert_eq!(key, "user_id");
                value.into_owned()
            })
            .collect::<Vec<_>>();
        assert!((1..=10).contains(&ids.len()));
        state.consent_queries.lock().unwrap().push(ids.clone());
        state.consent_calls.fetch_add(1, Ordering::SeqCst);
        if state.hold_consent.load(Ordering::SeqCst) {
            state.release_consent.notified().await;
        }
        let mode = state.consent_mode.load(Ordering::SeqCst);
        if mode == 3 {
            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let mut rows = ids
            .into_iter()
            .map(|id| {
                let mut row =
                    json!({"user_id":id,"has_authorized":mode != 1,"scopes":["channel:bot"]});
                match mode {
                    2 => row["scopes"] = json!([]),
                    4 => row["scopes"] = json!([1]),
                    6 => row["user_id"] = json!("999999"),
                    _ => {}
                }
                row
            })
            .collect::<Vec<_>>();
        if mode == 5 {
            rows.clear();
        }
        if mode == 7 {
            rows.push(rows[0].clone());
        }
        axum::Json(json!({"data":rows})).into_response()
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
    async fn list_subscriptions(
        State(state): State<Arc<Helix>>,
        Query(query): Query<HashMap<String, String>>,
    ) -> axum::Json<Value> {
        assert_eq!(
            query.get("conduit_id").map(String::as_str),
            Some("conduit-1")
        );
        assert!(
            !query.contains_key("first"),
            "Twitch does not document a page-size parameter"
        );
        state.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(rows) = state.seeded.lock().unwrap().clone() {
            let page_size = state.list_page_size.load(Ordering::SeqCst);
            if page_size > 0 {
                let start = query
                    .get("after")
                    .map(|value| value.parse::<usize>().unwrap())
                    .unwrap_or(0);
                let end = (start + page_size).min(rows.len());
                let pagination = if end < rows.len() {
                    json!({"cursor":end.to_string()})
                } else {
                    json!({})
                };
                return axum::Json(json!({"data":rows[start..end],"pagination":pagination}));
            }
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
        if let Some(rows) = state.seeded.lock().unwrap().as_mut() {
            rows.retain(|row| row["id"].as_str() != query.get("id").map(String::as_str));
        }
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
        .route("/oauth/validate", get(validate))
        .route("/helix/authorization/users", get(consent))
        .route("/oauth/device", post(|| async {
            axum::Json(json!({
                "device_code":"device","user_code":"CODE","verification_uri":"https://www.twitch.tv/activate",
                "expires_in":60,"interval":1
            }))
        }))
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
    let expected = store
        .snapshot_versions(vec![1])
        .await
        .unwrap()
        .into_iter()
        .collect();
    let skipped = super::maintenance::snapshot(&store, &providers, Some(expected))
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
    super::maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
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
    super::maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
    assert!(
        store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "Osu!Lazer restores eligibility"
    );

    // Just Chatting is observed too, but never grants eligibility.
    state.stream_mode.store(3, Ordering::SeqCst);
    super::maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
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
    let (_, topology) = watch::channel(super::transport::Topology {
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
    super::maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
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
        super::maintenance::snapshot(&store, &providers, None)
            .await
            .unwrap();
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
        "lookup and cache-hit requests must both log completion; captured logs: {output}"
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
) -> (super::transport::Pool, watch::Sender<u32>) {
    let mut config = mock_config(address);
    let settings = Arc::get_mut(&mut config).unwrap();
    settings.twitch_eventsub_url = sockets.url.clone();
    settings.eventsub_max_shards = cap;
    settings.http_timeout_secs = 30;
    let shutdown = CancellationToken::new();
    let providers = Providers::new(config.clone(), shutdown.clone()).unwrap();
    let (target, targets) = watch::channel(remote_count);
    let pool = super::transport::Pool::start(
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
        super::transport::Topology {
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
    let expected: std::collections::BTreeMap<i64, i64> = [(1, revision)].into_iter().collect();
    assert!(
        super::maintenance::snapshot(&store, &providers, Some(expected))
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
        super::maintenance::snapshot(&lookup_store, &providers, Some(expected)).await
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
            == super::transport::Topology {
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

// --- Relocated module unit tests -------------------------------------------
//
// These cover the private logic of the modules that own it; fixtures shared
// with the behaviour tests above live here too.

#[test]
fn desired_contains_state_and_chat_for_every_granted_channel() {
    let rows = conduit::desired(&[1, 2], "bot").unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == "stream.online")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == "channel.update")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.kind == conduit::CHAT_TYPE.0)
            .count(),
        2
    );
    assert!(rows.iter().any(|row| row.kind == conduit::CHAT_TYPE.0
        && row.broadcaster == 2
        && row.user.as_deref() == Some("bot")));
    assert!(
        rows.iter()
            .filter(|row| row.kind == conduit::CHAT_TYPE.0)
            .all(|row| row.user.is_some())
    );
}

#[test]
fn desired_rejects_over_capacity_without_a_partial_set() {
    let grants: Vec<i64> = (1..=conduit::MAX_ACTIVE_GRANTS as i64).collect();
    assert_eq!(
        conduit::desired(&grants, "bot").unwrap().len(),
        conduit::MAX_EVENTS_SUBSCRIPTIONS
    );
    let mut oversized = grants;
    oversized.push(conduit::MAX_ACTIVE_GRANTS as i64 + 1);
    let error = conduit::desired(&oversized, "bot").unwrap_err().to_string();
    assert!(error.contains("2500-grant"));
    assert!(error.contains("without partial reconciliation"));
}

#[test]
fn remote_conduit_requires_positive_valid_topology_without_truncation() {
    let conduit = conduit::parse_conduit(&json!({"id":"owned","shard_count":65})).unwrap();
    assert_eq!(
        conduit.shard_count, 65,
        "pool must observe over-bound topology"
    );
    for row in [
        json!({"id":"owned"}),
        json!({"id":"owned","shard_count":0}),
        json!({"id":"owned","shard_count":-1}),
        json!({"id":"owned","shard_count":"3"}),
        json!({"id":"owned","shard_count":u64::from(u32::MAX) + 1}),
        json!({"id":"","shard_count":1}),
        json!({"shard_count":1}),
    ] {
        assert!(
            conduit::parse_conduit(&row).is_err(),
            "accepted invalid row: {row}"
        );
    }
}

#[test]
fn resize_response_must_match_the_owned_id_and_requested_count() {
    assert!(
        conduit::validate_resize(
            &json!({"data":[{"id":"owned","shard_count":3}]}),
            "owned",
            3
        )
        .is_ok()
    );
    for value in [
        json!({"data":[{"id":"foreign","shard_count":3}]}),
        json!({"data":[{"id":"owned","shard_count":2}]}),
        json!({"data":[{"id":"owned","shard_count":0}]}),
        json!({"data":[{"id":"owned"}]}),
        json!({"data":[]}),
        json!({"data":[{"id":"owned","shard_count":3},{"id":"foreign","shard_count":1}]}),
    ] {
        assert!(conduit::validate_resize(&value, "owned", 3).is_err());
    }
}

#[test]
fn attachment_response_requires_exact_numeric_shard_and_session() {
    let attached = json!({
        "data":[{"id":"12","status":"enabled","transport":{
            "method":"websocket","session_id":"session-new"
        }}]
    });
    assert!(conduit::validate_attachment(&attached, 12, "session-new").is_ok());
    assert!(conduit::validate_attachment(&attached, 0, "session-new").is_err());
    assert!(conduit::validate_attachment(&attached, 12, "session-old").is_err());

    for (field, invalid) in [
        ("id", json!("012")),
        ("id", json!(12)),
        ("status", json!("websocket_disconnected")),
        (
            "transport",
            json!({"method":"websocket","session_id":"other"}),
        ),
        (
            "transport",
            json!({"method":"webhook","session_id":"session-new"}),
        ),
        ("transport", Value::Null),
        ("errors", json!([{"code":"websocket_disconnected"}])),
    ] {
        let mut value = attached.clone();
        value["data"][0][field] = invalid;
        assert!(
            conduit::validate_attachment(&value, 12, "session-new").is_err(),
            "accepted invalid attachment: {value}"
        );
    }
    let mut top_level_error = attached.clone();
    top_level_error["errors"] = json!([{"id":"12","code":"websocket_disconnected"}]);
    assert!(conduit::validate_attachment(&top_level_error, 12, "session-new").is_err());
    let mut duplicate = attached;
    let row = duplicate["data"][0].clone();
    duplicate["data"].as_array_mut().unwrap().push(row);
    assert!(conduit::validate_attachment(&duplicate, 12, "session-new").is_err());
}

#[test]
fn owned_types_ignore_unrelated_subscriptions() {
    assert!(conduit::owned_version("channel.chat.message").is_some());
    assert!(conduit::owned_version("channel.update").is_some());
    assert!(conduit::owned_version("conduit.shard.disabled").is_none());
    assert!(conduit::owned_version("channel.follow").is_none());
}

#[test]
fn subscription_backoff_grows_capped_and_recovers() {
    let now = Instant::now();
    let mut backoff = Backoff::new(Duration::from_secs(60), Duration::from_secs(600));
    assert!(backoff.ready(now));
    backoff.fail();
    assert!(!backoff.ready(now));
    let first = backoff.until().unwrap() - now;
    assert!((Duration::from_secs(60)..=Duration::from_secs(75)).contains(&first));
    backoff.fail();
    assert!(backoff.until().unwrap() - now > first);
    for _ in 0..10 {
        backoff.fail();
    }
    assert!(
        (Duration::from_secs(600)..=Duration::from_secs(750))
            .contains(&(backoff.until().unwrap() - now))
    );
    let until = backoff.until().unwrap();
    assert!(backoff.ready(until));
    backoff.reset();
    assert!(backoff.ready(now));
}

#[test]
fn welcome_keepalive_and_notification_are_decoded() {
    let welcome = transport::parse(r#"{"metadata":{"message_type":"session_welcome"},"payload":{"session":{"id":"s1","keepalive_timeout_seconds":12,"connected_at":"now"}}}"#).unwrap();
    let Incoming::Welcome(welcome) = welcome else {
        panic!("expected welcome");
    };
    assert_eq!(welcome.session_id, "s1");
    assert_eq!(welcome.keepalive, Duration::from_secs(12));
    let defaulted = transport::parse(r#"{"metadata":{"message_type":"session_welcome"},"payload":{"session":{"id":"s1","keepalive_timeout_seconds":null}}}"#).unwrap();
    assert!(
        matches!(defaulted, Incoming::Welcome(welcome) if welcome.keepalive == Duration::from_secs(30))
    );
    let notification = r#"{"metadata":{"message_id":"m1","message_type":"notification","message_timestamp":"2024-01-01T00:00:00.123456789Z"},"payload":{"subscription":{"type":"channel.chat.message"},"event":{"message_id":"x"}}}"#;
    let Incoming::Notification {
        message_id,
        timestamp,
        event,
        ..
    } = transport::parse(notification).unwrap()
    else {
        panic!("expected notification");
    };
    assert_eq!(message_id, "m1");
    assert_eq!(
        transport::timestamp_nanos(&timestamp).unwrap(),
        1_704_067_200_123_456_789
    );
    assert_eq!(event["message_id"], "x");
    assert!(transport::parse("{}").is_err());
    assert!(
        transport::parse(r#"{"metadata":{"message_type":"notification"},"payload":{}}"#).is_err()
    );
}

#[test]
fn rfc6761_localhost_names_are_loopback_but_other_domains_are_not() {
    for accepted in [
        "ws://localhost/ws",
        "ws://recover.localhost/ws",
        "ws://127.0.0.1:9000/ws",
        "ws://[::1]:9000/ws",
    ] {
        let url = url::Url::parse(accepted).unwrap();
        assert!(transport::is_loopback(&url), "{accepted} must be loopback");
    }
    for rejected in [
        "ws://attacker.example/ws",
        "ws://notlocalhost/ws",
        "ws://localhost.attacker.example/ws",
        "ws://evil.localhost.example/ws",
    ] {
        let url = url::Url::parse(rejected).unwrap();
        assert!(
            !transport::is_loopback(&url),
            "{rejected} must not be loopback"
        );
    }
    assert!(
        transport::validate_reconnect_url(
            "ws://recover.localhost:9000/ws",
            "ws://recover.localhost:9000/ws?handoff=1",
        )
        .is_ok()
    );
    assert!(
        transport::validate_reconnect_url(
            "ws://recover.localhost:9000/ws",
            "ws://attacker.example:9000/ws",
        )
        .is_err()
    );
}

#[test]
fn reconnect_must_stay_on_the_configured_origin() {
    let configured = "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30";
    let candidate = "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30&session=abc";
    assert_eq!(
        transport::validate_reconnect_url(configured, candidate).unwrap(),
        candidate
    );
    for rejected in [
        "wss://evil.example/ws",
        "ws://eventsub.wss.twitch.tv/ws",
        "wss://eventsub.wss.twitch.tv:8443/ws",
        "https://eventsub.wss.twitch.tv/ws",
    ] {
        assert!(
            transport::validate_reconnect_url(configured, rejected).is_err(),
            "{rejected}"
        );
    }
    assert!(
        transport::validate_reconnect_url("ws://127.0.0.1:9/ws", "ws://127.0.0.1:9/ws?x=1").is_ok()
    );
    assert!(
        transport::validate_reconnect_url("ws://127.0.0.1:9/ws", "ws://localhost:9/ws").is_err()
    );
}

#[test]
fn connection_backoff_is_capped_and_monotonic_before_jitter() {
    let mut backoff = Backoff::new(Duration::from_millis(500), Duration::from_secs(30));
    let mut previous = Duration::ZERO;
    for _ in 0..12 {
        let delay = backoff.next_delay();
        assert!(delay >= previous.min(Duration::from_secs(30)));
        assert!(delay <= Duration::from_millis(37_500));
        previous = delay;
    }
    assert!(backoff.next_delay() <= Duration::from_millis(37_500));
    backoff.reset();
    assert!(backoff.next_delay() < Duration::from_secs(1));
}

#[test]
fn growth_waits_for_every_observed_shard_and_never_shrinks() {
    let mut state = transport::State {
        remote_count: 3,
        attached: std::collections::BTreeSet::from([0, 1]),
        uncertain: false,
    };
    assert_eq!(state.candidate(5, 8), None);
    state.set_attached(2, true);
    assert_eq!(state.candidate(5, 8), Some(3));
    assert_eq!(state.candidate(1, 8), None);
    assert_eq!(
        state.topology(),
        Topology {
            remote_count: 3,
            attached_count: 3
        }
    );
    state.observe(4);
    assert_eq!(state.candidate(5, 8), None);
    // A failed attachment retains the committed remote count and blocks the
    // next growth step until the missing shard actually recovers.
    state.set_attached(3, false);
    assert_eq!(state.remote_count, 4);
    state.set_attached(3, true);
    assert_eq!(state.candidate(5, 8), Some(4));
    state.uncertain = true;
    assert_eq!(state.candidate(5, 8), None);
    assert_eq!(state.topology().attached_count, 0);
    assert_eq!(state.attached.len(), 4);
}

#[test]
fn persisted_count_above_cap_is_attached_but_above_hard_bound_is_not() {
    let mut state = transport::State {
        remote_count: 4,
        attached: std::collections::BTreeSet::new(),
        uncertain: false,
    };
    for id in 0..4 {
        state.set_attached(id, true);
    }
    assert_eq!(state.topology().attached_count, 4);
    assert_eq!(state.candidate(8, 2), None);
    state.observe(65);
    state.set_attached(0, true);
    assert_eq!(
        state.topology(),
        Topology {
            remote_count: 65,
            attached_count: 0
        }
    );
    assert_eq!(state.candidate(100, 100), None);
}

#[tokio::test]
async fn welcome_deadlines_are_independent_and_cover_the_whole_operation() {
    let first_welcome = Instant::now() - transport::SHARD_ATTACH_DEADLINE;
    let second_welcome = Instant::now();
    let (expired, healthy) = tokio::join!(
        transport::attach_before(first_welcome + transport::SHARD_ATTACH_DEADLINE, async {
            std::future::pending::<anyhow::Result<()>>().await
        }),
        transport::attach_before(second_welcome + transport::SHARD_ATTACH_DEADLINE, async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(())
        }),
    );
    assert!(expired.unwrap_err().to_string().contains("nine seconds"));
    assert!(healthy.is_ok());
    let deadline = Instant::now() + Duration::from_millis(10);
    let result = transport::attach_before(deadline, async {
        tokio::task::yield_now().await;
        std::future::pending::<anyhow::Result<()>>().await
    })
    .await;
    assert!(result.is_err());
}

struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn reader_close_joins_cooperative_reader() {
    let dropped = Arc::new(AtomicBool::new(false));
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let guard = Dropped(dropped.clone());
    let handle = tokio::spawn(async move {
        let _guard = guard;
        token.cancelled().await;
    });
    Reader { handle, cancel }.close().await;
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn reader_close_aborts_and_joins_uncooperative_reader() {
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Dropped(dropped.clone());
    let handle = tokio::spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    Reader {
        handle,
        cancel: CancellationToken::new(),
    }
    .close()
    .await;
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn link_shutdown_cancels_and_joins_all_readers() {
    let mut link = Link::new();
    let mut flags = Vec::new();
    for id in 0..4 {
        let dropped = Arc::new(AtomicBool::new(false));
        let guard = Dropped(dropped.clone());
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            let _guard = guard;
            token.cancelled().await;
        });
        link.readers.insert(id, Reader { handle, cancel });
        flags.push(dropped);
    }
    link.shutdown().await;
    assert!(link.readers.is_empty());
    assert!(flags.iter().all(|flag| flag.load(Ordering::SeqCst)));
}

// --- Maintenance unit tests ------------------------------------------------

#[tokio::test]
async fn overflow_coalesces_snapshot_without_losing_later_revisions() {
    let (actions, mut queue) = maintenance::actions();
    let (signals, mut revisions) = watch::channel(Signal::default());
    let captured = *revisions.borrow_and_update();
    for id in 0..maintenance::TARGET_CAPACITY {
        maintenance::enqueue(
            &actions,
            &signals,
            Action::Schedule {
                id: id as i64,
                expected_revision: 7,
            },
        );
    }
    maintenance::enqueue(&actions, &signals, Action::Cancel { id: 0 });
    let overflow = *revisions.borrow_and_update();
    assert_eq!(overflow.snapshot_revision, captured.snapshot_revision + 1);
    assert_eq!(overflow.audit_revision, captured.audit_revision);
    assert_eq!(
        queue.len(),
        maintenance::TARGET_CAPACITY,
        "overflow never expands the queue"
    );
    // Completing an operation at the captured revision cannot acknowledge the
    // overflow obligation or a later independent revocation.
    maintenance::request(&signals, true, false);
    let later = *revisions.borrow_and_update();
    assert!(later.snapshot_revision > captured.snapshot_revision);
    assert!(later.revision > overflow.revision);
    assert!(later.audit_revision > overflow.audit_revision);
    for id in 0..maintenance::TARGET_CAPACITY {
        assert!(matches!(queue.recv().await,
            Some(Action::Schedule { id: queued, expected_revision: 7 }) if queued == id as i64));
    }
    assert!(queue.try_recv().is_err());
}

#[test]
fn pending_debounces_bounds_and_preserves_retry_revision() {
    let mut pending = Pending::default();
    assert!(pending.schedule(1, 10));
    assert!(pending.take_due().is_empty());
    let original = pending.due.remove(&1).unwrap();
    assert!(original.due > Instant::now());
    assert!(pending.retry(1, original));
    let retry = pending.due.get(&1).unwrap();
    assert_eq!(retry.attempt, 1);
    assert_eq!(
        retry.expected,
        Some(10),
        "retry may not recapture an offline revision"
    );
    assert!(retry.due > Instant::now());
    for id in 2..=conduit::MAX_ACTIVE_GRANTS as i64 {
        assert!(pending.schedule(id, 1));
    }
    assert!(!pending.schedule(conduit::MAX_ACTIVE_GRANTS as i64 + 1, 1));
    assert!(
        pending.schedule(1, 11),
        "a newer online revision supersedes existing work"
    );
    assert_eq!(pending.due[&1].expected, Some(11));
    assert_eq!(pending.due.len(), conduit::MAX_ACTIVE_GRANTS);
}

#[tokio::test]
async fn failed_online_lookup_retry_does_not_adopt_newer_offline_revision() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Online, 100)
        .await
        .unwrap();
    let online_revision = store.snapshot_versions(vec![1]).await.unwrap()[0].1;
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (_signals, signal_rx) = watch::channel(Signal::default());
    let (action_tx, action_rx) = maintenance::actions();
    let (targets, _target_rx) = watch::channel(1);
    let (_topology_tx, topology) = watch::channel(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    let mut instance = Maintenance::new(
        config,
        store.clone(),
        providers,
        "conduit-1".into(),
        "bot".into(),
        Channels {
            membership: store.channel_changes(),
            signals: signal_rx,
            actions: action_rx,
            targets,
            topology,
        },
    );
    // This is the exact event-triggered retry retained after a failed HTTP
    // lookup. Its deadline is due, but its baseline is still the online event.
    instance.pending.insert(
        1,
        Lookup {
            due: Instant::now(),
            attempt: 1,
            expected: Some(online_revision),
        },
    );
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 200)
        .await
        .unwrap();
    instance.run_pending().await.unwrap();
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        0,
        "a stale retry is discarded before issuing HTTP, not rebound to offline"
    );
    assert!(instance.pending.due.is_empty());
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    // Ordered cancellation also removes a scheduled online lookup outright.
    action_tx
        .try_send(Action::Schedule {
            id: 1,
            expected_revision: online_revision,
        })
        .unwrap();
    action_tx.try_send(Action::Cancel { id: 1 }).unwrap();
    instance.drain_actions();
    assert!(instance.pending.due.is_empty());
    server.abort();
}

#[tokio::test]
async fn completed_creation_pass_requires_snapshot_after_an_intermediate_correction() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    store
        .call(|db| {
            db.execute(
            "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
            [],
        )?;
            Ok(())
        })
        .await
        .unwrap();
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (_signals, signal_rx) = watch::channel(Signal::default());
    let (_actions, action_rx) = maintenance::actions();
    let (targets, _target_rx) = watch::channel(1);
    let (_topology_tx, topology) = watch::channel(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    let mut instance = Maintenance::new(
        config,
        store.clone(),
        providers.clone(),
        "conduit-1".into(),
        "bot".into(),
        Channels {
            membership: store.channel_changes(),
            signals: signal_rx,
            actions: action_rx,
            targets,
            topology,
        },
    );
    instance.subscriptions_reconciled(&conduit::Summary {
        created: 200,
        deferred: 4,
        ..Default::default()
    });
    // A useful correction can run between paced creation passes, but this
    // broadcaster's subscriptions have not all been established yet.
    state.stream_mode.store(1, Ordering::SeqCst);
    maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
    instance.snapshot_pending = false;
    assert!(instance.snapshot_after_reconcile);
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);

    // Twitch changes state before the final subscriptions are created, so no
    // notification can be relied on for that transition.
    state.stream_mode.store(0, Ordering::SeqCst);
    instance.subscriptions_reconciled(&conduit::Summary {
        created: 4,
        deferred: 0,
        ..Default::default()
    });
    assert!(
        instance.snapshot_pending,
        "the intermediate snapshot must not consume the post-subscription correction"
    );
    assert!(!instance.snapshot_after_reconcile);
    maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
    instance.snapshot_pending = false;
    assert!(
        store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "the final correction repairs the otherwise missed transition"
    );
    assert_eq!(state.stream_calls.load(Ordering::SeqCst), 2);
    instance.subscriptions_reconciled(&conduit::Summary::default());
    assert!(
        !instance.snapshot_pending,
        "ordinary no-change reconciliations must not continuously poll streams"
    );
    server.abort();
}

#[tokio::test]
async fn single_pass_grant_activation_uses_only_its_targeted_correction() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (_signals, signals) = watch::channel(Signal::default());
    let (_actions, actions) = maintenance::actions();
    let (targets, _target_rx) = watch::channel(1);
    let (_topology_tx, topology) = watch::channel(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    // The shared fixture already owns a chat row for user "bot". Use a
    // different managed identity so this activation creates all four desired
    // subscriptions rather than retaining that existing row.
    let bot_id = "activation-bot";
    let mut instance = Maintenance::new(
        config,
        store.clone(),
        providers.clone(),
        "conduit-1".into(),
        bot_id.into(),
        Channels {
            membership: store.channel_changes(),
            signals,
            actions,
            targets,
            topology,
        },
    );
    // An imported enabled account is already known at startup, without a
    // grant. Complete its offline startup correction before activation.
    state.stream_mode.store(1, Ordering::SeqCst);
    instance.refresh_membership().await.unwrap();
    instance.run_pending().await.unwrap();
    instance.subscriptions_reconciled(&conduit::Summary::default());
    maintenance::snapshot(&store, &providers, None)
        .await
        .unwrap();
    instance.snapshot_pending = false;
    let baseline = state.stream_calls.load(Ordering::SeqCst);
    store
        .call(|db| {
            db.execute(
            "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
            [],
        )?;
            Ok(())
        })
        .await
        .unwrap();
    state.stream_mode.store(0, Ordering::SeqCst);
    instance.refresh_membership().await.unwrap();
    let desired = conduit::desired(instance.grants.as_ref().unwrap(), bot_id).unwrap();
    let summary = instance
        .subs
        .reconcile(&providers, &desired, true, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.created, 4);
    assert_eq!(summary.deferred, 0);
    instance.subscriptions_reconciled(&summary);
    assert!(
        !instance.snapshot_pending,
        "a normal creation pass must not duplicate the grant-activation lookup"
    );
    instance.run_pending().await.unwrap();
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    assert_eq!(state.stream_calls.load(Ordering::SeqCst), baseline + 1);
    instance.refresh_membership().await.unwrap();
    instance.run_pending().await.unwrap();
    instance.subscriptions_reconciled(&conduit::Summary::default());
    assert!(!instance.snapshot_pending);
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        baseline + 1,
        "unrelated membership refreshes must not repeat the correction"
    );
    server.abort();
}

/// Retry a cheap observation with a bounded timeout instead of sleeping for a
/// guessed duration.
async fn poll_until(mut condition: impl FnMut() -> bool, what: &str) {
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("bounded wait for {what} expired");
}

#[tokio::test]
async fn session_recovery_audits_and_snapshots_without_reprovisioning_subscriptions() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    store
        .call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // The channel was fully subscribed before its shard dropped. Only the live
    // transition happened while disconnected, so the recovery re-list must find
    // every desired subscription already established.
    *state.seeded.lock().unwrap() = Some(vec![
        json!({"id":"state-online","status":"enabled","type":"stream.online","version":"1","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
        json!({"id":"state-offline","status":"enabled","type":"stream.offline","version":"1","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
        json!({"id":"state-update","status":"enabled","type":"channel.update","version":"2","condition":{"broadcaster_user_id":"1"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
        json!({"id":"chat-1","status":"enabled","type":"channel.chat.message","version":"1","condition":{"broadcaster_user_id":"1","user_id":"bot"},"transport":{"method":"conduit","conduit_id":"conduit-1"}}),
    ]);
    state.stream_mode.store(1, Ordering::SeqCst);
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (signals, signal_rx) = watch::channel(Signal::default());
    let (_actions, action_rx) = maintenance::actions();
    let (targets, _target_rx) = watch::channel(1);
    let (topology_tx, topology) = watch::channel(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    let instance = Maintenance::new(
        config,
        store.clone(),
        providers,
        "conduit-1".into(),
        "bot".into(),
        Channels {
            membership: store.channel_changes(),
            signals: signal_rx,
            actions: action_rx,
            targets,
            topology,
        },
    );
    let shutdown = CancellationToken::new();
    let worker = tokio::spawn(instance.run(shutdown.clone()));
    // Converged: startup membership/audit kept the seeded subscriptions, then
    // the full snapshot and the activation lookup completed.
    poll_until(
        || {
            state.list_calls.load(Ordering::SeqCst) >= 1
                && state.stream_calls.load(Ordering::SeqCst) >= 2
        },
        "maintenance startup convergence",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let audited = state.list_calls.load(Ordering::SeqCst);
    let snapshots = state.stream_calls.load(Ordering::SeqCst);
    assert_eq!(
        (audited, snapshots),
        (1, 2),
        "startup ran one audit, the full snapshot and the activation lookup"
    );
    assert_eq!(
        (
            state.creates.load(Ordering::SeqCst),
            state.deletes.load(Ordering::SeqCst)
        ),
        (0, 0),
        "startup must reuse the established subscriptions instead of provisioning them"
    );
    assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);

    // The shard drops while the provider goes live, then reattaches. The
    // foreground topology observer requests exactly this audit and snapshot.
    topology_tx.send_replace(Topology {
        remote_count: 1,
        attached_count: 0,
    });
    state.stream_mode.store(0, Ordering::SeqCst);
    topology_tx.send_replace(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    maintenance::request(&signals, true, true);

    let mut corrected = false;
    for _ in 0..600 {
        if store.user_by_twitch(1).await.unwrap().unwrap().is_live {
            corrected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        corrected,
        "bounded wait for the recovery snapshot correction expired"
    );
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("maintenance loop stops when cancelled")
        .unwrap();

    // The recovery re-listed the owned subscriptions, found them complete and
    // provisioned nothing: a topology/session change never recreates or deletes
    // subscriptions. Exactly one guarded full snapshot repaired the transition
    // that was missed while disconnected.
    assert_eq!(
        state.list_calls.load(Ordering::SeqCst),
        audited + 1,
        "the recovery signal must run the remote audit"
    );
    assert_eq!(
        state.creates.load(Ordering::SeqCst),
        0,
        "recovery must not recreate existing owned subscriptions"
    );
    assert_eq!(
        state.deletes.load(Ordering::SeqCst),
        0,
        "recovery must not delete existing owned subscriptions"
    );
    assert_eq!(
        state.stream_calls.load(Ordering::SeqCst),
        snapshots + 1,
        "exactly one revision-guarded full snapshot"
    );
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
    server.abort();
}

// --- Request scheduler unit tests ------------------------------------------

use std::pin::Pin;

type Work = Pin<Box<dyn Future<Output = ()> + Send>>;

struct Controlled {
    gates: std::collections::BTreeMap<String, tokio::sync::oneshot::Receiver<bool>>,
    started: mpsc::UnboundedSender<String>,
    finished: mpsc::UnboundedSender<String>,
}

impl Controlled {
    fn work(&mut self, message: Message, _: Beatmap) -> Work {
        let gate = self
            .gates
            .remove(&message.id)
            .expect("one start per request");
        let started = self.started.clone();
        let finished = self.finished.clone();
        Box::pin(async move {
            started.send(message.id.clone()).unwrap();
            let panic = gate.await.expect("test must release active work");
            assert!(!panic, "controlled worker panic");
            finished.send(message.id).unwrap();
        })
    }
}

struct Fixture {
    budget: Arc<tokio::sync::Semaphore>,
    control: Controlled,
    gates: std::collections::BTreeMap<String, tokio::sync::oneshot::Sender<bool>>,
    started: mpsc::UnboundedReceiver<String>,
    finished: mpsc::UnboundedReceiver<String>,
}

impl Fixture {
    fn new(capacity: usize) -> Self {
        let (started_tx, started) = mpsc::unbounded_channel();
        let (finished_tx, finished) = mpsc::unbounded_channel();
        Self {
            budget: Arc::new(tokio::sync::Semaphore::new(capacity)),
            control: Controlled {
                gates: std::collections::BTreeMap::new(),
                started: started_tx,
                finished: finished_tx,
            },
            gates: std::collections::BTreeMap::new(),
            started,
            finished,
        }
    }

    fn request(&mut self, channel: i64, id: &str) -> RequestEnvelope {
        let (release, gate) = tokio::sync::oneshot::channel();
        self.gates.insert(id.to_owned(), release);
        self.control.gates.insert(id.to_owned(), gate);
        RequestEnvelope {
            message: Message {
                id: id.to_owned(),
                channel: format!("channel{channel}"),
                channel_id: channel,
                author: "requester".into(),
                author_id: "100".into(),
                moderator: false,
                subscriber: false,
                vip: false,
                points: false,
            },
            beatmap: Beatmap {
                id: 1,
                kind: BeatmapKind::Map,
                mods: String::new(),
            },
            permit: self.budget.clone().try_acquire_owned().unwrap(),
            span: tracing::Span::none(),
            queued_at: Instant::now(),
        }
    }

    fn release(&mut self, id: &str, panic: bool) {
        self.gates.remove(id).unwrap().send(panic).unwrap();
    }

    async fn started(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.started.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn finished(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(5), self.finished.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

fn fill(scheduler: &mut Scheduler, fixture: &mut Fixture, workers: usize) {
    while scheduler.jobs.len() < workers {
        let Some(RequestEnvelope {
            message,
            beatmap,
            permit,
            span,
            queued_at,
        }) = scheduler.next()
        else {
            break;
        };
        let channel = message.channel_id;
        let work = fixture.control.work(message, beatmap);
        scheduler.spawn(channel, permit, span, queued_at, work);
    }
}

async fn collect(scheduler: &mut Scheduler) {
    let result = tokio::time::timeout(Duration::from_secs(5), scheduler.jobs.join_next_with_id())
        .await
        .unwrap()
        .unwrap();
    scheduler.complete(result);
}

fn assert_empty(scheduler: &Scheduler) {
    assert!(scheduler.pending.is_empty());
    assert!(scheduler.ready.is_empty());
    assert!(scheduler.active.is_empty());
    assert!(scheduler.jobs.is_empty());
    assert!(scheduler.channels_by_task.is_empty());
}

#[tokio::test]
async fn one_budget_bounds_workers_pending_and_uncollected_results() {
    let mut fixture = Fixture::new(32);
    let mut scheduler = Scheduler::default();
    for index in 0..32 {
        scheduler.admit(fixture.request(index % 4, &format!("r{index}")));
    }
    assert!(fixture.budget.clone().try_acquire_owned().is_err());
    fill(&mut scheduler, &mut fixture, 4);
    let mut first = std::collections::BTreeSet::new();
    for _ in 0..4 {
        first.insert(fixture.started().await);
    }
    assert_eq!(
        first,
        std::collections::BTreeSet::from(["r0".into(), "r1".into(), "r2".into(), "r3".into()])
    );
    assert!(fixture.started.try_recv().is_err());
    assert_eq!(scheduler.active.len(), 4);
    assert_eq!(scheduler.jobs.len(), 4);
    assert_eq!(
        scheduler
            .pending
            .values()
            .map(std::collections::VecDeque::len)
            .sum::<usize>(),
        28
    );
    fixture.release("r1", false);
    assert_eq!(fixture.finished().await, "r1");
    assert_eq!(
        fixture.budget.available_permits(),
        0,
        "completed but uncollected jobs still own admission"
    );
    collect(&mut scheduler).await;
    assert_eq!(fixture.budget.available_permits(), 1);
    fill(&mut scheduler, &mut fixture, 4);
    assert_eq!(
        fixture.started().await,
        "r5",
        "another channel progresses while r0 is held"
    );
    assert!(
        fixture.started.try_recv().is_err(),
        "same-channel successors cannot overlap"
    );
    scheduler.discard_pending();
    assert_eq!(fixture.budget.available_permits(), 28);
    for id in ["r0", "r2", "r3", "r5"] {
        fixture.release(id, false);
    }
    while !scheduler.jobs.is_empty() {
        collect(&mut scheduler).await;
    }
    assert_empty(&scheduler);
    assert_eq!(fixture.budget.available_permits(), 32);
}

#[tokio::test]
async fn channel_fifo_is_serial_and_ready_channels_rotate_fairly() {
    let mut fixture = Fixture::new(5);
    let mut scheduler = Scheduler::default();
    for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1"), (2, "b2"), (3, "c1")] {
        scheduler.admit(fixture.request(channel, id));
    }
    // Defensive stale entries cannot resurrect empty channel state.
    scheduler.ready.push_front(999);
    for expected in ["a1", "b1", "c1", "a2", "b2"] {
        fill(&mut scheduler, &mut fixture, 1);
        assert_eq!(fixture.started().await, expected);
        assert!(fixture.started.try_recv().is_err());
        fixture.release(expected, false);
        collect(&mut scheduler).await;
    }
    assert_empty(&scheduler);
    assert_eq!(fixture.budget.available_permits(), 5);
}

#[tokio::test]
async fn panicked_worker_releases_channel_and_admission_without_blocking_peers() {
    let mut fixture = Fixture::new(3);
    let mut scheduler = Scheduler::default();
    for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1")] {
        scheduler.admit(fixture.request(channel, id));
    }
    fill(&mut scheduler, &mut fixture, 2);
    let mut starts = std::collections::BTreeSet::new();
    starts.insert(fixture.started().await);
    starts.insert(fixture.started().await);
    assert_eq!(
        starts,
        std::collections::BTreeSet::from(["a1".into(), "b1".into()])
    );
    fixture.release("a1", true);
    collect(&mut scheduler).await;
    assert_eq!(fixture.budget.available_permits(), 1);
    assert_eq!(scheduler.active, std::collections::BTreeSet::from([2]));
    assert_eq!(scheduler.channels_by_task.len(), 1);
    fill(&mut scheduler, &mut fixture, 2);
    assert_eq!(fixture.started().await, "a2");
    fixture.release("a2", false);
    fixture.release("b1", false);
    collect(&mut scheduler).await;
    collect(&mut scheduler).await;
    assert_empty(&scheduler);
    assert_eq!(fixture.budget.available_permits(), 3);
}

#[tokio::test]
async fn cancelled_task_clears_active_channel_and_skips_stale_ready_entries() {
    let mut fixture = Fixture::new(2);
    let mut scheduler = Scheduler::default();
    scheduler.admit(fixture.request(1, "a1"));
    scheduler.admit(fixture.request(1, "a2"));
    fill(&mut scheduler, &mut fixture, 2);
    assert_eq!(fixture.started().await, "a1");
    scheduler.ready.push_front(1);
    fill(&mut scheduler, &mut fixture, 2);
    assert!(fixture.started.try_recv().is_err());
    assert_eq!(scheduler.jobs.len(), 1);
    assert!(scheduler.ready.is_empty());
    scheduler.jobs.abort_all();
    collect(&mut scheduler).await;
    assert!(scheduler.active.is_empty());
    assert!(scheduler.channels_by_task.is_empty());
    assert_eq!(fixture.budget.available_permits(), 1);
    fill(&mut scheduler, &mut fixture, 2);
    assert_eq!(fixture.started().await, "a2");
    fixture.release("a2", false);
    collect(&mut scheduler).await;
    assert_empty(&scheduler);
    assert_eq!(fixture.budget.available_permits(), 2);
}

#[tokio::test]
async fn shutdown_closes_ingress_discards_pending_and_joins_active_work() {
    let mut fixture = Fixture::new(4);
    let (tx, mut rx) = mpsc::channel(4);
    for (channel, id) in [(1, "a1"), (1, "a2"), (2, "b1"), (2, "b2")] {
        tx.try_send(fixture.request(channel, id))
            .unwrap_or_else(|_| panic!("queue has capacity"));
    }
    let mut scheduler = Scheduler::default();
    scheduler.admit(rx.try_recv().unwrap());
    let Some(RequestEnvelope {
        message,
        beatmap,
        permit,
        span,
        queued_at,
    }) = scheduler.next()
    else {
        panic!("the first request must be runnable");
    };
    let channel = message.channel_id;
    let work = fixture.control.work(message, beatmap);
    scheduler.spawn(channel, permit, span, queued_at, work);
    assert_eq!(fixture.started().await, "a1");
    while let Ok(request) = rx.try_recv() {
        scheduler.admit(request);
    }
    assert_eq!(fixture.budget.available_permits(), 0);
    let drain =
        tokio::spawn(async move { super::requests::drain_ingress(&mut rx, &mut scheduler).await });
    tokio::time::timeout(Duration::from_secs(5), tx.closed())
        .await
        .unwrap();
    assert!(tx.is_closed());
    assert_eq!(fixture.budget.available_permits(), 3);
    assert!(
        !drain.is_finished(),
        "shutdown must join rather than cancel active bookkeeping"
    );
    assert!(fixture.started.try_recv().is_err());
    fixture.gates.remove("a1").unwrap().send(false).unwrap();
    tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.budget.available_permits(), 4);
    assert!(fixture.started.try_recv().is_err());
}

// --- Stream snapshot revision guard ---------------------------------------

#[tokio::test]
async fn revision_guard_prevents_a_slow_snapshot_from_reactivating() {
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    store
        .call(|db| {
            db.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(1,0,'granted')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Online, 1_000)
        .await
        .unwrap();
    store
        .apply_stream_event(
            1,
            "streamer".into(),
            StreamEvent::Category("21465".into()),
            2_000,
        )
        .await
        .unwrap();
    assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);

    let expected = store
        .snapshot_versions(vec![1])
        .await
        .unwrap()
        .into_iter()
        .collect();
    // A newer offline event lands while the HTTP snapshot is in flight.
    store
        .apply_stream_event(1, "streamer".into(), StreamEvent::Offline, 3_000)
        .await
        .unwrap();
    let skipped = maintenance::apply(
        &store,
        &[1],
        &expected,
        vec![crate::providers::LiveStream {
            id: 1,
            login: "streamer".into(),
            category_id: "21465".into(),
        }],
    )
    .await
    .unwrap();
    assert_eq!(skipped, vec![1], "the stale snapshot must report a retry");
    assert!(
        !store.user_by_twitch(1).await.unwrap().unwrap().is_live,
        "a slow snapshot must not reactivate an offline channel"
    );
}

#[test]
fn notification_guard_drops_redeliveries_and_stays_bounded() {
    use super::dispatch::Seen;

    let mut seen = Seen::default();
    let huge = "m".repeat(1024 * 1024);
    assert!(seen.insert(&huge), "first delivery is new");
    assert!(!seen.insert(&huge), "a redelivered id must be dropped");
    assert!(seen.insert(""), "an absent id is never deduplicated");

    for index in 0..2_050u32 {
        assert!(seen.insert(&format!("m{index}")));
    }
    assert!(
        !seen.insert(&format!("m{}", 2_049)),
        "the most recent id is still remembered"
    );
    assert!(
        seen.insert("m0"),
        "the oldest ids are evicted once the bound is reached"
    );
}

#[tokio::test]
async fn disabled_owned_subscriptions_are_replaced_without_touching_foreign_rows() {
    let (state, address, server) = helix_server().await;
    let disabled = json!({
        "id":"disabled","status":"authorization_revoked","type":"channel.chat.message","version":"1",
        "condition":{"broadcaster_user_id":"1","user_id":"bot"},
        "transport":{"method":"conduit","conduit_id":"conduit-1"}
    });
    let mut foreign = disabled.clone();
    foreign["id"] = json!("foreign-disabled");
    foreign["transport"]["conduit_id"] = json!("foreign");
    *state.seeded.lock().unwrap() = Some(vec![disabled.clone(), foreign]);
    assert!(
        conduit::owned(&disabled, "conduit-1", Some("bot")).is_some(),
        "disabled status is not part of subscription identity: revocations must parse"
    );
    let providers = Providers::new(mock_config(address), CancellationToken::new()).unwrap();
    let mut subscriptions = Subscriptions::new("conduit-1".into(), "bot".into());
    let desired = vec![conduit::Desired {
        kind: conduit::CHAT_TYPE.0,
        version: conduit::CHAT_TYPE.1,
        broadcaster: 1,
        user: Some("bot".into()),
    }];
    let summary = subscriptions
        .reconcile(&providers, &desired, true, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((summary.kept, summary.removed, summary.created), (0, 1, 1));
    assert_eq!(*state.deleted.lock().unwrap(), vec!["disabled"]);
    server.abort();
}

#[tokio::test]
async fn chat_revocation_requires_authoritative_broadcaster_consent_loss() {
    for mode in 0..=7 {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        grant_and_go_live(&store, 1, "streamer").await;
        state.consent_mode.store(mode, Ordering::SeqCst);
        let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
        let (mut instance, _, actions) = maintenance_for_test(config, store.clone(), providers);
        for _ in 0..32 {
            actions
                .try_send(Action::Revoked {
                    id: 1,
                    expected_revision: 1,
                })
                .unwrap();
        }
        instance.drain_actions();
        assert_eq!(
            state.consent_calls.load(Ordering::SeqCst),
            0,
            "foreground admission must never perform HTTP"
        );
        let result = instance.check_revocations(&CancellationToken::new()).await;
        if mode != 3 {
            assert_eq!(
                state.consent_calls.load(Ordering::SeqCst),
                1,
                "one burst must coalesce into one authoritative lookup"
            );
        }
        if matches!(mode, 3 | 4 | 6 | 7) {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
        if matches!(mode, 1 | 2) {
            assert!(store.granted_twitch_ids().await.unwrap().is_empty());
            assert!(!store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        } else {
            assert_eq!(
                store.granted_twitch_ids().await.unwrap(),
                vec![1],
                "bot-wide revocation, unavailable or malformed consent must not revoke a broadcaster"
            );
            assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        }
        if mode >= 3 {
            state.consent_mode.store(1, Ordering::SeqCst);
            instance
                .check_revocations(&CancellationToken::new())
                .await
                .unwrap();
            assert!(
                store.granted_twitch_ids().await.unwrap().is_empty(),
                "inconclusive lookups must retain the queued attribution for retry"
            );
        }
        server.abort();
    }
}

#[tokio::test]
async fn delivery_reservation_is_exercised_by_concurrent_real_requests() {
    let (config, store, providers, sends, server) =
        delivery_fixture(axum::http::StatusCode::OK).await;
    let mut user = store.user_by_twitch(1).await.unwrap().unwrap();
    user.settings.test = true; // Do not let cooldown hide a broken uniqueness gate.
    store.save_user(user).await.unwrap();
    let (echoes, _rx) = mpsc::channel(4);
    let message = request_message();
    let beatmap = Beatmap {
        id: 42,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    let clock = Clock::new();
    let (first, second) = tokio::join!(
        process_one(
            &config, &store, &providers, &echoes, &message, &beatmap, clock
        ),
        process_one(
            &config, &store, &providers, &echoes, &message, &beatmap, clock
        ),
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM deliveries WHERE status='sent'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    server.abort();
}

#[tokio::test]
async fn interrupted_real_delivery_is_recovered_by_schema_and_never_replayed() {
    let (config, fixture_store, providers, sends, server) =
        delivery_fixture(axum::http::StatusCode::OK).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delivery.sqlite");
    let store = Store::open(&path).await.unwrap();
    seed_user(&store, 1, "streamer").await;
    grant_and_go_live(&store, 1, "streamer").await;
    store
        .cache_put(fixture_store.cache_get(42).await.unwrap().unwrap())
        .await
        .unwrap();
    store.call(|db| {
        db.execute_batch("CREATE TRIGGER stop_bookkeeping BEFORE INSERT ON statistics BEGIN SELECT RAISE(ABORT, 'process interrupted before commit'); END")?;
        Ok(())
    }).await.unwrap();
    let (echoes, _rx) = mpsc::channel(4);
    let message = request_message();
    let beatmap = Beatmap {
        id: 42,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    assert!(
        process_one(
            &config,
            &store,
            &providers,
            &echoes,
            &message,
            &beatmap,
            Clock::new()
        )
        .await
        .is_err()
    );
    drop(store);
    let recovered = Store::open(&path).await.unwrap();
    recovered
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='evt-1'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "uncertain"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
    process_one(
        &config,
        &recovered,
        &providers,
        &echoes,
        &message,
        &beatmap,
        Clock::new(),
    )
    .await
    .unwrap();
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn bot_startup_preserves_pending_legacy_reports() {
    let store = Store::open(":memory:").await.unwrap();
    seed_user(&store, 1, "streamer").await;
    assert_eq!(
        store
            .claim_legacy_delivery("legacy-pending".into(), 1, 1, "hello".into())
            .await
            .unwrap(),
        crate::storage::ClaimOutcome::Claimed
    );
    store.call(|db| {
        db.execute("INSERT INTO deliveries(twitch_message_id,target_osu_id,message,status,created_at,source) VALUES('new-pending',1,'hello','pending',0,'new')", [])?;
        Ok(())
    }).await.unwrap();
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let mut config = Config::from_env().unwrap();
    config.twitch_client_id = "client".into();
    config.twitch_token_key = Some([42; 32]);
    let config = Arc::new(config);
    let auth = TwitchAuth::new(config.clone(), store.clone()).unwrap();
    super::run(config, store.clone(), auth, shutdown)
        .await
        .unwrap();
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='legacy-pending'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "pending"
            );
            assert_eq!(
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='new-pending'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "failed"
            );
            Ok(())
        })
        .await
        .unwrap();
    let statistics = serde_json::from_value(json!({
        "requesterChannelName":"viewer","requestedBeatmapId":42,
        "requestedChannelName":"streamer","mods":"","timestamp":1
    }))
    .unwrap();
    assert_eq!(
        store
            .complete_legacy_delivery("legacy-pending".into(), true, None, Some(statistics))
            .await
            .unwrap(),
        crate::storage::DeliveryOutcome::Recorded
    );
    store
        .call(|db| {
            assert_eq!(
                db.query_row(
                    "SELECT status FROM deliveries WHERE twitch_message_id='legacy-pending'",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "sent"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM statistics", [], |r| r
                    .get::<_, i64>(0))?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn metadata_independent_policy_rejects_before_404_fetch_or_echo() {
    let (mut config, store, _providers, sends, server) =
        delivery_fixture(axum::http::StatusCode::OK).await;
    let fetches = Arc::new(AtomicUsize::new(0));
    let observed = fetches.clone();
    let router = Router::new().route(
        "/api/v2/beatmaps/{id}",
        get(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::NOT_FOUND
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config.osu_api_base = format!("http://{}/api/v2", listener.local_addr().unwrap());
    let metadata_server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let providers = Providers::new(Arc::new(config.clone()), CancellationToken::new()).unwrap();
    let baseline = store.user_by_twitch(1).await.unwrap().unwrap();
    let beatmap = Beatmap {
        id: 404,
        kind: BeatmapKind::Map,
        mods: String::new(),
    };
    let (echoes, mut rx) = mpsc::channel(4);
    for restriction in 0..4 {
        let mut settings = baseline.settings.clone();
        let mut message = request_message();
        match restriction {
            0 => {}
            1 => settings.sub_only = true,
            2 => settings.points_only = true,
            _ => message.author = "streamer".into(),
        }
        store.set_settings(1, settings).await.unwrap();
        store
            .set_excluded(1, "viewer".into(), restriction == 0)
            .await
            .unwrap();
        process_one(
            &config,
            &store,
            &providers,
            &echoes,
            &message,
            &beatmap,
            Clock::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            fetches.load(Ordering::SeqCst),
            0,
            "restriction {restriction}"
        );
        assert!(
            rx.try_recv().is_err(),
            "policy rejection must not produce a misleading not-found echo"
        );
    }
    for test_mode in [false, true] {
        let mut settings = baseline.settings.clone();
        settings.test = test_mode;
        if test_mode {
            settings.sub_only = true;
            settings.points_only = true;
        }
        store.set_settings(1, settings).await.unwrap();
        store
            .set_excluded(1, "viewer".into(), test_mode)
            .await
            .unwrap();
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
        assert_eq!(rx.try_recv().unwrap().text, "@viewer Beatmap not found.");
    }
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert_eq!(sends.load(Ordering::SeqCst), 0);
    metadata_server.abort();
    server.abort();
}

#[tokio::test]
async fn conduit_audit_handles_unspecified_small_pages_beyond_one_hundred() {
    let (state, address, server) = helix_server().await;
    state.list_page_size.store(1, Ordering::SeqCst);
    *state.seeded.lock().unwrap() = Some((1..=101).map(|id| json!({
        "id":format!("owned-{id}"),"status":"enabled","type":"stream.online","version":"1",
        "condition":{"broadcaster_user_id":id.to_string()},
        "transport":{"method":"conduit","conduit_id":"conduit-1"}
    })).collect());
    let desired = (1..=101)
        .map(|id| conduit::Desired {
            kind: "stream.online",
            version: "1",
            broadcaster: id,
            user: None,
        })
        .collect::<Vec<_>>();
    let providers = Providers::new(mock_config(address), CancellationToken::new()).unwrap();
    let mut subscriptions = Subscriptions::new("conduit-1".into(), "bot".into());
    let summary = subscriptions
        .reconcile(&providers, &desired, false, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.kept, 101);
    assert_eq!(state.list_calls.load(Ordering::SeqCst), 101);
    assert_eq!(
        (
            state.creates.load(Ordering::SeqCst),
            state.deletes.load(Ordering::SeqCst)
        ),
        (0, 0)
    );
    server.abort();
}

#[tokio::test]
async fn channel_reauthorization_while_queued_or_validating_survives_revocation() {
    for during_http in [false, true] {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        grant_and_go_live(&store, 1, "streamer").await;
        let shutdown = CancellationToken::new();
        state.consent_mode.store(1, Ordering::SeqCst);
        let (_, signals) = watch::channel(Signal::default());
        let (action_tx, actions) = maintenance::actions();
        let (targets, _) = watch::channel(1);
        let (_, topology) = watch::channel(Topology {
            remote_count: 1,
            attached_count: 1,
        });
        let providers = Providers::new(config.clone(), shutdown.clone()).unwrap();
        let mut instance = Maintenance::new(
            config,
            store.clone(),
            providers,
            "conduit-1".into(),
            "bot".into(),
            Channels {
                membership: store.channel_changes(),
                signals,
                actions,
                targets,
                topology,
            },
        );
        action_tx
            .try_send(Action::Revoked {
                id: 1,
                expected_revision: 1,
            })
            .unwrap();
        instance.drain_actions();
        if !during_http {
            store.call(|db| {
            db.execute("UPDATE twitch_channel_grants SET revision=revision+1,status='granted' WHERE twitch_id=1", [])?;
            Ok(())
        }).await.unwrap();
        }
        let before = state.consent_calls.load(Ordering::SeqCst);
        state.hold_consent.store(true, Ordering::SeqCst);
        let check = tokio::spawn(async move { instance.check_revocations(&shutdown).await });
        eventually(|| state.consent_calls.load(Ordering::SeqCst) > before).await;
        // Even identical grant timestamps represent a new consent generation.
        if during_http {
            store.call(|db| {
        db.execute("UPDATE twitch_channel_grants SET revision=revision+1,status='granted' WHERE twitch_id=1", [])?;
        Ok(())
    }).await.unwrap();
        }
        state.hold_consent.store(false, Ordering::SeqCst);
        state.release_consent.notify_one();
        check.await.unwrap().unwrap();
        assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![1]);
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        server.abort();
    }
}

#[tokio::test]
async fn bot_revocation_or_reauthorization_preserves_authorized_broadcaster() {
    for restored in [false, true] {
        let (state, address, server) = helix_server().await;
        let config = mock_config(address);
        let store = Store::open(":memory:").await.unwrap();
        seed_user(&store, 1, "streamer").await;
        grant_and_go_live(&store, 1, "streamer").await;
        let shutdown = CancellationToken::new();
        TwitchAuth::authorize_bot(
            config.clone(),
            store.clone(),
            "smokebot",
            false,
            shutdown.clone(),
        )
        .await
        .unwrap();
        state.validate_mode.store(1, Ordering::SeqCst);
        if restored {
            state.validate_mode.store(0, Ordering::SeqCst);
            TwitchAuth::authorize_bot(
                config.clone(),
                store.clone(),
                "smokebot",
                false,
                shutdown.clone(),
            )
            .await
            .unwrap();
        }
        let before = state.validate_calls.load(Ordering::SeqCst);
        let providers = Providers::new(config.clone(), shutdown.clone()).unwrap();
        let (mut instance, _, actions) = maintenance_for_test(config, store.clone(), providers);
        actions
            .try_send(Action::Revoked {
                id: 1,
                expected_revision: 1,
            })
            .unwrap();
        instance.drain_actions();
        instance.check_revocations(&shutdown).await.unwrap();
        assert_eq!(
            state.validate_calls.load(Ordering::SeqCst),
            before,
            "old revocation envelopes must be attributed using broadcaster consent, not current bot health"
        );
        assert_eq!(store.granted_twitch_ids().await.unwrap(), vec![1]);
        assert!(store.user_by_twitch(1).await.unwrap().unwrap().is_live);
        server.abort();
    }
}

#[tokio::test]
async fn authorization_batches_remain_bounded_and_omitted_users_do_not_starve_others() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let (mut instance, _, actions) = maintenance_for_test(config, store, providers);
    for id in 1..=11 {
        actions
            .try_send(Action::Revoked {
                id,
                expected_revision: 1,
            })
            .unwrap();
    }
    instance.drain_actions();
    state.consent_mode.store(5, Ordering::SeqCst);
    instance
        .check_revocations(&CancellationToken::new())
        .await
        .unwrap();
    state.consent_mode.store(0, Ordering::SeqCst);
    instance
        .check_revocations(&CancellationToken::new())
        .await
        .unwrap();
    let queries = state.consent_queries.lock().unwrap().clone();
    assert_eq!(queries[0].len(), 10);
    assert_eq!(queries[1].len(), 10);
    assert!(
        queries[1].contains(&"11".to_owned()),
        "retained early users must not permanently starve later inbox entries"
    );
    server.abort();
}

#[tokio::test]
async fn full_active_audit_capacity_still_removes_disabled_rows_without_partial_cache() {
    let (state, address, server) = helix_server().await;
    state.list_page_size.store(1, Ordering::SeqCst);
    let first = json!({
        "id":"active-1","status":"enabled","type":"stream.online","version":"1",
        "condition":{"broadcaster_user_id":"1"},
        "transport":{"method":"conduit","conduit_id":"conduit-1"}
    });
    let mut second = first.clone();
    second["id"] = json!("active-2");
    second["condition"]["broadcaster_user_id"] = json!("2");
    let mut disabled = first.clone();
    disabled["id"] = json!("disabled");
    disabled["status"] = json!("authorization_revoked");
    // Same production path as 10,000 enabled rows plus a disabled leftover.
    // Deleting the middle row shifts the index cursor and requires a fresh list.
    *state.seeded.lock().unwrap() = Some(vec![first, disabled, second]);
    let providers = Providers::new(mock_config(address), CancellationToken::new()).unwrap();
    let mut subscriptions = Subscriptions::new("conduit-1".into(), "bot".into());
    assert_eq!(
        subscriptions
            .refresh_remote_bounded(&providers, &CancellationToken::new(), 2)
            .await
            .unwrap(),
        2
    );
    let desired = (1..=2)
        .map(|id| conduit::Desired {
            kind: "stream.online",
            version: "1",
            broadcaster: id,
            user: None,
        })
        .collect::<Vec<_>>();
    let summary = subscriptions
        .reconcile(&providers, &desired, true, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!((summary.kept, summary.removed, summary.created), (2, 1, 0));
    assert_eq!(*state.deleted.lock().unwrap(), vec!["disabled"]);
    assert_eq!(
        state.creates.load(Ordering::SeqCst),
        0,
        "cleanup pagination must never publish a partial enabled cache"
    );
    server.abort();
}

#[tokio::test]
async fn revocation_action_overflow_rescans_every_supported_grant_without_losing_consent_updates() {
    let (state, address, server) = helix_server().await;
    let config = mock_config(address);
    let store = Store::open(":memory:").await.unwrap();
    store.call(|db| {
        let transaction = db.transaction()?;
        for id in 1..=conduit::MAX_ACTIVE_GRANTS as i64 {
            transaction.execute(
                "INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(?1,'osu',?1,?2,'{\"enable\":true}')",
                rusqlite::params![id, format!("streamer{id}")],
            )?;
            transaction.execute(
                "INSERT INTO twitch_channel_grants(twitch_id,granted_at,status) VALUES(?1,0,'granted')",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }).await.unwrap();
    let (signals, signal_rx) = watch::channel(Signal::default());
    let before = *signal_rx.borrow();
    let (actions, action_rx) = maintenance::actions();
    for id in 1..=conduit::MAX_ACTIVE_GRANTS as i64 {
        maintenance::enqueue(
            &actions,
            &signals,
            Action::Revoked {
                id,
                expected_revision: 1,
            },
        );
    }
    assert_eq!(
        action_rx.len(),
        maintenance::TARGET_CAPACITY,
        "the foreground action queue must remain bounded"
    );
    assert!(
        signal_rx.borrow().consent_revision > before.consent_revision,
        "overflow must preserve an authoritative full-consent-scan obligation"
    );
    assert_eq!(
        signal_rx.borrow().snapshot_revision,
        before.snapshot_revision
    );
    assert_eq!(state.consent_calls.load(Ordering::SeqCst), 0);
    let (targets, _) = watch::channel(1);
    let (_, topology) = watch::channel(Topology {
        remote_count: 1,
        attached_count: 1,
    });
    let providers = Providers::new(config.clone(), CancellationToken::new()).unwrap();
    let mut instance = Maintenance::new(
        config,
        store.clone(),
        providers,
        "conduit-1".into(),
        "bot".into(),
        Channels {
            membership: store.channel_changes(),
            signals: signal_rx,
            actions: action_rx,
            targets,
            topology,
        },
    );
    instance.drain_actions();
    instance.require_consent_rescan();
    instance.rescan_revocations().await.unwrap();
    state.consent_mode.store(1, Ordering::SeqCst);
    for _ in 0..conduit::MAX_ACTIVE_GRANTS.div_ceil(10) {
        instance.rescan_revocations().await.unwrap();
        instance
            .check_revocations(&CancellationToken::new())
            .await
            .unwrap();
    }
    assert!(
        store.granted_twitch_ids().await.unwrap().is_empty(),
        "even overflowed revocations must produce durable consent updates at full supported capacity"
    );
    assert!(
        state
            .consent_queries
            .lock()
            .unwrap()
            .iter()
            .all(|ids| ids.len() <= 10)
    );
    server.abort();
}
