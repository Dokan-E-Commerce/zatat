use std::net::SocketAddr;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
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
        .route("/app/:app_key", get(ws_upgrade))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_upgrade(
    Path(app_key): Path<String>,
    Query(q): Query<UpgradeQuery>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ServerState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let app = match state.config.app_by_key(&AppKey::from(app_key.clone())) {
        Some(app) => app,
        None => {
            warn!(app_key = %app_key, "rejected connection: unknown app_key");
            return (StatusCode::UNAUTHORIZED, "Application does not exist").into_response();
        }
    };
    if let Some(protocol) = q.protocol.as_deref() {
        match protocol {
            "5" | "6" | "7" => {}
            other => {
                warn!(app = %app.id, protocol = %other, "rejected: unsupported protocol");
                return (StatusCode::BAD_REQUEST, "Unsupported protocol version").into_response();
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

    // Pusher protocol contract: upgrade the WS first, THEN deliver any
    // connection-refusal as a `pusher:error` frame. That's the only way a
    // browser-side pusher-js client ever surfaces the reason — an HTTP
    // 403 pre-upgrade looks to JS like a generic "WS failed".
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
}

fn url_host_or_self(raw: &str) -> &str {
    if let Some(rest) = raw.split_once("://").map(|(_, r)| r) {
        let rest = rest.split('/').next().unwrap_or("");
        rest.split(':').next().unwrap_or(rest)
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_parser_strips_scheme_and_port() {
        assert_eq!(url_host_or_self("https://example.com"), "example.com");
        assert_eq!(url_host_or_self("https://example.com:8443"), "example.com");
        assert_eq!(url_host_or_self("http://a.b.c:80/path"), "a.b.c");
        assert_eq!(url_host_or_self("example.com"), "example.com");
    }
}
