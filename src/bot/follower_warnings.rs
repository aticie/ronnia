//! One durable osu! warning to the streamer when Twitch rejects chat in followers-only mode.

use anyhow::Result;

use crate::{config::Config, providers::Providers, storage::Store};

const TEXT: &str = "Ronnia's Twitch chat confirmations are blocked by followers-only mode. Give ronniabot VIP or moderator status, or disable followers-only mode in Twitch.";

pub(super) async fn send(
    config: &Config,
    store: &Store,
    providers: &Providers,
    channel_id: i64,
) -> Result<()> {
    if config.dry_run {
        return Ok(());
    }
    let Some(claim) = store.claim_follower_warning(channel_id).await? else {
        tracing::debug!(
            channel_id,
            "Followers-only osu! warning not eligible or already attempted this stream"
        );
        return Ok(());
    };
    match providers.send_message(claim.osu_id, TEXT, false).await {
        Ok(()) => {
            super::record_outcome("ronnia.follower_warnings", "sent");
            tracing::info!(
                channel_id,
                target_osu_id = claim.osu_id,
                stream_id = %claim.stream_id,
                "Followers-only osu! warning delivered"
            );
            store
                .finish_follower_warning(channel_id, claim.stream_id, "sent", None)
                .await?;
            Ok(())
        }
        Err(error) => {
            let status = if error.uncertain {
                "uncertain"
            } else {
                "failed"
            };
            super::record_outcome("ronnia.follower_warnings", status);
            let finished = store
                .finish_follower_warning(
                    channel_id,
                    claim.stream_id,
                    status,
                    Some(error.to_string()),
                )
                .await;
            let error = anyhow::Error::new(error);
            match finished {
                Ok(()) => Err(error),
                Err(finish_error) => Err(error.context(format!(
                    "persisting follower warning outcome failed: {finish_error:#}"
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::{Settings, User},
        providers::ChatError,
    };
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    const CHANNEL_ID: i64 = 17;
    const STREAMER_OSU_ID: i64 = 29;

    struct MockState {
        messages: Mutex<Vec<Value>>,
        status: StatusCode,
    }

    async fn token(Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(body["client_id"], "warning-test-client");
        assert_eq!(body["client_secret"], "warning-test-secret");
        assert_eq!(body["grant_type"], "client_credentials");
        assert_eq!(body["scope"], "delegate chat.write");
        Json(json!({"access_token":"warning-test-token","expires_in":3600}))
    }

    async fn chat(
        State(state): State<Arc<MockState>>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        state.messages.lock().unwrap().push(body);
        (
            state.status,
            Json(json!({"message":"warning-test-response"})),
        )
    }

    struct Fixture {
        config: Config,
        store: Store,
        providers: Providers,
        state: Arc<MockState>,
        server: tokio::task::JoinHandle<()>,
        _directory: tempfile::TempDir,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        async fn new(status: StatusCode, dry_run: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let state = Arc::new(MockState {
                messages: Mutex::new(Vec::new()),
                status,
            });
            let router = Router::new()
                .route("/oauth/token", post(token))
                .route("/api/v2/chat/new", post(chat))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let origin = format!("http://{address}");
            let mut config = Config::from_env().unwrap();
            config.database_path = directory.path().join("warnings.sqlite3");
            config.osu_client_id = "warning-test-client".into();
            config.osu_client_secret = "warning-test-secret".into();
            config.osu_api_base = format!("{origin}/api/v2");
            config.osu_oauth_base = format!("{origin}/oauth");
            config.osu_redirect_uri = format!("{origin}/osu-redirect");
            config.twitch_client_id = "warning-test-twitch-client".into();
            config.twitch_client_secret = "warning-test-twitch-secret".into();
            config.twitch_api_base = format!("{origin}/helix");
            config.twitch_oauth_base = format!("{origin}/twitch-oauth");
            config.twitch_redirect_uri = format!("{origin}/twitch-redirect");
            config.twitch_eventsub_url = format!("ws://{address}/eventsub");
            config.twitch_token_key = None;
            config.public_origin = origin.clone();
            config.api_origin = origin;
            config.dry_run = dry_run;
            config.http_timeout_secs = 2;
            config.bot_enabled = true;
            config.migration_active = false;
            config.backup = None;
            let providers =
                Providers::new(Arc::new(config.clone()), CancellationToken::new()).unwrap();
            let store = Store::open(&config.database_path).await.unwrap();
            store
                .save_user(User {
                    osu_id: STREAMER_OSU_ID,
                    osu_username: "streamer-osu".into(),
                    osu_avatar_url: String::new(),
                    twitch_id: CHANNEL_ID,
                    twitch_username: "streamer".into(),
                    twitch_avatar_url: String::new(),
                    excluded_users: Vec::new(),
                    is_live: false,
                    settings: Settings::default(),
                })
                .await
                .unwrap();
            store
                .call(|connection| {
                    connection.execute(
                        "INSERT INTO twitch_channel_grants(twitch_id,granted_at,active_at,status) VALUES(?1,0,0,'granted')",
                        [CHANNEL_ID],
                    )?;
                    connection.execute(
                        "INSERT INTO channel_stream_state(twitch_id,online,stream_id,category_id) VALUES(?1,1,'stream-one','21465')",
                        [CHANNEL_ID],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
            Self {
                config,
                store,
                providers,
                state,
                server,
                _directory: directory,
            }
        }

        async fn send(&self) -> Result<()> {
            send(&self.config, &self.store, &self.providers, CHANNEL_ID).await
        }

        fn messages(&self) -> Vec<Value> {
            self.state.messages.lock().unwrap().clone()
        }

        async fn receipt(&self) -> (String, Option<String>) {
            self.store
                .call(|connection| {
                    Ok(connection.query_row(
                        "SELECT status,error FROM follower_warnings WHERE twitch_id=?1",
                        [CHANNEL_ID],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?)
                })
                .await
                .unwrap()
        }
    }

    #[tokio::test]
    async fn warns_streamer_once_per_stream_across_concurrent_repeats_and_reopen() {
        let mut fixture = Fixture::new(StatusCode::OK, false).await;
        let (first, repeated) = tokio::join!(fixture.send(), fixture.send());
        first.unwrap();
        repeated.unwrap();
        assert_eq!(
            fixture.messages(),
            vec![json!({
                "target_id": STREAMER_OSU_ID,
                "message": "Ronnia's Twitch chat confirmations are blocked by followers-only mode. Give ronniabot VIP or moderator status, or disable followers-only mode in Twitch.",
                "is_action": false,
            })]
        );
        assert_eq!(fixture.receipt().await, ("sent".into(), None));

        fixture.store = Store::open(&fixture.config.database_path).await.unwrap();
        fixture.providers =
            Providers::new(Arc::new(fixture.config.clone()), CancellationToken::new()).unwrap();
        fixture.send().await.unwrap();
        assert_eq!(fixture.messages().len(), 1);

        fixture
            .store
            .call(|connection| {
                connection.execute(
                    "UPDATE channel_stream_state SET stream_id='stream-two' WHERE twitch_id=?1",
                    [CHANNEL_ID],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        fixture.send().await.unwrap();
        fixture.send().await.unwrap();
        assert_eq!(fixture.messages().len(), 2);
        assert_eq!(fixture.receipt().await, ("sent".into(), None));
    }

    #[tokio::test]
    async fn ambiguous_failure_is_durable_and_never_repeated() {
        let mut fixture = Fixture::new(StatusCode::INTERNAL_SERVER_ERROR, false).await;
        let error = fixture.send().await.unwrap_err();
        assert!(error.downcast_ref::<ChatError>().unwrap().uncertain);
        assert_eq!(
            fixture.receipt().await,
            ("uncertain".into(), Some(error.to_string()))
        );
        assert!(error.to_string().contains("warning-test-response"));
        fixture.send().await.unwrap();
        fixture.store = Store::open(&fixture.config.database_path).await.unwrap();
        fixture.send().await.unwrap();
        assert_eq!(fixture.messages().len(), 1);
    }

    #[tokio::test]
    async fn definite_rejection_preserves_error_without_repeating() {
        let fixture = Fixture::new(StatusCode::BAD_REQUEST, false).await;
        let error = fixture.send().await.unwrap_err();
        assert!(!error.downcast_ref::<ChatError>().unwrap().uncertain);
        assert_eq!(
            fixture.receipt().await,
            ("failed".into(), Some(error.to_string()))
        );
        fixture.send().await.unwrap();
        assert_eq!(fixture.messages().len(), 1);
    }

    #[tokio::test]
    async fn dry_run_leaves_no_receipt_for_later_live_send() {
        let mut fixture = Fixture::new(StatusCode::OK, true).await;
        fixture.send().await.unwrap();
        assert!(fixture.messages().is_empty());
        let receipts: i64 = fixture
            .store
            .call(|connection| {
                Ok(
                    connection.query_row("SELECT COUNT(*) FROM follower_warnings", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        fixture.config.dry_run = false;
        fixture.providers =
            Providers::new(Arc::new(fixture.config.clone()), CancellationToken::new()).unwrap();
        fixture.send().await.unwrap();
        assert_eq!(fixture.messages().len(), 1);
        assert_eq!(fixture.receipt().await, ("sent".into(), None));
    }
}
