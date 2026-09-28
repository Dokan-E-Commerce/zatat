use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fred::clients::SubscriberClient;
use fred::prelude::*;
use fred::types::{PerformanceConfig, ReconnectPolicy, ServerConfig};
use tokio::sync::broadcast;
use tracing::{info, warn};

use zatat_config::RedisConfig;

use crate::provider::PubSubProvider;

pub struct RedisPubSubProvider {
    subscribe_channel: String,
    publisher: RedisClient,
    tx: broadcast::Sender<Vec<u8>>,
    _subscriber: Arc<SubscriberClient>,
}

impl RedisPubSubProvider {
    pub async fn connect(cfg: &RedisConfig, channel: String) -> Result<Arc<Self>, String> {
        let builder = build_builder(cfg).map_err(|e| format!("redis config: {e}"))?;
        let publisher: RedisClient = builder
            .build()
            .map_err(|e| format!("redis publisher build: {e}"))?;
        let subscriber: SubscriberClient = builder
            .build_subscriber_client()
            .map_err(|e| format!("redis subscriber build: {e}"))?;

        watch_connection_events(&publisher, "publisher");
        // No `on_error` listener on the subscriber: with one attached, fred
        // 9.4 can end the subscriber's reader without scheduling a reconnect
        // (reproduced by the Redis-restart scenario). The watchdog below
        // reports the subscriber's health instead, and also catches that.
        drop(subscriber.on_reconnect(|server| {
            metrics::counter!("zatat_redis_reconnects_total", "client" => "subscriber")
                .increment(1);
            info!(client = "subscriber", %server, "redis connected");
            Ok(())
        }));
        publisher
            .init()
            .await
            .map_err(|e| format!("redis publisher init: {e}"))?;
        subscriber
            .init()
            .await
            .map_err(|e| format!("redis subscriber init: {e}"))?;

        subscriber
            .subscribe(channel.clone())
            .await
            .map_err(|e| format!("redis subscribe: {e}"))?;
        let _resubscribe_task = subscriber.manage_subscriptions();
        tokio::spawn(subscriber_watchdog(subscriber.clone()));

        // Oversized on purpose — a single slow EventDispatcher consumer on
        // the receiving end shouldn't lose messages during a traffic burst.
        let (tx, _rx) = broadcast::channel::<Vec<u8>>(16_384);
        let mut message_rx = subscriber.message_rx();
        let tx_clone = tx.clone();
        let bridge_channel = channel.clone();
        tokio::spawn(async move {
            loop {
                match message_rx.recv().await {
                    Ok(message) => {
                        if message.channel != bridge_channel.as_str() {
                            continue;
                        }
                        let bytes_opt: Option<Vec<u8>> = match &message.value {
                            RedisValue::Bytes(b) => Some(b.to_vec()),
                            RedisValue::String(s) => Some(s.as_bytes().to_vec()),
                            _ => None,
                        };
                        if let Some(b) = bytes_opt {
                            if tx_clone.send(b).is_err() {
                                tracing::debug!("redis bridge: no receivers");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Upstream (fred's internal broadcast) dropped frames faster
                        // than this task could consume. broadcast_channel_capacity in
                        // build_builder() should be large enough that this never fires.
                        warn!(skipped = n, "fred message_rx lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("redis message channel closed, bridge exiting");
                        return;
                    }
                }
            }
        });

        info!(%channel, "redis pub/sub connected");
        Ok(Arc::new(Self {
            subscribe_channel: channel,
            publisher,
            tx,
            _subscriber: Arc::new(subscriber),
        }))
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[async_trait]
impl PubSubProvider for RedisPubSubProvider {
    async fn publish(&self, payload: Vec<u8>) {
        let channel = self.subscribe_channel.clone();
        let value = RedisValue::Bytes(payload.into());
        let res: Result<i64, RedisError> = self.publisher.publish(channel, value).await;
        if let Err(err) = res {
            metrics::counter!("zatat_redis_publish_failures_total").increment(1);
            warn!(%err, "redis publish failed");
        }
    }

    async fn subscribe(&self) -> Result<broadcast::Receiver<Vec<u8>>, String> {
        Ok(self.tx.subscribe())
    }
}

const SUBSCRIBER_PING_INTERVAL: Duration = Duration::from_secs(5);
const SUBSCRIBER_PING_TIMEOUT: Duration = Duration::from_secs(3);
/// Consecutive failed pings before the subscriber is forcibly reconnected.
const SUBSCRIBER_MAX_FAILED_PINGS: u32 = 2;

/// A pub/sub connection can die without the client noticing (a half-open
/// TCP connection, or a reconnect that never gets scheduled), and then
/// silently receives nothing from other nodes. PING it periodically; after
/// repeated failures force a reconnect, which re-subscribes via
/// `manage_subscriptions`.
async fn subscriber_watchdog(subscriber: SubscriberClient) {
    let mut tick = tokio::time::interval(SUBSCRIBER_PING_INTERVAL);
    tick.tick().await;
    let mut failures = 0u32;
    loop {
        tick.tick().await;
        // In subscribed mode Redis answers PING with an array; accept any reply.
        let ok = matches!(
            tokio::time::timeout(SUBSCRIBER_PING_TIMEOUT, subscriber.ping::<RedisValue>()).await,
            Ok(Ok(_))
        );
        metrics::gauge!("zatat_redis_connected", "client" => "subscriber").set(if ok {
            1.0
        } else {
            0.0
        });
        if ok {
            failures = 0;
            continue;
        }
        failures += 1;
        metrics::counter!("zatat_redis_connection_errors_total", "client" => "subscriber")
            .increment(1);
        if failures >= SUBSCRIBER_MAX_FAILED_PINGS {
            warn!(failures, "redis subscriber unresponsive; forcing reconnect");
            if let Err(err) = subscriber.force_reconnection().await {
                warn!(%err, "redis subscriber forced reconnect failed; will retry");
            }
            failures = 0;
        }
    }
}

/// Exposes the client's connection lifecycle as metrics:
/// `zatat_redis_reconnects_total`, `zatat_redis_connection_errors_total` and
/// `zatat_redis_connected` (1 after a (re)connect, 0 after an error).
fn watch_connection_events<C: EventInterface>(client: &C, role: &'static str) {
    // The listener tasks are already spawned; dropping the handles detaches them.
    drop(client.on_reconnect(move |server| {
        metrics::counter!("zatat_redis_reconnects_total", "client" => role).increment(1);
        metrics::gauge!("zatat_redis_connected", "client" => role).set(1.0);
        info!(client = role, %server, "redis connected");
        Ok(())
    }));
    drop(client.on_error(move |err| {
        metrics::counter!("zatat_redis_connection_errors_total", "client" => role).increment(1);
        metrics::gauge!("zatat_redis_connected", "client" => role).set(0.0);
        warn!(client = role, %err, "redis connection error");
        Ok(())
    }));
}

fn build_builder(cfg: &RedisConfig) -> Result<Builder, RedisError> {
    // Redis may initialize TLS before the HTTPS listener. Pick a provider
    // explicitly because transitive dependencies can enable both backends.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = match &cfg.url {
        Some(url) => Builder::from_config(RedisConfig_Fred::from_url(url)?),
        None => {
            let config = RedisConfig_Fred {
                server: ServerConfig::new_centralized(&cfg.host, cfg.port),
                database: Some(cfg.db),
                username: cfg.username.clone(),
                password: cfg.password.clone(),
                ..Default::default()
            };
            Builder::from_config(config)
        }
    };
    builder.set_policy(ReconnectPolicy::new_exponential(0, 100, 1_000, 2));
    builder.with_performance_config(|p: &mut PerformanceConfig| {
        p.default_command_timeout = Duration::from_secs(cfg.timeout_seconds);
        // fred defaults to 32, which drops messages under any real burst on
        // the pub/sub bus. Our own bridge downstream can absorb plenty, so
        // this just has to be "large enough that Redis delivery never lags
        // fred's internal broadcaster".
        p.broadcast_channel_capacity = 65_536;
    });
    Ok(builder)
}

use fred::types::RedisConfig as RedisConfig_Fred;

#[cfg(test)]
mod tests {
    use super::*;
    use zatat_config::RedisConfig;

    fn base() -> RedisConfig {
        RedisConfig {
            url: None,
            host: "127.0.0.1".into(),
            port: 6379,
            db: 0,
            username: None,
            password: None,
            timeout_seconds: 5,
        }
    }

    #[test]
    fn url_form_centralized() {
        let mut cfg = base();
        cfg.url = Some("redis://127.0.0.1:6379/0".into());
        assert!(build_builder(&cfg).is_ok());
    }

    #[test]
    fn url_form_tls() {
        let mut cfg = base();
        cfg.url = Some("rediss://127.0.0.1:6379".into());
        let builder = build_builder(&cfg).unwrap();
        assert!(builder.get_config().unwrap().tls.is_some());
    }

    #[test]
    fn url_form_sentinel() {
        let mut cfg = base();
        cfg.url =
            Some("redis-sentinel://:mypass@127.0.0.1:26379/0?sentinelServiceName=mymaster".into());
        assert!(build_builder(&cfg).is_ok());
    }

    #[test]
    fn url_form_cluster() {
        let mut cfg = base();
        cfg.url = Some("redis-cluster://127.0.0.1:7000?node=127.0.0.1:7001".into());
        assert!(build_builder(&cfg).is_ok());
    }

    #[test]
    fn plain_host_port_without_url_works() {
        assert!(build_builder(&base()).is_ok());
    }
}
