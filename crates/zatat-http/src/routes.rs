use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;

use zatat_channels::{ChannelManager, ChannelStats};
use zatat_config::Config;
use zatat_core::application::AppArc;
use zatat_core::channel_name::{MAX_CHANNEL_NAME_LEN, MAX_EVENT_NAME_LEN};
use zatat_core::id::{AppId, SocketId};
use zatat_scaling::EventDispatcher;
use zatat_webhooks::WebhookDispatcher;

use crate::sign::{verify_request, VerifyError};

pub type ApiState = Arc<ApiStateInner>;

pub struct ApiStateInner {
    pub config: Config,
    pub channels: ChannelManager,
    pub dispatcher: Arc<EventDispatcher>,
    pub webhooks: Arc<WebhookDispatcher>,
}

pub fn build_api_router(state: ApiState) -> Router {
    Router::new()
        .route("/apps/:app_id/events", post(publish_event))
        .route("/apps/:app_id/batch_events", post(publish_batch_events))
        .route("/apps/:app_id/channels", get(list_channels))
        .route("/apps/:app_id/channels/:channel", get(channel_info))
        .route("/apps/:app_id/channels/:channel/users", get(channel_users))
        .route("/apps/:app_id/users/:user_id/events", post(user_events))
        .route("/apps/:app_id/users/:user_id", delete(terminate_user))
        .route(
            "/apps/:app_id/users/:user_id/terminate_connections",
            post(terminate_user_pusher),
        )
        .with_state(state)
}

#[derive(Debug, Deserialize, Clone)]
struct EventPayload {
    name: String,
    data: Value,
    #[serde(default)]
    channels: Option<Vec<String>>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    socket_id: Option<String>,
    #[serde(default)]
    info: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BatchEventsPayload {
    batch: Vec<EventPayload>,
}

#[derive(Debug, Deserialize)]
struct UserEventPayload {
    name: String,
    data: Value,
}

struct ApiError(u16, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0).unwrap_or(StatusCode::BAD_REQUEST);
        (status, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<VerifyError> for ApiError {
    fn from(e: VerifyError) -> Self {
        Self(e.code, e.message)
    }
}

struct MethodUri(Method, Uri);

#[axum::async_trait]
impl<S> axum::extract::FromRequestParts<S> for MethodUri
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.method.clone(), parts.uri.clone()))
    }
}

fn resolve_app(state: &ApiState, app_id: &str) -> Result<AppArc, ApiError> {
    state
        .config
        .app_by_id(&AppId::from(app_id))
        .ok_or_else(|| ApiError(404, "Application does not exist".into()))
}

fn verify_and_log(
    app: &AppArc,
    method_uri: &MethodUri,
    path_prefix: &str,
    body: &Bytes,
    _headers: &HeaderMap,
) -> Result<(), ApiError> {
    verify_request(app, method_uri.0.as_str(), &method_uri.1, path_prefix, body)?;
    Ok(())
}

fn channel_stats_object(s: &ChannelStats, info_csv: Option<&str>) -> Value {
    let wants = |k: &str| match info_csv {
        Some(s) => s.split(',').any(|p| p.trim() == k),
        None => false,
    };
    let mut obj = serde_json::Map::new();
    if wants("occupied") {
        obj.insert("occupied".into(), Value::Bool(s.occupied));
    }
    if wants("subscription_count") {
        obj.insert(
            "subscription_count".into(),
            Value::from(s.subscription_count),
        );
    }
    if wants("user_count") {
        if let Some(u) = s.user_count {
            obj.insert("user_count".into(), Value::from(u));
        }
    }
    if wants("cache") {
        obj.insert("cache".into(), Value::Bool(s.has_cached_payload));
    }
    Value::Object(obj)
}

async fn publish_event(
    Path(app_id): Path<String>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &body,
        &headers,
    )?;

    let payload: EventPayload =
        serde_json::from_slice(&body).map_err(|e| ApiError(422, format!("invalid body: {e}")))?;
    validate_event_payload(&payload)?;

    let info_requested = payload.info.clone();
    let channels_list = event_channels(&payload);
    dispatch_event(&state, &app, payload).await;
    if let Some(info) = info_requested {
        let mut per_channel = serde_json::Map::new();
        for ch in channels_list {
            let stats = state.channels.channel_stats(&app.id, &ch);
            let obj = stats
                .map(|s| channel_stats_object(&s, Some(&info)))
                .unwrap_or_else(|| json!({}));
            per_channel.insert(ch, obj);
        }
        return Ok((StatusCode::OK, Json(json!({ "channels": per_channel }))).into_response());
    }
    Ok((StatusCode::OK, Json(json!({}))).into_response())
}

