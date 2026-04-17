use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use zatat_connection::{Connection, ConnectionHandle, Outbound, RateLimiter};
use zatat_core::application::{AcceptClientEventsFrom, AppArc};
use zatat_core::channel_name::{
    ChannelKind, ChannelName, MAX_CHANNEL_NAME_LEN, MAX_EVENT_NAME_LEN,
};
use zatat_core::error::PusherError;
use zatat_core::id::SocketId;
use zatat_protocol::auth::{verify_channel_auth, verify_user_auth};
use zatat_protocol::envelope::parse_inbound;
use zatat_protocol::outbound;
use zatat_protocol::presence::PresenceMember;
use zatat_webhooks::WebhookEvent;

use crate::state::ServerState;

/// Accept a WS that the router has decided to reject (origin denied,
/// over quota, etc.), send a `pusher:error` frame with the real reason,
/// and close with a matching Pusher close code. Gives the browser-side
/// client a clear error to surface instead of an opaque WS failure.
pub async fn run_rejected_connection(socket: WebSocket, code: u16, reason: String) {
    let (mut sink, _stream) = socket.split();
    let frame = outbound::error_with_message(code, &reason);
    let _ = sink.send(Message::Text(frame)).await;
    let _ = sink
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

pub async fn run_connection(
    state: ServerState,
    app: AppArc,
    socket: WebSocket,
    origin: Option<String>,
) {
    let socket_id = SocketId::generate();
    // Large buffer so bursty fan-out (e.g. a 50-msg scaling-bus chunk on a
    // 500-sub channel) doesn't hit try_send full before the WS sink drains.
    // If it ever DOES fill, the kick Notify fires and the connection closes
    // with 4301 rather than silently missing the frame.
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<Outbound>(4096);
    let kick = Arc::new(tokio::sync::Notify::new());
    let conn = Arc::new(Connection::new(app.clone(), socket_id.clone(), origin));
    let handle = ConnectionHandle::from_parts(socket_id.clone(), outbound_tx, kick.clone());

    if let Err(_e) = state
        .channels
        .register_connection(app.clone(), handle.clone())
    {
        let (mut sink, _stream) = socket.split();
        let _ = sink
            .send(Message::Text(outbound::error(
                &PusherError::OverConnectionQuota,
            )))
            .await;
        let _ = sink.send(pusher_close(4004, "over connection quota")).await;
        return;
    }
    state.tracker.register(crate::tasks::TrackedConnection {
        conn: conn.clone(),
        handle: handle.clone(),
    });

    let (mut sink, mut stream) = socket.split();
    let established = outbound::connection_established(socket_id.as_str(), app.activity_timeout);
    if sink.send(Message::Text(established)).await.is_err() {
        return;
    }

    let rate_limiter = app
        .rate_limiting
        .filter(|r| r.enabled)
        .map(|r| RateLimiter::new(r.max_attempts, r.decay_seconds));

    metrics::counter!(zatat_metrics::COUNTER_CONNECTIONS_TOTAL, "app" => app.id.as_str().to_string()).increment(1);
    metrics::gauge!(zatat_metrics::GAUGE_CONNECTIONS, "app" => app.id.as_str().to_string())
        .increment(1.0);

    let mut shutdown_rx = state.shutdown.subscribe();

    'conn: loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                let _ = sink.send(pusher_close(1001, "server going away")).await;
                break 'conn;
            }
            _ = kick.notified() => {
                // Slow-subscriber policy: outbound mpsc filled. The client's
                // TCP recv buffer is probably full too, so bound the close
                // writes with a short deadline and then drop.
                let frame = outbound::error_with_message(4301, "slow subscriber");
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sink.send(Message::Text(frame)),
                ).await;
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sink.send(pusher_close(4301, "slow subscriber")),
                ).await;
                break 'conn;
            }
            maybe_out = outbound_rx.recv() => {
                // Wrap the sink.send() in its own select so the kick can
                // abort a send that's been blocked on a slow client's TCP
                // recv buffer for too long.
                let kicked = match maybe_out {
                    Some(Outbound::Text(arc)) => {
                        metrics::counter!(zatat_metrics::COUNTER_MESSAGES_SENT, "app" => app.id.as_str().to_string()).increment(1);
                        tokio::select! {
                            _ = kick.notified() => true,
                            res = sink.send(Message::Text(arc.to_string())) => {
                                if res.is_err() { break 'conn; }
                                false
                            }
                        }
                    }
                    Some(Outbound::Ping) => {
                        // App-level pusher:ping demands a pusher:pong reply; WS-level PING
                        // is answered by the client's library without user code seeing it.
                        tokio::select! {
                            _ = kick.notified() => true,
                            res = sink.send(Message::Text(outbound::ping())) => {
                                if res.is_err() { break 'conn; }
                                false
                            }
                        }
                    }
                    Some(Outbound::Close { code, reason }) => {
                        let frame = outbound::error_with_message(code, &reason);
                        let _ = sink.send(Message::Text(frame)).await;
                        let _ = sink.send(pusher_close(code, &reason)).await;
                        break 'conn;
                    }
                    None => break 'conn,
                };
                if kicked {
                    let frame = outbound::error_with_message(4301, "slow subscriber");
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        sink.send(Message::Text(frame)),
                    ).await;
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        sink.send(pusher_close(4301, "slow subscriber")),
                    ).await;
                    break 'conn;
                }
            }
            maybe_frame = stream.next() => {
                match maybe_frame {
                    Some(Ok(Message::Text(text))) => {
                        conn.touch();
                        metrics::counter!(zatat_metrics::COUNTER_MESSAGES_RECEIVED, "app" => app.id.as_str().to_string()).increment(1);

                        if text.len() > app.max_message_size as usize {
                            let _ = sink
                                .send(Message::Text(outbound::error(&PusherError::InvalidMessageFormat)))
                                .await;
                            continue;
                        }
                        if let Some(rl) = &rate_limiter {
                            if !rl.check() {
                                metrics::counter!(zatat_metrics::COUNTER_RATE_LIMITED, "app" => app.id.as_str().to_string()).increment(1);
                                let _ = sink
                                    .send(Message::Text(outbound::error(&PusherError::RateLimitExceeded)))
                                    .await;
                                if app
                                    .rate_limiting
                                    .as_ref()
                                    .map(|c| c.terminate_on_limit)
                                    .unwrap_or(false)
                                {
                                    break;
                                }
                                continue;
                            }
                        }
                        match handle_inbound(&state, &app, &conn, &handle, &text).await {
                            Ok(Some(response)) => {
                                if sink.send(Message::Text(response)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => {}
                            Err(ControlFlow::Close) => break,
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        conn.touch();
                        let _ = sink.send(Message::Pong(p)).await;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        conn.touch();
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(Message::Binary(_))) => {}
                    Some(Err(_)) => break,
                    None => break,
                }
            }
        }
    }

    state.tracker.unregister(&socket_id);
    cleanup_connection(&state, &app, &conn, &socket_id).await;

    metrics::counter!(zatat_metrics::COUNTER_CONNECTIONS_CLOSED, "app" => app.id.as_str().to_string()).increment(1);
    metrics::gauge!(zatat_metrics::GAUGE_CONNECTIONS, "app" => app.id.as_str().to_string())
        .decrement(1.0);
}

