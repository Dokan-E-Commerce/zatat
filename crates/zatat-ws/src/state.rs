use std::sync::Arc;

use tokio::sync::broadcast;

use zatat_channels::ChannelManager;
use zatat_config::Config;
use zatat_scaling::{EventDispatcher, LocalOnlyProvider, PubSubProvider};
use zatat_webhooks::{CompiledTarget, WebhookConfig, WebhookDispatcher};

pub type ServerState = Arc<ServerStateInner>;

pub struct ServerStateInner {
    pub config: Config,
    pub channels: ChannelManager,
    pub dispatcher: Arc<EventDispatcher>,
    pub webhooks: Arc<WebhookDispatcher>,
    pub shutdown: broadcast::Sender<()>,
    pub tracker: crate::tasks::ConnectionTracker,
}

impl std::fmt::Debug for ServerStateInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerStateInner")
            .field("apps", &self.config.apps_by_id.len())
            .field("connections", &self.connection_count_total())
            .finish()
    }
}

impl ServerStateInner {
    pub fn new(config: Config) -> Arc<Self> {
        Self::with_provider(config, Arc::new(LocalOnlyProvider), false)
    }

    pub fn with_provider(
        config: Config,
        provider: Arc<dyn PubSubProvider>,
        scaling_enabled: bool,
    ) -> Arc<Self> {
        let channels = ChannelManager::new();
        let dispatcher = Arc::new(EventDispatcher::new(
            channels.clone(),
            provider,
            scaling_enabled,
        ));
        let targets_by_app = compile_webhook_targets(&config);
        let webhooks = Arc::new(WebhookDispatcher::spawn(move |app_id| {
            targets_by_app.get(app_id).cloned().unwrap_or_default()
        }));
        let (shutdown, _) = broadcast::channel(1);
        Arc::new(Self {
            config,
            channels,
            dispatcher,
            webhooks,
            shutdown,
            tracker: crate::tasks::ConnectionTracker::new(),
        })
    }

    pub fn shutdown_now(&self) {
        let _ = self.shutdown.send(());
    }

    pub fn connection_count_total(&self) -> usize {
        self.config
            .apps_by_id
            .keys()
            .map(|id| self.channels.connection_count(id))
            .sum()
    }
}

fn compile_webhook_targets(
    config: &Config,
) -> std::collections::HashMap<String, Vec<CompiledTarget>> {
    use tracing::warn;
    let mut map: std::collections::HashMap<String, Vec<CompiledTarget>> = Default::default();
    for (id, app) in &config.apps_by_id {
        let mut targets = Vec::new();
        for raw in &app.webhooks {
            let cfg: WebhookConfig = match serde_json::from_value(raw.clone()) {
                Ok(c) => c,
                Err(err) => {
                    warn!(app = %id, %err, "skipping invalid webhook config entry");
                    continue;
                }
            };
            targets.push(CompiledTarget {
                app_id: id.as_str().to_string(),
                app_key: app.key.as_str().to_string(),
                app_secret: app.secret.clone(),
                url: cfg.url,
                event_filter: cfg.event_types,
                channel_prefix: cfg.filter_by_prefix,
            });
        }
        if !targets.is_empty() {
            map.insert(id.as_str().to_string(), targets);
        }
    }
    map
}