async fn publish_batch_events(
    Path(app_id): Path<String>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &body,
        &headers,
    )?;

    let payload: BatchEventsPayload =
        serde_json::from_slice(&body).map_err(|e| ApiError(422, format!("invalid body: {e}")))?;
    for ev in &payload.batch {
        validate_event_payload(ev)?;
    }

    // Pusher spec: `batch` in the response is same-length as the input,
    // with per-event stats whenever that event requested `info`.
    let any_info = payload.batch.iter().any(|ev| ev.info.is_some());
    let mut out: Vec<Value> = Vec::with_capacity(payload.batch.len());
    for ev in payload.batch {
        let info = ev.info.clone();
        let first_channel = event_channels(&ev).into_iter().next();
        dispatch_event(&state, &app, ev).await;
        let entry = match (info, first_channel) {
            (Some(info), Some(ch)) => state
                .channels
                .channel_stats(&app.id, &ch)
                .map(|s| channel_stats_object(&s, Some(&info)))
                .unwrap_or_else(|| json!({})),
            _ => json!({}),
        };
        out.push(entry);
    }
    if any_info {
        Ok((StatusCode::OK, Json(json!({ "batch": out }))).into_response())
    } else {
        Ok((StatusCode::OK, Json(json!({ "batch": [] }))).into_response())
    }
}

async fn dispatch_event(state: &ApiState, app: &AppArc, ev: EventPayload) {
    let channels = event_channels(&ev);
    let data_str = data_to_string(&ev.data);
    let except = ev.socket_id.map(SocketId::from_string);

    for channel in channels {
        state
            .dispatcher
            .dispatch_message(
                app.clone(),
                channel,
                ev.name.clone(),
                data_str.clone(),
                except.clone(),
            )
            .await;
    }
}

fn validate_event_payload(ev: &EventPayload) -> Result<(), ApiError> {
    if ev.name.len() > MAX_EVENT_NAME_LEN {
        return Err(ApiError(
            422,
            format!("event name exceeds {MAX_EVENT_NAME_LEN} bytes"),
        ));
    }
    for ch in event_channels(ev) {
        if ch.len() > MAX_CHANNEL_NAME_LEN {
            return Err(ApiError(
                422,
                format!("channel name exceeds {MAX_CHANNEL_NAME_LEN} bytes: {ch}"),
            ));
        }
    }
    Ok(())
}

fn event_channels(ev: &EventPayload) -> Vec<String> {
    if let Some(list) = &ev.channels {
        list.clone()
    } else if let Some(one) = &ev.channel {
        vec![one.clone()]
    } else {
        Vec::new()
    }
}

fn data_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

async fn list_channels(
    Path(app_id): Path<String>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &Bytes::new(),
        &headers,
    )?;

    let pairs = crate::sign::parse_query(&method_uri.1);
    let filter = pairs
        .iter()
        .find_map(|(k, v)| (k == "filter_by_prefix").then_some(v.as_str()));
    let info = pairs
        .iter()
        .find_map(|(k, v)| (k == "info").then_some(v.as_str()));

    #[derive(Default)]
    struct Merged {
        occupied: bool,
        subscription_count: usize,
        user_ids: std::collections::BTreeSet<String>,
        has_cached_payload: bool,
        is_presence: bool,
    }
    let mut merged: std::collections::BTreeMap<String, Merged> = std::collections::BTreeMap::new();
    for ch in state.channels.channels(&app.id) {
        if let Some(f) = filter {
            if !ch.name().as_str().starts_with(f) {
                continue;
            }
        }
        let stats = ch.stats();
        let entry = merged.entry(ch.name().as_str().to_string()).or_default();
        entry.occupied = entry.occupied || stats.occupied;
        entry.subscription_count += stats.subscription_count;
        entry.has_cached_payload = entry.has_cached_payload || stats.has_cached_payload;
        if ch.kind().is_presence() {
            entry.is_presence = true;
            for (_, _, presence) in ch.members_iter() {
                if let Some(m) = presence {
                    entry.user_ids.insert(m.user_id);
                }
            }
        }
    }

    let remote = state
        .dispatcher
        .ask_fleet_for_channels(
            &app,
            zatat_scaling::message::MetricsQuery {
                filter_by_prefix: filter.map(|s| s.to_string()),
                info: info.map(|s| s.to_string()),
            },
            std::time::Duration::from_millis(750),
        )
        .await
        .unwrap_or_default();
    for m in remote {
        let entry = merged.entry(m.name).or_default();
        entry.occupied = entry.occupied || m.occupied;
        entry.subscription_count += m.subscription_count;
        entry.has_cached_payload = entry.has_cached_payload || m.has_cached_payload;
        if m.user_count.is_some() {
            entry.is_presence = true;
            for id in m.presence_user_ids {
                entry.user_ids.insert(id);
            }
        }
    }

    let mut obj = serde_json::Map::new();
    for (name, m) in merged {
        let stats = zatat_channels::ChannelStats {
            occupied: m.occupied,
            subscription_count: m.subscription_count,
            user_count: if m.is_presence {
                Some(m.user_ids.len())
            } else {
                None
            },
            has_cached_payload: m.has_cached_payload,
        };
        obj.insert(name, channel_stats_object(&stats, info));
    }
    let channels_value = if obj.is_empty() {
        Value::Array(Vec::new())
    } else {
        Value::Object(obj)
    };
    Ok(Json(json!({ "channels": channels_value })).into_response())
}