enum ControlFlow {
    #[allow(dead_code)]
    Close,
}

async fn handle_inbound(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    text: &str,
) -> Result<Option<String>, ControlFlow> {
    let frame = match parse_inbound(text) {
        Ok(f) => f,
        Err(_) => return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat))),
    };

    match frame.event.as_str() {
        "pusher:ping" => Ok(Some(outbound::pong())),
        "pusher:subscribe" => handle_subscribe(state, app, conn, handle, &frame.data).await,
        "pusher:unsubscribe" => handle_unsubscribe(state, app, conn, &frame.data).await,
        "pusher:signin" => handle_signin(state, app, conn, handle, &frame.data).await,

        ev if ev.starts_with("client-") => {
            if ev.len() > MAX_EVENT_NAME_LEN {
                return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
            }
            handle_client_event(state, app, conn, ev, &frame).await
        }
        _ => Ok(None),
    }
}

async fn handle_subscribe(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    data: &Value,
) -> Result<Option<String>, ControlFlow> {
    let data_obj = data_object(data);
    let Some(channel_name) = data_obj.get("channel").and_then(|v| v.as_str()) else {
        return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
    };
    if channel_name.len() > MAX_CHANNEL_NAME_LEN {
        return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
    }
    let name = ChannelName::new(channel_name.to_string());
    let kind = name.kind();

    let channel_data = data_obj.get("channel_data").and_then(|v| v.as_str());
    if kind.is_private() {
        let Some(auth) = data_obj.get("auth").and_then(|v| v.as_str()) else {
            return Ok(Some(outbound::error(&PusherError::Unauthorized)));
        };
        if verify_channel_auth(
            conn.socket_id.as_str(),
            channel_name,
            channel_data,
            auth,
            &app.secret,
        )
        .is_err()
        {
            return Ok(Some(outbound::error(&PusherError::Unauthorized)));
        }
    }

    let presence = if kind.is_presence() {
        let Some(cd_str) = channel_data else {
            return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
        };
        let parsed: Value = serde_json::from_str(cd_str)
            .map_err(|_| ())
            .unwrap_or(Value::Null);
        let user_id = parsed.get("user_id").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        let Some(user_id) = user_id else {
            return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
        };
        let user_info = parsed.get("user_info").cloned();
        Some(PresenceMember { user_id, user_info })
    } else {
        None
    };

    let outcome = state.channels.subscribe(
        app,
        &name,
        conn.socket_id.clone(),
        handle.clone(),
        presence.clone(),
    );

    // subscription_succeeded must flush before member_added so the joining
    // client binds listeners before peers see the add.
    let succeeded = if kind.is_presence() {
        let merged = outcome.presence_snapshot.as_ref().map(|local| {
            let remote = state
                .dispatcher
                .presence_cache()
                .remote_members_for(app.id.as_str(), channel_name);
            if remote.is_empty() {
                local.clone()
            } else {
                merge_presence(local, remote)
            }
        });
        match merged {
            Some(p) => outbound::subscription_succeeded_with_presence(channel_name, &p),
            None => outbound::subscription_succeeded(channel_name, None),
        }
    } else {
        outbound::subscription_succeeded(channel_name, None)
    };

    if outcome.was_new && outcome.member_count == 1 {
        state.webhooks.enqueue(
            app.id.as_str(),
            WebhookEvent::ChannelOccupied {
                channel: channel_name.to_string(),
            },
        );
    }

    // On cache channels the cached payload replaces subscription_succeeded.
    if kind.is_cache() {
        if let Some(payload) = outcome.cached_payload {
            return Ok(Some(payload.to_string()));
        }
        state.webhooks.enqueue(
            app.id.as_str(),
            WebhookEvent::CacheMiss {
                channel: channel_name.to_string(),
            },
        );
        let miss = outbound::cache_miss(channel_name);
        let _ = handle.try_send(Outbound::Text(Arc::from(miss.into_boxed_str())));
    }

    if outcome.was_new && kind.is_presence() && outcome.user_added {
        if let Some(pm) = presence.as_ref() {
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::MemberAdded {
                    channel: channel_name.to_string(),
                    user_id: pm.user_id.clone(),
                },
            );
            // Defer so the joining client's subscription_succeeded flushes first.
            let channels = state.channels.clone();
            let app_id = app.id.clone();
            let channel_name_cloned = channel_name.to_string();
            let sender = conn.socket_id.clone();
            let user_id = pm.user_id.clone();
            let user_info = pm.user_info.clone();
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                let frame =
                    outbound::member_added(&channel_name_cloned, &user_id, user_info.as_ref());
                if let Some(ch) = channels.find_channel(&app_id, &channel_name_cloned) {
                    let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                    ch.broadcast_protocol(arc, Some(&sender));
                }
            });
        }
    }

    if app.emit_subscription_count && !kind.is_presence() {
        let count = outcome.member_count;
        let frame = outbound::subscription_count(channel_name, count);
        if let Some(ch) = state.channels.find_channel(&app.id, channel_name) {
            let arc: Arc<str> = Arc::from(frame.into_boxed_str());
            ch.broadcast_protocol(arc, None);
        }
        state.webhooks.enqueue(
            app.id.as_str(),
            WebhookEvent::SubscriptionCount {
                channel: channel_name.to_string(),
                count,
            },
        );
    }

    Ok(Some(succeeded))
}

