use std::net::SocketAddr;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tracing::{info, warn};

use zatat_core::id::AppKey;

use crate::handler::run_connection;
use crate::state::ServerState;

#[derive(Debug, Deserialize)]
pub struct UpgradeQuery {
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

pub fn build_router(state: ServerState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/up", get(health))
        .route("/app/:app_key", get(ws_upgrade))
        .with_state(state)
}

/// Liveness *and* readiness: 503 once the node is draining, so load
/// balancers stop sending new connections before the listener closes.
pub async fn health(State(state): State<ServerState>) -> Response {
    if state.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response()
    } else {
        "ok".into_response()
    }
}

/// Transport-level cap on one inbound WebSocket message (and frame). The
/// app's `max_message_size` is enforced after parsing with a Pusher error
/// reply; this bound stops a client from making the server buffer the
/// transport default of 64 MiB before that check runs.
fn transport_message_limit(app_max_message_size: u32) -> usize {
    (app_max_message_size as usize)
        .saturating_mul(4)
        .max(64 * 1024)
}

async fn ws_upgrade(
    Path(app_key): Path<String>,
    Query(q): Query<UpgradeQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ServerState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if state.is_draining() {
        return (StatusCode::SERVICE_UNAVAILABLE, "server is shutting down").into_response();
    }

    // Pusher protocol contract: surface rejections as WS close codes, not
    // pre-upgrade HTTP. Browsers see HTTP 401/400 pre-upgrade as a generic
    // "connection failed"; they only surface the Pusher error code when the
    // WS has already been upgraded. So for every rejection path we do a
    // bare upgrade and immediately close with the right code.

    // 4001: application not found.
    let app = match state.config.app_by_key(&AppKey::from(app_key.clone())) {
        Some(app) => app,
        None => {
            warn!(app_key = %app_key, "rejected connection: unknown app_key (sending 4001)");
            let app_key_owned = app_key.clone();
            return ws
                .max_message_size(64 * 1024)
                .max_frame_size(64 * 1024)
                .on_upgrade(move |socket| async move {
                    crate::handler::run_rejected_connection(
                        socket,
                        4001,
                        format!("Application does not exist for key '{app_key_owned}'"),
                    )
                    .await;
                })
                .into_response();
        }
    };
    let limit = transport_message_limit(app.max_message_size);
    let ws = ws.max_message_size(limit).max_frame_size(limit);

    // 4007: unsupported Pusher protocol version.
    if let Some(protocol) = q.protocol.as_deref() {
        match protocol {
            "5" | "6" | "7" => {}
            other => {
                warn!(app = %app.id, protocol = %other, "rejected: unsupported protocol (sending 4007)");
                let protocol_owned = other.to_string();
                return ws.on_upgrade(move |socket| async move {
                    crate::handler::run_rejected_connection(
                        socket,
                        4007,
                        format!(
                            "Unsupported protocol version '{protocol_owned}' — server supports 5, 6, 7",
                        ),
                    )
                    .await;
                }).into_response();
            }
        }
    }

    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let origin_blocked = match origin.as_deref() {
        Some(origin_hdr) => !app.origin_is_allowed(url_host_or_self(origin_hdr)),
        None => false,
    };
    if origin_blocked {
        warn!(
            app = %app.id,
            origin = origin.as_deref().unwrap_or("<none>"),
            allowed = ?app.allowed_origins_raw,
            "origin rejected — accepting WS and sending pusher:error 4009 so the client can see why"
        );
    } else {
        info!(
            app = %app.id,
            peer = %peer,
            origin = origin.as_deref().unwrap_or("<none>"),
            "ws upgrade accepted"
        );
    }

    let origin_for_msg = origin.clone();
    ws.on_upgrade(move |socket| async move {
        if origin_blocked {
            crate::handler::run_rejected_connection(
                socket,
                4009,
                format!(
                    "Origin '{}' is not in the allowed list for app '{}'",
                    origin_for_msg.as_deref().unwrap_or(""),
                    app.id,
                ),
            )
            .await;
        } else {
            run_connection(state, app, socket, origin).await;
        }
    })
    .into_response()
}

pub(crate) fn url_host_or_self(raw: &str) -> &str {
    if let Some(rest) = raw.split_once("://").map(|(_, r)| r) {
        let authority = rest.split('/').next().unwrap_or("");
        if let Some(rest) = authority.strip_prefix('[') {
            rest.split(']').next().unwrap_or(rest)
        } else {
            authority.split(':').next().unwrap_or(authority)
        }
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_limit_covers_app_limit_with_a_floor() {
        assert_eq!(transport_message_limit(10_000), 64 * 1024);
        assert_eq!(transport_message_limit(100_000), 400_000);
        assert_eq!(transport_message_limit(u32::MAX), u32::MAX as usize * 4);
    }

    #[test]
    fn origin_parser_strips_scheme_and_port() {
        assert_eq!(url_host_or_self("https://example.com"), "example.com");
        assert_eq!(url_host_or_self("https://example.com:8443"), "example.com");
        assert_eq!(url_host_or_self("http://a.b.c:80/path"), "a.b.c");
        assert_eq!(url_host_or_self("example.com"), "example.com");
        assert_eq!(url_host_or_self("https://[::1]:8443"), "::1");
        assert_eq!(url_host_or_self("http://[2001:db8::1]/x"), "2001:db8::1");
    }
}
