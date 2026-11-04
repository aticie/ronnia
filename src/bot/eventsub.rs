//! EventSub over WebSocket with the framing, liveness and reconnect rules from
//! <https://dev.twitch.tv/docs/eventsub/handling-websocket-events/>.

use anyhow::{Context, Result, bail, ensure};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use url::{Host, Url};

/// A validated EventSub message is small; oversized frames are a protocol error.
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 512 * 1024;
/// Time allowed for the TCP/TLS/HTTP upgrade, and separately for the welcome.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);
/// Used when a welcome omits `keepalive_timeout_seconds`.
pub const KEEPALIVE_FALLBACK: Duration = Duration::from_secs(30);

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Clone, Debug)]
pub struct Welcome {
    pub session_id: String,
    pub keepalive: Duration,
}

/// One decoded server frame. `Ignored` covers control frames and unknown
/// message types; ping and pong never extend the keepalive deadline.
#[derive(Debug)]
pub enum Incoming {
    Welcome(Welcome),
    Keepalive,
    Notification {
        subscription: Value,
        event: Value,
        timestamp: String,
        message_id: String,
    },
    Reconnect {
        url: String,
    },
    Revocation {
        subscription: Value,
    },
    Close {
        code: u16,
        reason: String,
    },
    Failed(String),
    Ignored,
}

pub struct Connection {
    socket: Socket,
}

impl Connection {
    pub async fn open(url: &str) -> Result<Self> {
        let parsed = Url::parse(url).context("invalid EventSub WebSocket URL")?;
        match parsed.scheme() {
            "wss" => {}
            "ws" => ensure!(
                is_loopback(&parsed),
                "plain ws EventSub URLs are only accepted for loopback test hosts"
            ),
            scheme => bail!("EventSub WebSocket URL must use ws or wss, not {scheme}"),
        }
        ensure!(parsed.host().is_some(), "EventSub URL omitted a host");
        let config = WebSocketConfig::default()
            .read_buffer_size(16 * 1024)
            .write_buffer_size(0)
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_FRAME_BYTES))
            .accept_unmasked_frames(false);
        let (socket, _) = timeout(
            HANDSHAKE_TIMEOUT,
            connect_async_with_config(url, Some(config), true),
        )
        .await
        .map_err(|_| anyhow::anyhow!("EventSub WebSocket connect timed out"))?
        .context("EventSub WebSocket connect failed")?;
        Ok(Self { socket })
    }

    /// Send a close frame so the server sees a clean shutdown.
    pub async fn close(&mut self) {
        let _ = self.socket.close(None).await;
    }

    /// Read the next message before the application keepalive deadline.
    ///
    /// The caller extends the deadline only for notifications and keepalives;
    /// ping and pong frames are answered by tungstenite but never count as
    /// liveness, exactly as the Twitch protocol requires. A timeout closes the
    /// connection: a partial frame cannot be resumed, and the supervisor
    /// reconnects instead.
    pub async fn read(&mut self, deadline: Instant) -> Result<Incoming> {
        let mut next = std::pin::pin!(self.socket.next());
        let message = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                bail!("EventSub keepalive deadline elapsed with no notification or keepalive")
            }
            message = &mut next => message,
        };
        match message.transpose()? {
            None => bail!("EventSub WebSocket closed without a close frame"),
            Some(Message::Text(text)) => parse(&text),
            Some(Message::Binary(bytes)) => {
                bail!("unexpected binary EventSub frame ({} bytes)", bytes.len())
            }
            // tungstenite answers pings; neither ping nor pong resets the timer.
            Some(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => Ok(Incoming::Ignored),
            Some(Message::Close(frame)) => Ok(Incoming::Close {
                code: frame.as_ref().map_or(1005, |frame| frame.code.into()),
                reason: frame
                    .map(|frame| frame.reason.to_string())
                    .unwrap_or_default(),
            }),
        }
    }
}

fn parse(text: &str) -> Result<Incoming> {
    ensure!(
        text.len() <= MAX_MESSAGE_BYTES,
        "oversized EventSub message"
    );
    let value: Value = serde_json::from_str(text).context("invalid EventSub JSON")?;
    let metadata = &value["metadata"];
    let payload = &value["payload"];
    let message_type = metadata["message_type"]
        .as_str()
        .context("EventSub message omitted metadata.message_type")?;
    Ok(match message_type {
        "session_welcome" => {
            let session = &payload["session"];
            let session_id = session["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .context("welcome omitted the session id")?
                .to_owned();
            let keepalive = session["keepalive_timeout_seconds"]
                .as_u64()
                .filter(|seconds| (10..=600).contains(seconds))
                .map(Duration::from_secs)
                .unwrap_or(KEEPALIVE_FALLBACK);
            Incoming::Welcome(Welcome {
                session_id,
                keepalive,
            })
        }
        "session_keepalive" => Incoming::Keepalive,
        "notification" => Incoming::Notification {
            message_id: metadata["message_id"]
                .as_str()
                .context("notification omitted message_id")?
                .to_owned(),
            timestamp: metadata["message_timestamp"]
                .as_str()
                .context("notification omitted message_timestamp")?
                .to_owned(),
            subscription: payload["subscription"].clone(),
            event: payload["event"].clone(),
        },
        "session_reconnect" => Incoming::Reconnect {
            url: payload["session"]["reconnect_url"]
                .as_str()
                .filter(|url| !url.is_empty())
                .context("reconnect message omitted the reconnect URL")?
                .to_owned(),
        },
        "revocation" => Incoming::Revocation {
            subscription: payload["subscription"].clone(),
        },
        other => {
            tracing::debug!(
                message_type = other,
                "ignoring unknown EventSub message type"
            );
            Incoming::Ignored
        }
    })
}

/// RFC 3339 with nanoseconds, as used by every EventSub timestamp.
pub fn timestamp_nanos(value: &str) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid EventSub timestamp {value:?}"))?
        .timestamp_nanos_opt()
        .context("EventSub timestamp is outside the supported nanosecond range")
}