async fn handle_unsubscribe(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    data: &Value,
) -> Result<Option<String>, ControlFlow> {
    let data_obj = data_object(data);
    let Some(channel_name) = data_obj.get("channel").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let outcome = state
        .channels
        .unsubscribe(&app.id, channel_name, &conn.socket_id);
    if let Some(outcome) = outcome {
        let kind = ChannelKind::from_name(channel_name);
        if outcome.was_member && kind.is_presence() {
            if let Some(user_id) = outcome.user_removed.as_deref() {
                let frame = outbound::member_removed(channel_name, user_id);
                if let Some(ch) = state.channels.find_channel(&app.id, channel_name) {
                    let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                    ch.broadcast_protocol(arc, None);
                }
                state.webhooks.enqueue(
                    app.id.as_str(),
                    WebhookEvent::MemberRemoved {
                        channel: channel_name.to_string(),
                        user_id: user_id.to_string(),
                    },
                );
            }
        }
        if outcome.was_member && app.emit_subscription_count && !kind.is_presence() {
            let count = outcome.member_count;
            let frame = outbound::subscription_count(channel_name, count);
            if let Some(ch) = state.channels.find_channel(&app.id, channel_name) {
                let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                ch.broadcast_protocol(arc, None);
            }
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::SubscriptionCount {
                    channel: channel_name.to_string(),
                    count,
                },
            );
        }
        if outcome.member_count == 0 {
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::ChannelVacated {
                    channel: channel_name.to_string(),
                },
            );
        }
    }
    Ok(None)
}

