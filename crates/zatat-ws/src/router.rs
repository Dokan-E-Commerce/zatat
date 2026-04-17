use std::net::SocketAddr;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tracing::{info, warn};

use zatat_core::error::PusherError;
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
    let app = match state.config.apps_by_key.get(&AppKey::from(app_key.clone())) {
        Some(app) => app.clone(),
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
    if let Some(origin_hdr) = origin.as_deref() {
        let host = url_host_or_self(origin_hdr);
        if !app.origin_is_allowed(host) {
            warn!(app = %app.id, origin = %origin_hdr, "rejected: origin not allowed");
            return (StatusCode::FORBIDDEN, PusherError::InvalidOrigin.message()).into_response();
        }
    }

    info!(
        app = %app.id,
        peer = %peer,
        origin = origin.as_deref().unwrap_or("<none>"),
        "ws upgrade accepted"
    );

    ws.on_upgrade(move |socket| async move {
        run_connection(state, app, socket, origin).await;
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