async fn channel_info(
    Path((app_id, channel)): Path<(String, String)>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &Bytes::new(),
        &headers,
    )?;

    let pairs = crate::sign::parse_query(&method_uri.1);
    let info = pairs
        .iter()
        .find_map(|(k, v)| (k == "info").then_some(v.as_str()));

    let info_plus_occupied: String = match info {
        Some(s) if !s.is_empty() => format!("{},occupied,subscription_count", s),
        _ => "occupied,subscription_count".into(),
    };
    let stats = match state.channels.channel_stats(&app.id, &channel) {
        Some(s) => channel_stats_object(&s, Some(&info_plus_occupied)),
        None => json!({ "occupied": false }),
    };
    Ok(Json(stats).into_response())
}

async fn channel_users(
    Path((app_id, channel_name)): Path<(String, String)>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &Bytes::new(),
        &headers,
    )?;

    let Some(ch) = state.channels.find_channel(&app.id, &channel_name) else {
        return Err(ApiError(404, "channel not found".into()));
    };
    if !ch.kind().is_presence() {
        return Err(ApiError(400, "not a presence channel".into()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for (_, _, presence) in ch.members_iter() {
        if let Some(m) = presence {
            seen.insert(m.user_id);
        }
    }
    let users: Vec<Value> = seen.into_iter().map(|id| json!({ "id": id })).collect();
    Ok(Json(json!({ "users": users })).into_response())
}

async fn user_events(
    Path((app_id, user_id)): Path<(String, String)>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &body,
        &headers,
    )?;

    let payload: UserEventPayload =
        serde_json::from_slice(&body).map_err(|e| ApiError(400, format!("invalid body: {e}")))?;
    let data_str = data_to_string(&payload.data);

    let handles_before = state.channels.connections_for_user(&app.id, &user_id).len();
    state
        .dispatcher
        .dispatch_user_event(app.clone(), user_id.clone(), payload.name, data_str)
        .await;

    Ok((StatusCode::OK, Json(json!({ "delivered": handles_before }))).into_response())
}

async fn terminate_user(
    Path((app_id, user_id)): Path<(String, String)>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    terminate_user_impl(app_id, user_id, state, method_uri, headers, Bytes::new()).await
}

async fn terminate_user_pusher(
    Path((app_id, user_id)): Path<(String, String)>,
    State(state): State<ApiState>,
    method_uri: MethodUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    terminate_user_impl(app_id, user_id, state, method_uri, headers, body).await
}

async fn terminate_user_impl(
    app_id: String,
    user_id: String,
    state: ApiState,
    method_uri: MethodUri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let app = resolve_app(&state, &app_id)?;
    verify_and_log(
        &app,
        &method_uri,
        &state.config.server.path,
        &body,
        &headers,
    )?;

    let handles = state.channels.connections_for_user(&app.id, &user_id);
    let n = handles.len();
    for h in handles {
        let _ = h.try_send(zatat_connection::Outbound::Close {
            code: 4009,
            reason: "terminated".into(),
        });
    }
    state
        .dispatcher
        .publish_terminate_user(&app, user_id.clone())
        .await;
    warn!(app = %app.id, user = %user_id, closed_here = %n, "terminated user connections (local); peers notified");
    Ok((StatusCode::OK, Json(json!({ "terminated": n }))).into_response())
}