async fn handle_signin(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    data: &Value,
) -> Result<Option<String>, ControlFlow> {
    let data_obj = data_object(data);
    let Some(auth) = data_obj.get("auth").and_then(|v| v.as_str()) else {
        return Ok(Some(outbound::error(&PusherError::Unauthorized)));
    };
    let Some(user_data) = data_obj.get("user_data").and_then(|v| v.as_str()) else {
        return Ok(Some(outbound::error(&PusherError::Unauthorized)));
    };
    if verify_user_auth(conn.socket_id.as_str(), user_data, auth, &app.secret).is_err() {
        return Ok(Some(outbound::error(&PusherError::Unauthorized)));
    }
    let parsed: Value = serde_json::from_str(user_data).unwrap_or(Value::Null);
    let Some(user_id) = parsed.get("id").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }) else {
        return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
    };
    if user_id.is_empty() {
        return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
    }
    let was_online = state.channels.is_user_online(&app.id, &user_id);
    conn.bind_user(user_id.clone());
    state.channels.bind_user(&app.id, &user_id, &conn.socket_id);

    // Watchlist lives inside user_data, not at the signin payload's top level.
    let watchlist_value = parsed.get("watchlist").cloned();
    if let Some(Value::Array(arr)) = watchlist_value {
        if arr.len() > 100 {
            return Ok(Some(outbound::error_with_message(
                4302,
                "Watchlist exceeds 100 users",
            )));
        }
        let list: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        conn.set_watchlist(list.clone());
        for watched in &list {
            state.channels.add_watcher(&app.id, watched, &user_id);
        }
        let online: Vec<String> = list
            .iter()
            .filter(|u| state.channels.is_user_online(&app.id, u))
            .cloned()
            .collect();
        // pusher-js requires `data.events` to be an array of `{name, user_ids}`.
        let frame = zatat_protocol::envelope::encode_envelope(
            "pusher_internal:watchlist_events",
            Some(&json!({
                "events": [{
                    "name": "online",
                    "user_ids": online,
                }]
            })),
            None,
        );
        let _ = handle.try_send(Outbound::Text(Arc::from(frame.into_boxed_str())));
    }

    if !was_online {
        fanout_watchlist_event(state, app, &user_id, "online").await;
    }

    Ok(Some(outbound::signin_success(user_data)))
}

async fn handle_client_event(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    event: &str,
    frame: &zatat_protocol::envelope::InboundFrame,
) -> Result<Option<String>, ControlFlow> {
    if app.accept_client_events_from == AcceptClientEventsFrom::None {
        return Ok(Some(outbound::error(&PusherError::ClientEventsDisabled)));
    }
    let Some(channel_name) = frame.channel.as_deref() else {
        return Ok(Some(outbound::error(&PusherError::InvalidMessageFormat)));
    };
    let kind = ChannelKind::from_name(channel_name);
    if !kind.is_private() {
        return Ok(Some(outbound::error(&PusherError::ClientEventsDisabled)));
    }
    let Some(channel) = state.channels.find_channel(&app.id, channel_name) else {
        return Ok(Some(outbound::error(&PusherError::NotChannelMember)));
    };
    let members = channel.members_iter();
    if !members
        .iter()
        .any(|(sid, _, _)| sid == conn.socket_id.as_str())
    {
        return Ok(Some(outbound::error(&PusherError::NotChannelMember)));
    }
    if matches!(
        app.accept_client_events_from,
        AcceptClientEventsFrom::Members
    ) && !kind.is_private()
    {
        return Ok(Some(outbound::error(&PusherError::ClientEventsDisabled)));
    }
    let data_str = match &frame.data {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let encoded = zatat_protocol::envelope::encode_envelope_raw_data(
        event,
        Some(&data_str),
        Some(channel_name),
    );
    let arc: Arc<str> = Arc::from(encoded.into_boxed_str());
    channel.broadcast(arc, Some(&conn.socket_id));

    state.webhooks.enqueue(
        app.id.as_str(),
        WebhookEvent::ClientEvent {
            channel: channel_name.to_string(),
            event: event.to_string(),
            data: data_str,
            socket_id: Some(conn.socket_id.as_str().to_string()),
            user_id: conn.user_id(),
        },
    );
    Ok(None)
}

fn pusher_close(code: u16, reason: &str) -> Message {
    // pusher-js (and browsers generally) surface the WS close code to app
    // code. We always carry the Pusher error code here so clients don't
    // see 1005 "no status received" on every server-initiated close.
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.to_string().into(),
    }))
}

