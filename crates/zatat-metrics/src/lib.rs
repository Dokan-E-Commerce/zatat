#![forbid(unsafe_code)]

use std::net::SocketAddr;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

pub const COUNTER_CONNECTIONS_TOTAL: &str = "zatat_connections_total";
pub const COUNTER_CONNECTIONS_CLOSED: &str = "zatat_connections_closed_total";
pub const GAUGE_CONNECTIONS: &str = "zatat_connections";
pub const COUNTER_MESSAGES_SENT: &str = "zatat_messages_sent_total";
pub const COUNTER_MESSAGES_RECEIVED: &str = "zatat_messages_received_total";
pub const GAUGE_CHANNELS: &str = "zatat_channels_total";
pub const COUNTER_RATE_LIMITED: &str = "zatat_rate_limited_total";
pub const COUNTER_REDIS_RECONNECTS: &str = "zatat_redis_reconnects_total";

pub struct MetricsInstaller {
    handle: PrometheusHandle,
    listen: SocketAddr,
    bearer_token: Option<String>,
}

impl MetricsInstaller {
    pub fn install(listen: SocketAddr, bearer_token: Option<String>) -> Result<Self, String> {
        let builder = PrometheusBuilder::new();
        let handle = builder
            .install_recorder()
            .map_err(|e| format!("failed to install recorder: {e}"))?;

        metrics::describe_counter!(
            COUNTER_CONNECTIONS_TOTAL,
            "total WebSocket connections accepted"
        );
        metrics::describe_counter!(
            COUNTER_CONNECTIONS_CLOSED,
            "total WebSocket connections closed"
        );
        metrics::describe_gauge!(GAUGE_CONNECTIONS, "current live WebSocket connections");
        metrics::describe_counter!(COUNTER_MESSAGES_SENT, "total frames sent to clients");
        metrics::describe_counter!(
            COUNTER_MESSAGES_RECEIVED,
            "total frames received from clients"
        );
        metrics::describe_gauge!(GAUGE_CHANNELS, "current number of channels");
        metrics::describe_counter!(COUNTER_RATE_LIMITED, "total rate-limit rejections");
        metrics::describe_counter!(COUNTER_REDIS_RECONNECTS, "total Redis reconnect events");

        // Register the series so `# HELP` / `# TYPE` are exposed before traffic.
        metrics::counter!(COUNTER_CONNECTIONS_TOTAL).absolute(0);
        metrics::counter!(COUNTER_CONNECTIONS_CLOSED).absolute(0);
        metrics::gauge!(GAUGE_CONNECTIONS).set(0.0);
        metrics::counter!(COUNTER_MESSAGES_SENT).absolute(0);
        metrics::counter!(COUNTER_MESSAGES_RECEIVED).absolute(0);
        metrics::gauge!(GAUGE_CHANNELS).set(0.0);
        metrics::counter!(COUNTER_RATE_LIMITED).absolute(0);
        metrics::counter!(COUNTER_REDIS_RECONNECTS).absolute(0);

        if !listen.ip().is_loopback() && bearer_token.is_none() {
            warn!(
                "Prometheus /metrics is bound to a non-loopback interface WITHOUT a bearer_token — set server.prometheus.bearer_token before exposing this port. listen={listen}"
            );
        }
        info!(%listen, "metrics endpoint configured");
        Ok(Self {
            handle,
            listen,
            bearer_token,
        })
    }

    pub fn listen_addr(&self) -> SocketAddr {
        self.listen
    }

    /// Constant-time check of `Authorization: Bearer <token>`. When no
    /// token is configured, allows the request unconditionally (install()
    /// warns at startup if that happens on a non-loopback bind).
    pub fn authorize(&self, auth_header: Option<&str>) -> bool {
        match &self.bearer_token {
            Some(expected) => {
                let Some(header) = auth_header else {
                    return false;
                };
                let Some(token) = header.strip_prefix("Bearer ") else {
                    return false;
                };
                expected.as_bytes().ct_eq(token.as_bytes()).into()
            }
            None => true,
        }
    }

    pub fn render(&self) -> String {
        self.handle.render()
    }
}
