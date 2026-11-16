//! Periodic, durable business-state gauges for Sentry. These are snapshots,
//! not counters: adding repeated samples would multiply account/request totals.
use crate::{storage::BusinessMetrics, storage::Store};
use anyhow::Result;
use sentry::{Hub, metrics::gauge};
use std::{sync::Arc, time::Duration};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;

const INTERVAL: Duration = Duration::from_secs(60);

/// Run alongside the API, independently of bot readiness and incoming requests.
/// The caller only starts this worker when Sentry is configured.
pub async fn run(store: Store, hub: Arc<Hub>, shutdown: CancellationToken) -> Result<()> {
    let mut timer = interval(INTERVAL);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Imported history is immutable in the serving process. Avoid rescanning
    // the historical statistics table on every one-minute snapshot.
    let mut imported_requests = None;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = timer.tick() => {},
        }
        let collected = tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            result = async {
                let imported = match imported_requests {
                    Some(value) => value,
                    None => {
                        let value = store.imported_request_total().await?;
                        imported_requests = Some(value);
                        value
                    }
                };
                Ok::<_, anyhow::Error>((imported, store.business_metrics().await?))
            } => result,
        };
        match collected {
            Ok((imported, snapshot)) => emit(&hub, imported, &snapshot),
            // Missing data must stay missing, not appear as a drop to zero.
            Err(error) => {
                tracing::warn!(error = %error, "Could not collect Sentry business metrics")
            }
        }
    }
}

