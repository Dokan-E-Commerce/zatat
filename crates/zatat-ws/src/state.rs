use std::sync::Arc;

use tokio::sync::watch;

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
    /// Flips to `true` once when the server starts draining. A `watch`
    /// (not a one-shot broadcast) so connections that arrive after the flip
    /// still observe it.
    pub shutdown: watch::Sender<bool>,
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
        // Fleet transitions this node reports on another node's behalf.
        let webhooks_for_dispatcher = webhooks.clone();
        dispatcher.set_transition_sink(move |app, transition| {
            let event = match transition {
                zatat_scaling::FleetTransition::MemberRemoved { channel, user_id } => {
                    zatat_webhooks::WebhookEvent::MemberRemoved { channel, user_id }
                }
                zatat_scaling::FleetTransition::ChannelVacated { channel } => {
                    zatat_webhooks::WebhookEvent::ChannelVacated { channel }
                }
            };
            webhooks_for_dispatcher.enqueue(app.id.as_str(), event);
        });
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self {
            config,
            channels,
            dispatcher,
            webhooks,
            shutdown,
            tracker: crate::tasks::ConnectionTracker::new(),
        })
    }

    /// Enter draining: new upgrades are refused, `/health` reports 503 so
    /// load balancers stop routing here, and every live connection is closed
    /// with 1001. Idempotent.
    pub fn shutdown_now(&self) {
        self.shutdown.send_replace(true);
    }

    pub fn is_draining(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Resolves once draining has begun, whatever triggered it.
    pub async fn draining(&self) {
        let mut rx = self.shutdown.subscribe();
        let _ = rx.wait_for(|draining| *draining).await;
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
