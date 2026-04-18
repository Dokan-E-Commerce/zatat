use std::sync::Arc;

use tokio::sync::broadcast;

use zatat_channels::ChannelManager;
use zatat_config::Config;
use zatat_core::application::Application;
use zatat_core::id::AppId;
use zatat_scaling::{EventDispatcher, LocalOnlyProvider, PubSubProvider, PublishOverflow};
use zatat_webhooks::{CompiledTarget, WebhookConfig, WebhookDispatcher, WebhookOverflow};

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
            .field("apps", &self.config.apps().by_id.len())
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
        let overflow = config
            .server
            .scaling
            .as_ref()
            .map(|s| match s.overflow_mode {
                zatat_config::OverflowMode::BestEffort => PublishOverflow::BestEffort,
                zatat_config::OverflowMode::Block => PublishOverflow::Block,
            })
            .unwrap_or_default();
        let dispatcher = Arc::new(EventDispatcher::with_overflow(
            channels.clone(),
            provider,
            scaling_enabled,
            overflow,
        ));
        // Webhook lookup reads the LIVE apps table, so adding/removing
        // webhook targets via a config reload takes effect immediately.
        let config_for_webhooks = config.clone();
        let webhook_overflow = match config.server.webhook_overflow_mode {
            zatat_config::OverflowMode::BestEffort => WebhookOverflow::BestEffort,
            zatat_config::OverflowMode::Block => WebhookOverflow::Block,
        };
        let webhooks = Arc::new(WebhookDispatcher::spawn_with_overflow(
            move |app_id| {
                let Some(app) = config_for_webhooks.app_by_id(&AppId::from(app_id)) else {
                    return Vec::new();
                };
                compile_webhook_targets_for_app(&app)
            },
            webhook_overflow,
        ));
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
            .apps()
            .by_id
            .keys()
            .map(|id| self.channels.connection_count(id))
            .sum()
    }
}

fn compile_webhook_targets_for_app(app: &Application) -> Vec<CompiledTarget> {
    use tracing::warn;
    let mut targets = Vec::new();
    for raw in &app.webhooks {
        let cfg: WebhookConfig = match serde_json::from_value(raw.clone()) {
            Ok(c) => c,
            Err(err) => {
                warn!(app = %app.id, %err, "skipping invalid webhook config entry");
                continue;
            }
        };
        targets.push(CompiledTarget {
            app_id: app.id.as_str().to_string(),
            app_key: app.key.as_str().to_string(),
            app_secret: app.secret.clone(),
            url: cfg.url,
            event_filter: cfg.event_types,
            channel_prefix: cfg.filter_by_prefix,
        });
    }
    targets
}