fn emit(hub: &Hub, imported_requests: i64, snapshot: &BusinessMetrics) {
    for (bot, value) in [
        ("rust", snapshot.live_rust),
        ("legacy", snapshot.live_legacy),
    ] {
        hub.capture_metric(gauge("ronnia.channels.live", value as f64).attribute("bot", bot));
    }
    hub.capture_metric(
        gauge("ronnia.channels.online", snapshot.online_rust as f64).attribute("bot", "rust"),
    );
    for (cohort, value) in [
        ("legacy", snapshot.accounts_legacy),
        ("new", snapshot.accounts_new),
    ] {
        hub.capture_metric(
            gauge("ronnia.accounts.total", value as f64).attribute("cohort", cohort),
        );
    }
    hub.capture_metric(gauge(
        "ronnia.accounts.enabled",
        snapshot.accounts_enabled as f64,
    ));
    for (state, value) in [
        ("pending", snapshot.migration_pending),
        ("authorized", snapshot.migration_authorized),
        ("revoked", snapshot.migration_revoked),
    ] {
        hub.capture_metric(
            gauge("ronnia.accounts.migration", value as f64).attribute("state", state),
        );
    }
    for (state, value) in [
        ("pending", snapshot.authorization_pending),
        ("authorized", snapshot.authorization_authorized),
        ("revoked", snapshot.authorization_revoked),
    ] {
        hub.capture_metric(
            gauge("ronnia.channels.authorization", value as f64).attribute("state", state),
        );
    }
    hub.capture_metric(gauge(
        "ronnia.channels.chat_banned",
        snapshot.chat_banned as f64,
    ));
    for (source, value) in [
        ("imported", imported_requests),
        ("rust", snapshot.requests_rust),
        ("legacy", snapshot.requests_legacy),
    ] {
        hub.capture_metric(
            gauge("ronnia.requests.total", value as f64).attribute("source", source),
        );
    }
    for (window, value) in [
        ("24h", snapshot.active_channels_24h),
        ("7d", snapshot.active_channels_7d),
    ] {
        hub.capture_metric(
            gauge("ronnia.channels.active", value as f64).attribute("window", window),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sentry::{Client, ClientOptions, Envelope, Scope, Transport};
    use serde_json::Value;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Captured(Mutex<Vec<Envelope>>);
    impl Transport for Captured {
        fn send_envelope(&self, envelope: Envelope) {
            self.0.lock().unwrap().push(envelope);
        }
    }

    fn client(mut options: ClientOptions) -> (Arc<Client>, Arc<Hub>, Arc<Captured>) {
        let captured = Arc::new(Captured::default());
        options.dsn = Some("https://public@example.invalid/1".parse().unwrap());
        options.transport = Some(Arc::new(captured.clone()));
        let client = Arc::new(Client::from_config(options));
        let hub = Arc::new(Hub::new(Some(client.clone()), Arc::new(Scope::default())));
        (client, hub, captured)
    }

    #[test]
    fn outgoing_gauges_preserve_cohorts_totals_and_zero_transitions_without_id_labels() {
        let (client, hub, captured) = client(ClientOptions::default());
        let snapshot = BusinessMetrics {
            accounts_legacy: 12,
            accounts_new: 3,
            migration_pending: 8,
            migration_authorized: 3,
            migration_revoked: 1,
            requests_rust: 7,
            requests_legacy: 5,
            live_rust: 2,
            ..BusinessMetrics::default()
        };
        emit(&hub, 100, &snapshot);
        emit(&hub, 100, &BusinessMetrics::default());
        assert!(client.flush(Some(Duration::from_secs(2))));
        let mut metrics = Vec::new();
        for envelope in captured.0.lock().unwrap().iter() {
            let mut bytes = Vec::new();
            envelope.to_writer(&mut bytes).unwrap();
            for line in bytes.split(|byte| *byte == b'\n') {
                if let Ok(value) = serde_json::from_slice::<Value>(line)
                    && let Some(items) = value["items"].as_array()
                {
                    metrics.extend(items.iter().cloned());
                }
            }
        }
        let values = |name: &str, key: &str, label: &str| -> Vec<f64> {
            metrics
                .iter()
                .filter(|metric| {
                    metric["name"] == name && metric["attributes"][key]["value"] == label
                })
                .map(|metric| metric["value"].as_f64().unwrap())
                .collect()
        };
        assert_eq!(
            values("ronnia.accounts.total", "cohort", "legacy"),
            vec![12.0, 0.0]
        );
        assert_eq!(
            values("ronnia.accounts.total", "cohort", "new"),
            vec![3.0, 0.0]
        );
        assert_eq!(
            values("ronnia.accounts.migration", "state", "pending"),
            vec![8.0, 0.0]
        );
        assert_eq!(
            values("ronnia.accounts.migration", "state", "authorized"),
            vec![3.0, 0.0]
        );
        assert_eq!(
            values("ronnia.requests.total", "source", "imported"),
            vec![100.0, 100.0]
        );
        assert_eq!(
            values("ronnia.requests.total", "source", "rust"),
            vec![7.0, 0.0]
        );
        assert_eq!(
            values("ronnia.requests.total", "source", "legacy"),
            vec![5.0, 0.0]
        );
        assert_eq!(
            values("ronnia.channels.live", "bot", "rust"),
            vec![2.0, 0.0]
        );
        for metric in &metrics {
            assert_eq!(metric["type"], "gauge");
            let attributes = metric["attributes"].as_object().unwrap();
            assert!(!attributes.contains_key("twitch_id"));
            assert!(!attributes.contains_key("osu_id"));
            assert!(!attributes.contains_key("stream_id"));
        }
    }

    #[tokio::test]
    async fn worker_reports_database_changes_on_later_ticks_and_stops_on_shutdown() {
        let store = Store::open(":memory:").await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut options = ClientOptions::default();
        options.before_send_metric = Some(Arc::new(move |metric| {
            if metric.name == "ronnia.accounts.total"
                && serde_json::to_value(&metric.attributes).unwrap()["cohort"]["value"] == "new"
            {
                tx.send(metric.value).unwrap();
            }
            None
        }));
        let (_client, hub, _captured) = client(options);
        let shutdown = CancellationToken::new();
        let job = tokio::spawn(run(store.clone(), hub, shutdown.clone()));
        assert_eq!(rx.recv().await.unwrap(), 0.0);
        tokio::time::pause();
        store.call(|db| {
            db.execute("INSERT INTO users(osu_id,osu_username,twitch_id,twitch_username,settings_json) VALUES(1,'player',2,'channel','{\"enable\":true}')", [])?;
            Ok(())
        }).await.unwrap();
        tokio::time::advance(INTERVAL).await;
        assert_eq!(rx.recv().await.unwrap(), 1.0);
        shutdown.cancel();
        job.await.unwrap().unwrap();
    }
}