fn data_object(data: &Value) -> std::collections::BTreeMap<String, Value> {
    let mut out = std::collections::BTreeMap::new();
    let obj = if let Value::String(s) = data {
        serde_json::from_str::<Value>(s).unwrap_or(Value::Null)
    } else {
        data.clone()
    };
    if let Value::Object(m) = obj {
        for (k, v) in m {
            out.insert(k, v);
        }
    }
    out
}

async fn cleanup_connection(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    socket_id: &SocketId,
) {
    for ch in state.channels.channels(&app.id) {
        let had = ch
            .members_iter()
            .iter()
            .any(|(sid, _, _)| sid == socket_id.as_str());
        if !had {
            continue;
        }
        let name = ch.name().as_str().to_string();
        let kind = ch.kind();
        if let Some(outcome) = state.channels.unsubscribe(&app.id, &name, socket_id) {
            if outcome.was_member && kind.is_presence() {
                if let Some(user_id) = outcome.user_removed.as_deref() {
                    let frame = outbound::member_removed(&name, user_id);
                    if let Some(live) = state.channels.find_channel(&app.id, &name) {
                        let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                        live.broadcast_protocol(arc, None);
                    }
                    state.webhooks.enqueue(
                        app.id.as_str(),
                        WebhookEvent::MemberRemoved {
                            channel: name.clone(),
                            user_id: user_id.to_string(),
                        },
                    );
                }
            }
            if outcome.was_member && app.emit_subscription_count && !kind.is_presence() {
                let count = outcome.member_count;
                if let Some(live) = state.channels.find_channel(&app.id, &name) {
                    let frame = outbound::subscription_count(&name, count);
                    let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                    live.broadcast_protocol(arc, None);
                }
                state.webhooks.enqueue(
                    app.id.as_str(),
                    WebhookEvent::SubscriptionCount {
                        channel: name.clone(),
                        count,
                    },
                );
            }
            if outcome.member_count == 0 {
                state.webhooks.enqueue(
                    app.id.as_str(),
                    WebhookEvent::ChannelVacated { channel: name },
                );
            }
        }
    }
    if let Some(user_id) = conn.user_id() {
        let last_socket = state.channels.unbind_user(&app.id, &user_id, socket_id);
        if last_socket {
            fanout_watchlist_event(state, app, &user_id, "offline").await;
            state.channels.remove_all_watches_for(&app.id, &user_id);
        }
    }
    state.channels.unregister_connection(&app.id, socket_id);
}

async fn fanout_watchlist_event(
    state: &ServerState,
    app: &AppArc,
    user_id: &str,
    event_name: &str,
) {
    let watchers = state.channels.watchers_of(&app.id, user_id);
    for watcher_user_id in watchers {
        for handle in state
            .channels
            .connections_for_user(&app.id, &watcher_user_id)
        {
            let frame = zatat_protocol::envelope::encode_envelope(
                "pusher_internal:watchlist_events",
                Some(&json!({
                    "events": [{
                        "name": event_name,
                        "user_ids": [user_id],
                    }]
                })),
                None,
            );
            let _ = handle.try_send(Outbound::Text(Arc::from(frame.into_boxed_str())));
        }
    }
}

fn merge_presence(
    local: &zatat_protocol::presence::PresenceData,
    remote: Vec<zatat_scaling::PresenceSnapshotMember>,
) -> zatat_protocol::presence::PresenceData {
    let mut hash = local.hash.clone();
    let mut ids: std::collections::BTreeSet<String> = local.ids.iter().cloned().collect();
    for m in remote {
        if !hash.contains_key(&m.user_id) {
            hash.insert(
                m.user_id.clone(),
                m.user_info.unwrap_or(serde_json::Value::Null),
            );
            ids.insert(m.user_id);
        }
    }
    let sorted_ids: Vec<String> = ids.into_iter().collect();
    zatat_protocol::presence::PresenceData {
        count: sorted_ids.len(),
        ids: sorted_ids,
        hash,
    }
}