/// The reconnect URL is used verbatim, but only when it stays on the configured
/// origin; Twitch never redirects EventSub to another host.
pub fn validate_reconnect_url(configured: &str, candidate: &str) -> Result<String> {
    let base = Url::parse(configured).context("invalid configured EventSub URL")?;
    let next = Url::parse(candidate).context("invalid EventSub reconnect URL")?;
    ensure!(
        matches!(next.scheme(), "ws" | "wss"),
        "reconnect URL is not a WebSocket URL"
    );
    ensure!(
        next.scheme() == base.scheme(),
        "reconnect URL changed the transport scheme"
    );
    ensure!(next.host() == base.host(), "reconnect URL changed the host");
    ensure!(
        next.port_or_known_default() == base.port_or_known_default(),
        "reconnect URL changed the port"
    );
    if next.scheme() == "ws" {
        ensure!(
            is_loopback(&next),
            "plain ws reconnect URLs are only accepted for loopback test hosts"
        );
    }
    Ok(candidate.to_owned())
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        // RFC 6761 reserves localhost and its subdomains for loopback use;
        // integration fixtures use names such as recover.localhost to exercise
        // real DNS failure/recovery without weakening plain-ws elsewhere.
        Some(Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
        None => false,
    }
}

/// Capped exponential backoff with jitter for connection-level failures.
pub struct Backoff {
    attempt: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    pub fn next_delay(&mut self) -> Duration {
        let base = 500u64
            .saturating_mul(1u64 << self.attempt.min(7))
            .min(30_000);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(base + jitter(base))
    }
}

fn jitter(base: u64) -> u64 {
    let span = (base / 4).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % span
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_keepalive_and_notification_are_decoded() {
        let welcome = parse(r#"{"metadata":{"message_type":"session_welcome"},"payload":{"session":{"id":"s1","keepalive_timeout_seconds":12,"connected_at":"now"}}}"#).unwrap();
        let Incoming::Welcome(welcome) = welcome else {
            panic!("expected welcome");
        };
        assert_eq!(welcome.session_id, "s1");
        assert_eq!(welcome.keepalive, Duration::from_secs(12));
        let defaulted = parse(r#"{"metadata":{"message_type":"session_welcome"},"payload":{"session":{"id":"s1","keepalive_timeout_seconds":null}}}"#).unwrap();
        assert!(
            matches!(defaulted, Incoming::Welcome(Welcome { keepalive, .. }) if keepalive == KEEPALIVE_FALLBACK)
        );
        let notification = r#"{"metadata":{"message_id":"m1","message_type":"notification","message_timestamp":"2024-01-01T00:00:00.123456789Z"},"payload":{"subscription":{"type":"channel.chat.message"},"event":{"message_id":"x"}}}"#;
        let Incoming::Notification {
            message_id,
            timestamp,
            event,
            ..
        } = parse(notification).unwrap()
        else {
            panic!("expected notification");
        };
        assert_eq!(message_id, "m1");
        assert_eq!(
            timestamp_nanos(&timestamp).unwrap(),
            1_704_067_200_123_456_789
        );
        assert_eq!(event["message_id"], "x");
        assert!(parse("{}").is_err());
        assert!(parse(r#"{"metadata":{"message_type":"notification"},"payload":{}}"#).is_err());
    }

    #[test]
    fn rfc6761_localhost_names_are_loopback_but_other_domains_are_not() {
        for accepted in [
            "ws://localhost/ws",
            "ws://recover.localhost/ws",
            "ws://127.0.0.1:9000/ws",
            "ws://[::1]:9000/ws",
        ] {
            let url = Url::parse(accepted).unwrap();
            assert!(is_loopback(&url), "{accepted} must be loopback");
        }
        for rejected in [
            "ws://attacker.example/ws",
            "ws://notlocalhost/ws",
            "ws://localhost.attacker.example/ws",
            "ws://evil.localhost.example/ws",
        ] {
            let url = Url::parse(rejected).unwrap();
            assert!(!is_loopback(&url), "{rejected} must not be loopback");
        }
        assert!(
            validate_reconnect_url(
                "ws://recover.localhost:9000/ws",
                "ws://recover.localhost:9000/ws?handoff=1",
            )
            .is_ok()
        );
        assert!(
            validate_reconnect_url(
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
            validate_reconnect_url(configured, candidate).unwrap(),
            candidate
        );
        for rejected in [
            "wss://evil.example/ws",
            "ws://eventsub.wss.twitch.tv/ws",
            "wss://eventsub.wss.twitch.tv:8443/ws",
            "https://eventsub.wss.twitch.tv/ws",
        ] {
            assert!(
                validate_reconnect_url(configured, rejected).is_err(),
                "{rejected}"
            );
        }
        assert!(validate_reconnect_url("ws://127.0.0.1:9/ws", "ws://127.0.0.1:9/ws?x=1").is_ok());
        assert!(validate_reconnect_url("ws://127.0.0.1:9/ws", "ws://localhost:9/ws").is_err());
    }

    #[test]
    fn backoff_is_capped_and_monotonic_before_jitter() {
        let mut backoff = Backoff::new();
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
}
