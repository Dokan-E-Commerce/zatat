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
            warn!(%err, "redis publish failed");
        }
    }

    async fn subscribe(&self) -> Result<broadcast::Receiver<Vec<u8>>, String> {
        Ok(self.tx.subscribe())
    }
}

fn build_builder(cfg: &RedisConfig) -> Result<Builder, RedisError> {
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
        assert!(build_builder(&cfg).is_ok());
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
