use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use zatat_channels::{Channel, SubscribeResult, UnsubscribeOutcome};
use zatat_connection::{Connection, ConnectionHandle, Outbound, RateLimiter};
use zatat_core::application::{AcceptClientEventsFrom, AppArc};
use zatat_core::channel_name::{
    is_valid_channel_name, ChannelKind, ChannelName, MAX_CHANNEL_NAME_LEN, MAX_EVENT_NAME_LEN,
};
use zatat_core::error::PusherError;
use zatat_core::id::SocketId;
use zatat_protocol::auth::{verify_channel_auth, verify_user_auth};
use zatat_protocol::envelope::parse_inbound;
use zatat_protocol::outbound;
use zatat_protocol::presence::PresenceMember;
use zatat_webhooks::WebhookEvent;

use crate::state::ServerState;

/// Upper bound on any single socket write. A peer that stops reading fills
/// its TCP window; without a deadline the write — and with it the whole
/// connection task, including kick and shutdown handling — would block
/// forever while holding the connection slot.
/// Pusher's per-user watchlist limit.
const MAX_WATCHLIST_SIZE: usize = 100;

const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Close frames go to peers we are dropping anyway; don't wait long.
const CLOSE_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

type WsSink = futures::stream::SplitSink<WebSocket, Message>;

/// Sends `msg` within `WRITE_TIMEOUT`. `false` means the socket is dead or
/// stalled and the connection must be dropped.
async fn send(sink: &mut WsSink, msg: Message) -> bool {
    match tokio::time::timeout(WRITE_TIMEOUT, sink.send(msg)).await {
        Ok(Ok(())) => true,
        Ok(Err(_)) => false,
        Err(_) => {
            metrics::counter!("zatat_ws_write_timeouts_total").increment(1);
            false
        }
    }
}

/// Best-effort `pusher:error` + close frame, each bounded by a short deadline.
async fn send_close(sink: &mut WsSink, code: u16, reason: &str) {
    let frame = outbound::error_with_message(code, reason);
    let _ = tokio::time::timeout(CLOSE_WRITE_TIMEOUT, sink.send(Message::Text(frame))).await;
    let _ = tokio::time::timeout(CLOSE_WRITE_TIMEOUT, sink.send(pusher_close(code, reason))).await;
}

/// Accept a WS that the router has decided to reject (origin denied,
/// over quota, etc.), send a `pusher:error` frame with the real reason,
/// and close with a matching Pusher close code. Gives the browser-side
/// client a clear error to surface instead of an opaque WS failure.
pub async fn run_rejected_connection(socket: WebSocket, code: u16, reason: String) {
    let (mut sink, _stream) = socket.split();
    send_close(&mut sink, code, &reason).await;
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
        send_close(&mut sink, 4004, PusherError::OverConnectionQuota.message()).await;
        return;
    }
    state.tracker.register(crate::tasks::TrackedConnection {
        conn: conn.clone(),
        handle: handle.clone(),
    });

    let (mut sink, mut stream) = socket.split();
    let established = outbound::connection_established(socket_id.as_str(), app.activity_timeout);
    if !send(&mut sink, Message::Text(established)).await {
        state.tracker.unregister(&socket_id);
        state.channels.unregister_connection(&app.id, &socket_id);
        return;
    }

    let rate_limiter = app
        .rate_limiting
        .filter(|r| r.enabled)
        .map(|r| RateLimiter::new(r.max_attempts, r.decay_seconds));
    let terminate_on_limit = app
        .rate_limiting
        .as_ref()
        .map(|c| c.terminate_on_limit)
        .unwrap_or(false);

    metrics::counter!(zatat_metrics::COUNTER_CONNECTIONS_TOTAL, "app" => app.id.as_str().to_string()).increment(1);
    metrics::gauge!(zatat_metrics::GAUGE_CONNECTIONS, "app" => app.id.as_str().to_string())
        .increment(1.0);

    'conn: loop {
        tokio::select! {
            _ = state.draining() => {
                let _ = tokio::time::timeout(
                    CLOSE_WRITE_TIMEOUT,
                    sink.send(pusher_close(1001, "server going away")),
                ).await;
                break 'conn;
            }
            _ = kick.notified() => {
                // Slow-subscriber policy: outbound mpsc filled. The client's
                // TCP recv buffer is probably full too, so bound the close
                // writes with a short deadline and then drop.
                send_close(&mut sink, 4301, "slow subscriber").await;
                break 'conn;
            }
            maybe_out = outbound_rx.recv() => {
                let msg = match maybe_out {
                    Some(Outbound::Text(arc)) => {
                        metrics::counter!(zatat_metrics::COUNTER_MESSAGES_SENT, "app" => app.id.as_str().to_string()).increment(1);
                        Message::Text(arc.to_string())
                    }
                    // App-level pusher:ping demands a pusher:pong reply; WS-level PING
                    // is answered by the client's library without user code seeing it.
                    Some(Outbound::Ping) => Message::Text(outbound::ping()),
                    Some(Outbound::Close { code, reason }) => {
                        send_close(&mut sink, code, &reason).await;
                        break 'conn;
                    }
                    None => break 'conn,
                };
                // The kick and shutdown can each abort a send that is stuck
                // on a slow client's TCP window; WRITE_TIMEOUT bounds the rest.
                tokio::select! {
                    _ = kick.notified() => {
                        send_close(&mut sink, 4301, "slow subscriber").await;
                        break 'conn;
                    }
                    _ = state.draining() => {
                        let _ = tokio::time::timeout(
                            CLOSE_WRITE_TIMEOUT,
                            sink.send(pusher_close(1001, "server going away")),
                        ).await;
                        break 'conn;
                    }
                    sent = send(&mut sink, msg) => {
                        if !sent { break 'conn; }
                    }
                }
            }
            maybe_frame = stream.next() => {
                let Some(Ok(frame)) = maybe_frame else { break 'conn };
                if matches!(frame, Message::Close(_)) {
                    break 'conn;
                }
                conn.touch();
                // Every inbound frame counts against the rate limit — pings
                // and ignored binary frames included — before any work.
                if let Some(rl) = &rate_limiter {
                    if !rl.check() {
                        metrics::counter!(zatat_metrics::COUNTER_RATE_LIMITED, "app" => app.id.as_str().to_string()).increment(1);
                        if !send(&mut sink, Message::Text(outbound::error(&PusherError::RateLimitExceeded))).await
                            || terminate_on_limit
                        {
                            break 'conn;
                        }
                        continue;
                    }
                }
                // Pings are answered by the transport; binary is not part of
                // the Pusher protocol.
                let Message::Text(text) = frame else { continue };
                metrics::counter!(zatat_metrics::COUNTER_MESSAGES_RECEIVED, "app" => app.id.as_str().to_string()).increment(1);

                if text.len() > app.max_message_size as usize {
                    if !send(&mut sink, Message::Text(outbound::error(&PusherError::InvalidMessageFormat))).await {
                        break 'conn;
                    }
                    continue;
                }
                if let Some(response) = handle_inbound(&state, &app, &conn, &handle, &text).await {
                    if !send(&mut sink, Message::Text(response)).await {
                        break 'conn;
                    }
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

async fn handle_inbound(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    text: &str,
) -> Option<String> {
    // Use the app's live settings: a rotated secret or changed limit
    // applies to later subscribes/sign-ins on existing connections too. A
    // removed app's connections are being closed by the reload.
    let live_app = state.config.app_by_id(&app.id);
    let app = live_app.as_ref().unwrap_or(app);
    let frame = match parse_inbound(text) {
        Ok(f) => f,
        Err(_) => return Some(outbound::error(&PusherError::InvalidMessageFormat)),
    };

    match frame.event.as_str() {
        "pusher:ping" => Some(outbound::pong()),
        "pusher:subscribe" => handle_subscribe(state, app, conn, handle, &frame.data).await,
        "pusher:unsubscribe" => handle_unsubscribe(state, app, conn, &frame.data).await,
        "pusher:signin" => handle_signin(state, app, conn, handle, &frame.data).await,

        ev if ev.starts_with("client-") => {
            if ev.len() > MAX_EVENT_NAME_LEN {
                return Some(outbound::error(&PusherError::InvalidMessageFormat));
            }
            handle_client_event(state, app, conn, ev, &frame).await
        }
        _ => None,
    }
}

async fn handle_subscribe(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    data: &Value,
) -> Option<String> {
    let data_obj = data_object(data);
    let Some(channel_name) = data_obj.get("channel").and_then(|v| v.as_str()) else {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    };
    if channel_name.len() > MAX_CHANNEL_NAME_LEN {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    }
    if !is_valid_channel_name(channel_name) {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    }
    let name = ChannelName::new(channel_name.to_string());
    let kind = name.kind();

    let channel_data = data_obj.get("channel_data").and_then(|v| v.as_str());
    if kind.is_private() {
        let Some(auth) = data_obj.get("auth").and_then(|v| v.as_str()) else {
            return Some(outbound::error(&PusherError::Unauthorized));
        };
        if verify_channel_auth(
            app.key.as_str(),
            conn.socket_id.as_str(),
            channel_name,
            channel_data,
            auth,
            &app.secret,
        )
        .is_err()
        {
            return Some(outbound::error(&PusherError::Unauthorized));
        }
    }

    // Re-subscribing is idempotent and must not count against the cap.
    if !conn.is_subscribed(channel_name)
        && conn.subscription_count() >= app.max_channels_per_connection as usize
    {
        return Some(outbound::error_with_message(
            4301,
            "Connection is subscribed to the maximum number of channels",
        ));
    }

    let presence = if kind.is_presence() {
        let Some(cd_str) = channel_data else {
            return Some(outbound::error(&PusherError::InvalidMessageFormat));
        };
        if cd_str.len() > app.max_presence_member_size_bytes as usize {
            return Some(outbound::error_with_message(
                4301,
                "Presence member data exceeds limit",
            ));
        }
        let parsed: Value = serde_json::from_str(cd_str)
            .map_err(|_| ())
            .unwrap_or(Value::Null);
        let user_id = parsed.get("user_id").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        });
        let Some(user_id) = user_id else {
            return Some(outbound::error(&PusherError::InvalidMessageFormat));
        };
        let user_info = parsed.get("user_info").cloned();
        Some(PresenceMember { user_id, user_info })
    } else {
        None
    };

    let presence_cache = state.dispatcher.presence_cache();
    let occupied_elsewhere = occupied_on_peers(state, app, channel_name, kind);
    // Read under the channel's transition lock (in `admit`), like every peer
    // presence update is applied: the roster sent to this joiner then
    // matches the member_added/member_removed stream that follows it.
    let remote_members = std::cell::RefCell::new(Vec::new());
    let remotely_present = std::cell::Cell::new(false);
    let cap = app.max_presence_members_per_channel as usize;
    let admit = |ch: &Channel| {
        let Some(pm) = presence.as_ref() else {
            return true;
        };
        *remote_members.borrow_mut() =
            presence_cache.remote_members_for(app.id.as_str(), channel_name);
        let remote_members = remote_members.borrow();
        remotely_present.set(presence_cache.is_present_excluding(
            app.id.as_str(),
            channel_name,
            &pm.user_id,
            None,
        ));
        if ch.has_user_id(&pm.user_id) || remotely_present.get() {
            return true;
        }
        // The cap is fleet-wide: distinct users here plus those only on peers.
        let remote_only: std::collections::HashSet<&str> = remote_members
            .iter()
            .map(|m| m.user_id.as_str())
            .filter(|id| !ch.has_user_id(id))
            .collect();
        ch.user_count() + remote_only.len() < cap
    };
    // Every consequence of the join — local frames, peer notification,
    // webhooks — is issued here, under the channel's transition lock, so a
    // concurrent leave on this node cannot overtake it locally, on the bus
    // (a FIFO queue) or in the webhook stream.
    let on_subscribed = |ch: &Channel, r: &SubscribeResult| {
        if !r.was_new {
            return;
        }
        if r.member_count == 1 && !occupied_elsewhere {
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::ChannelOccupied {
                    channel: channel_name.to_string(),
                },
            );
        }
        let Some(pm) = presence.as_ref() else {
            announce_subscription_count(state, app, ch, r.member_count, false);
            return;
        };
        if !r.user_added {
            return;
        }
        // A user already present on another node was announced by that node.
        if !remotely_present.get() {
            let frame = outbound::member_added(channel_name, &pm.user_id, pm.user_info.as_ref());
            ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), Some(&conn.socket_id));
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::MemberAdded {
                    channel: channel_name.to_string(),
                    user_id: pm.user_id.clone(),
                },
            );
        }
        // Always propagate to peer nodes — they dedupe against their own
        // local + remote state before emitting.
        state.dispatcher.publish_member_added(
            app,
            channel_name.to_string(),
            pm.user_id.clone(),
            pm.user_info.clone(),
        );
    };
    let Some(outcome) = state.channels.subscribe_with(
        app,
        &name,
        conn.socket_id.clone(),
        handle.clone(),
        presence.clone(),
        admit,
        on_subscribed,
    ) else {
        return Some(outbound::error_with_message(
            4301,
            "Presence channel is over capacity",
        ));
    };
    conn.add_subscription(channel_name);
    let remote_members = remote_members.into_inner();

    let succeeded = if kind.is_presence() {
        let merged = outcome.presence_snapshot.as_ref().map(|local| {
            if remote_members.is_empty() {
                local.clone()
            } else {
                merge_presence(local, remote_members)
            }
        });
        match merged {
            Some(p) => outbound::subscription_succeeded_with_presence(channel_name, &p),
            None => outbound::subscription_succeeded(channel_name, None),
        }
    } else {
        outbound::subscription_succeeded(channel_name, None)
    };

    // The caller writes subscription_succeeded before draining this queue.
    // A cache hit must not bypass subscription or presence notifications.
    if kind.is_cache() {
        if let Some(payload) = outcome.cached_payload {
            let _ = handle.try_send(Outbound::Text(payload));
        } else {
            // Several clients joining an empty channel yield one webhook.
            let notify = state
                .channels
                .find_channel(&app.id, channel_name)
                .is_some_and(|ch| ch.claim_cache_miss_notification());
            if notify {
                state.webhooks.enqueue(
                    app.id.as_str(),
                    WebhookEvent::CacheMiss {
                        channel: channel_name.to_string(),
                    },
                );
            }
            let miss = outbound::cache_miss(channel_name);
            let _ = handle.try_send(Outbound::Text(Arc::from(miss.into_boxed_str())));
        }
    }

    Some(succeeded)
}

/// Whether any other node reports subscribers on `channel`. Occupied and
/// vacated webhooks are fleet transitions, not per-node ones.
fn occupied_on_peers(state: &ServerState, app: &AppArc, channel: &str, kind: ChannelKind) -> bool {
    if kind.is_presence() {
        !state
            .dispatcher
            .presence_cache()
            .remote_members_for(app.id.as_str(), channel)
            .is_empty()
    } else {
        state
            .dispatcher
            .peer_channel_counts()
            .sum(app.id.as_str(), channel)
            > 0
    }
}

/// Shares a non-presence channel's new local count with peers (always:
/// fleet-wide channel stats and occupied/vacated webhooks depend on it)
/// and, when the app opted in, emits the fleet total to subscribers and
/// webhooks. Called under the channel's transition lock so counts reach
/// peers in the order they changed.
fn announce_subscription_count(
    state: &ServerState,
    app: &AppArc,
    ch: &Channel,
    local_count: usize,
    vacated_withheld: bool,
) {
    let channel_name = ch.name().as_str();
    if app.emit_subscription_count {
        let peer_sum = state
            .dispatcher
            .peer_channel_counts()
            .sum(app.id.as_str(), channel_name);
        let total = local_count + peer_sum;
        let frame = outbound::subscription_count(channel_name, total);
        ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
        state.webhooks.enqueue(
            app.id.as_str(),
            WebhookEvent::SubscriptionCount {
                channel: channel_name.to_string(),
                count: total,
            },
        );
    }
    state.dispatcher.publish_subscription_count(
        app,
        channel_name.to_string(),
        local_count,
        vacated_withheld,
    );
}

async fn handle_unsubscribe(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    data: &Value,
) -> Option<String> {
    let data_obj = data_object(data);
    let channel_name = data_obj.get("channel").and_then(|v| v.as_str())?;
    conn.remove_subscription(channel_name);
    leave_channel(state, app, &conn.socket_id, channel_name);
    None
}

/// Removes `socket_id` from `channel_name` and emits everything that
/// follows from it: `member_removed` (under the channel's transition lock),
/// peer updates, subscription counts and fleet-aware webhooks.
fn leave_channel(state: &ServerState, app: &AppArc, socket_id: &SocketId, channel_name: &str) {
    let kind = ChannelKind::from_name(channel_name);
    let presence_cache = state.dispatcher.presence_cache();
    // Everything that follows from the leave is issued under the channel's
    // transition lock, in the same order a concurrent join would observe.
    let on_unsubscribed = |ch: &Channel, outcome: &UnsubscribeOutcome| {
        if !outcome.was_member {
            return;
        }
        // A leave that is not fleet-wide withholds its webhook; the peer's
        // own leave reports it, or — if both leave at once — the flags on
        // the bus let exactly one of them report it.
        let vacated_withheld =
            outcome.member_count == 0 && occupied_on_peers(state, app, channel_name, kind);
        if vacated_withheld {
            state.dispatcher.note_withheld(app, channel_name, None);
        } else if outcome.member_count == 0 {
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::ChannelVacated {
                    channel: channel_name.to_string(),
                },
            );
        }
        if !kind.is_presence() {
            announce_subscription_count(state, app, ch, outcome.member_count, vacated_withheld);
            return;
        }
        let Some(user_id) = outcome.user_removed.as_deref() else {
            return;
        };
        // Only announce the leave if this user isn't still online on a peer.
        let still_remotely =
            presence_cache.is_present_excluding(app.id.as_str(), channel_name, user_id, None);
        if still_remotely {
            state
                .dispatcher
                .note_withheld(app, channel_name, Some(user_id));
        } else {
            let frame = outbound::member_removed(channel_name, user_id);
            ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
            state.webhooks.enqueue(
                app.id.as_str(),
                WebhookEvent::MemberRemoved {
                    channel: channel_name.to_string(),
                    user_id: user_id.to_string(),
                },
            );
        }
        // Always publish so peers can dedupe.
        state.dispatcher.publish_member_removed(
            app,
            channel_name.to_string(),
            user_id.to_string(),
            still_remotely,
            vacated_withheld,
        );
    };
    state
        .channels
        .unsubscribe_with(&app.id, channel_name, socket_id, on_unsubscribed);
}

async fn handle_signin(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    handle: &ConnectionHandle,
    data: &Value,
) -> Option<String> {
    let data_obj = data_object(data);
    let Some(auth) = data_obj.get("auth").and_then(|v| v.as_str()) else {
        return Some(outbound::error(&PusherError::Unauthorized));
    };
    let Some(user_data) = data_obj.get("user_data").and_then(|v| v.as_str()) else {
        return Some(outbound::error(&PusherError::Unauthorized));
    };
    if verify_user_auth(
        app.key.as_str(),
        conn.socket_id.as_str(),
        user_data,
        auth,
        &app.secret,
    )
    .is_err()
    {
        return Some(outbound::error(&PusherError::Unauthorized));
    }
    let parsed: Value = serde_json::from_str(user_data).unwrap_or(Value::Null);
    let Some(user_id) = parsed.get("id").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }) else {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    };
    if user_id.is_empty() {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    }
    // A socket's authenticated identity is immutable. Rebinding leaves the
    // old user index pointing at this socket and leaks user-targeted events.
    if let Some(existing_user) = conn.user_id() {
        return Some(if existing_user == user_id {
            outbound::signin_success(user_data)
        } else {
            outbound::error(&PusherError::Unauthorized)
        });
    }
    // Validate the watchlist BEFORE binding — malformed input must fail
    // atomically, not leave the user half-registered (online in the user
    // index, advertised to peers, but with the client seeing an error).
    let mut watchlist_truncated = false;
    let watchlist_list: Option<Vec<String>> = match parsed.get("watchlist") {
        None => None,
        Some(Value::Array(arr)) => {
            // Pusher protocol: "the first 100 user IDs will be accepted and
            // the sign-in operation will succeed", plus a 4302 error.
            watchlist_truncated = arr.len() > MAX_WATCHLIST_SIZE;
            Some(
                arr.iter()
                    .take(MAX_WATCHLIST_SIZE)
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
            )
        }
        Some(_) => {
            return Some(outbound::error(&PusherError::InvalidMessageFormat));
        }
    };

    // The online transition and everything it announces happen under the
    // user's lock, so a concurrent disconnect of this user's last socket
    // cannot be announced after it.
    state.channels.with_user_transition(&app.id, &user_id, || {
        let was_online_locally = state.channels.is_user_online(&app.id, &user_id);
        let was_online_remotely = state.dispatcher.peer_user_sessions().is_present_excluding(
            app.id.as_str(),
            &user_id,
            None,
        );
        conn.bind_user(user_id.clone());
        let first_local_socket = state.channels.bind_user(&app.id, &user_id, &conn.socket_id);

        if let Some(list) = watchlist_list {
            conn.set_watchlist(list.clone());
            for watched in &list {
                state.channels.add_watcher(&app.id, watched, &user_id);
            }
            let online: Vec<String> = list
                .iter()
                .filter(|u| {
                    state.channels.is_user_online(&app.id, u)
                        || state.dispatcher.peer_user_sessions().is_present_excluding(
                            app.id.as_str(),
                            u,
                            None,
                        )
                })
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

        // Global 0→1 transition: emit "online" to local watchers.
        if !was_online_locally && !was_online_remotely {
            fanout_watchlist_event(state, app, &user_id, "online");
        }
        // First socket for this user on this node → tell peers.
        if first_local_socket {
            state.dispatcher.publish_user_online(app, user_id.clone());
        }
    });
    if watchlist_truncated {
        // Queued, so it follows the signin_success written by the caller.
        let frame = outbound::error_with_message(
            4302,
            "Watchlist exceeds 100 users; only the first 100 are watched",
        );
        let _ = handle.try_send(Outbound::Text(Arc::from(frame.into_boxed_str())));
    }

    Some(outbound::signin_success(user_data))
}

async fn handle_client_event(
    state: &ServerState,
    app: &AppArc,
    conn: &Arc<Connection>,
    event: &str,
    frame: &zatat_protocol::envelope::InboundFrame,
) -> Option<String> {
    if app.accept_client_events_from == AcceptClientEventsFrom::None {
        return Some(outbound::error(&PusherError::ClientEventsDisabled));
    }
    let Some(channel_name) = frame.channel.as_deref() else {
        return Some(outbound::error(&PusherError::InvalidMessageFormat));
    };
    let kind = ChannelKind::from_name(channel_name);
    if !kind.is_private() {
        return Some(outbound::error(&PusherError::ClientEventsDisabled));
    }
    let Some(channel) = state.channels.find_channel(&app.id, channel_name) else {
        return Some(outbound::error(&PusherError::NotChannelMember));
    };
    if !channel.contains(conn.socket_id.as_str()) {
        return Some(outbound::error(&PusherError::NotChannelMember));
    }
    // Non-private channels were rejected above; Pusher only allows client
    // events on private and presence channels, from members.
    let data_str = match &frame.data {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    // Pusher attaches the sender's presence user_id so recipients know who
    // sent the event (Echo exposes it to `listenForWhisper`).
    let sender_user_id = if kind.is_presence() {
        channel.presence_user_id(conn.socket_id.as_str())
    } else {
        None
    };
    let encoded = zatat_protocol::envelope::encode_client_event(
        event,
        &data_str,
        channel_name,
        sender_user_id.as_deref(),
    );
    // Relayed but never cached: cache channels replay server events only.
    channel.broadcast_client_event(Arc::from(encoded.into_boxed_str()), Some(&conn.socket_id));

    // Fan out to peer nodes via the scaling bus so tabs connected to other
    // zatat nodes behind the LB also see this event. Without this, two tabs
    // hashed to different nodes never see each other's client-* frames.
    state
        .dispatcher
        .publish_client_event(
            app,
            channel_name.to_string(),
            event.to_string(),
            data_str.clone(),
            conn.socket_id.clone(),
            sender_user_id.clone(),
        )
        .await;

    state.webhooks.enqueue(
        app.id.as_str(),
        WebhookEvent::ClientEvent {
            channel: channel_name.to_string(),
            event: event.to_string(),
            data: data_str,
            socket_id: Some(conn.socket_id.as_str().to_string()),
            user_id: sender_user_id.or_else(|| conn.user_id()),
        },
    );
    None
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
    for name in conn.subscriptions_snapshot() {
        leave_channel(state, app, socket_id, &name);
    }
    if let Some(user_id) = conn.user_id() {
        // Under the user's lock: see `handle_signin`.
        state.channels.with_user_transition(&app.id, &user_id, || {
            let last_socket = state.channels.unbind_user(&app.id, &user_id, socket_id);
            if !last_socket {
                return;
            }
            // Only fire offline to local watchers if user isn't online on any peer.
            let still_remotely = state.dispatcher.peer_user_sessions().is_present_excluding(
                app.id.as_str(),
                &user_id,
                None,
            );
            if !still_remotely {
                fanout_watchlist_event(state, app, &user_id, "offline");
            }
            state.dispatcher.publish_user_offline(app, user_id.clone());
            state.channels.remove_all_watches_for(&app.id, &user_id);
        });
    }
    state.channels.unregister_connection(&app.id, socket_id);
}

fn fanout_watchlist_event(state: &ServerState, app: &AppArc, user_id: &str, event_name: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use zatat_config::Config;
    use zatat_core::id::AppId;
    use zatat_protocol::auth::sign_user_auth;

    fn setup() -> (
        ServerState,
        AppArc,
        Arc<Connection>,
        ConnectionHandle,
        mpsc::Receiver<Outbound>,
    ) {
        let config_path = std::env::temp_dir().join(format!(
            "zatat-handler-test-{}.toml",
            SocketId::generate().as_str()
        ));
        std::fs::write(
            &config_path,
            r#"
[[apps]]
id = "app"
key = "key"
secret = "secret"
"#,
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        std::fs::remove_file(config_path).unwrap();
        let app = config.app_by_id(&AppId::from("app")).unwrap();
        let state = crate::state::ServerStateInner::new(config);
        let socket_id = SocketId::generate();
        let conn = Arc::new(Connection::new(app.clone(), socket_id.clone(), None));
        let (tx, rx) = mpsc::channel(16);
        let handle =
            ConnectionHandle::from_parts(socket_id, tx, Arc::new(tokio::sync::Notify::new()));
        state
            .channels
            .register_connection(app.clone(), handle.clone())
            .unwrap();
        (state, app, conn, handle, rx)
    }

    async fn signin(
        state: &ServerState,
        app: &AppArc,
        conn: &Arc<Connection>,
        handle: &ConnectionHandle,
        user: &str,
    ) -> Value {
        let user_data = json!({"id": user}).to_string();
        let auth = sign_user_auth(
            app.key.as_str(),
            conn.socket_id.as_str(),
            &user_data,
            &app.secret,
        );
        let result = handle_signin(
            state,
            app,
            conn,
            handle,
            &json!({"auth": auth, "user_data": user_data}),
        )
        .await;
        let Some(frame) = result else {
            panic!("expected signin response")
        };
        serde_json::from_str(&frame).unwrap()
    }

    #[tokio::test]
    async fn signin_cannot_rebind_identity_and_cleanup_removes_original_user() {
        let (state, app, conn, handle, _rx) = setup();
        assert_eq!(
            signin(&state, &app, &conn, &handle, "alice").await["event"],
            "pusher:signin_success"
        );
        assert_eq!(
            signin(&state, &app, &conn, &handle, "alice").await["event"],
            "pusher:signin_success"
        );
        assert_eq!(
            state.channels.connections_for_user(&app.id, "alice").len(),
            1
        );
        assert_eq!(
            signin(&state, &app, &conn, &handle, "bob").await["event"],
            "pusher:error"
        );
        assert_eq!(conn.user_id().as_deref(), Some("alice"));
        assert!(state
            .channels
            .connections_for_user(&app.id, "bob")
            .is_empty());
        cleanup_connection(&state, &app, &conn, &conn.socket_id).await;
        assert!(!state.channels.is_user_online(&app.id, "alice"));
        assert_eq!(state.channels.connection_count(&app.id), 0);
    }

    #[tokio::test]
    async fn cache_hit_acknowledges_subscription_before_queued_replay() {
        let (state, app, conn, handle, mut rx) = setup();
        let cached =
            Arc::<str>::from(r#"{"event":"update","channel":"cache-news","data":"hello"}"#);
        state
            .channels
            .get_or_create_cache_channel(&app, "cache-news")
            .unwrap()
            .set_cached_payload(cached.clone());
        let response = handle_subscribe(
            &state,
            &app,
            &conn,
            &handle,
            &json!({"channel": "cache-news"}),
        )
        .await;
        let Some(frame) = response else {
            panic!("expected subscription response")
        };
        let frame: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["event"], "pusher_internal:subscription_succeeded");
        let Outbound::Text(replay) = rx.try_recv().unwrap() else {
            panic!("expected replay")
        };
        assert_eq!(replay, cached);
        assert!(rx.try_recv().is_err());
    }

    fn connect(
        state: &ServerState,
        app: &AppArc,
    ) -> (Arc<Connection>, ConnectionHandle, mpsc::Receiver<Outbound>) {
        let socket_id = SocketId::generate();
        let conn = Arc::new(Connection::new(app.clone(), socket_id.clone(), None));
        let (tx, rx) = mpsc::channel(256);
        let handle =
            ConnectionHandle::from_parts(socket_id, tx, Arc::new(tokio::sync::Notify::new()));
        state
            .channels
            .register_connection(app.clone(), handle.clone())
            .unwrap();
        (conn, handle, rx)
    }

    async fn join_presence(
        state: &ServerState,
        app: &AppArc,
        conn: &Arc<Connection>,
        handle: &ConnectionHandle,
        channel: &str,
        user: &str,
    ) -> Value {
        let channel_data = json!({"user_id": user, "user_info": {"name": user}}).to_string();
        let auth = zatat_protocol::auth::sign_channel_auth(
            app.key.as_str(),
            conn.socket_id.as_str(),
            channel,
            Some(&channel_data),
            &app.secret,
        );
        let frame = handle_subscribe(
            state,
            app,
            conn,
            handle,
            &json!({"channel": channel, "auth": auth, "channel_data": channel_data}),
        )
        .await
        .expect("subscribe response");
        serde_json::from_str(&frame).unwrap()
    }

    fn drain(rx: &mut mpsc::Receiver<Outbound>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(Outbound::Text(text)) = rx.try_recv() {
            out.push(serde_json::from_str(&text).unwrap());
        }
        out
    }

    /// Regression: member_added used to be broadcast from a spawned task
    /// that never re-checked membership, so a join immediately followed by a
    /// disconnect could reach peers as removed-then-added — a ghost member.
    #[tokio::test]
    async fn join_then_immediate_disconnect_reaches_peers_in_order() {
        let (state, app, observer, observer_handle, mut observer_rx) = setup();
        join_presence(
            &state,
            &app,
            &observer,
            &observer_handle,
            "presence-room",
            "watcher",
        )
        .await;
        drain(&mut observer_rx);

        let (conn, handle, _rx) = connect(&state, &app);
        let joined = join_presence(&state, &app, &conn, &handle, "presence-room", "ghost").await;
        assert_eq!(joined["event"], "pusher_internal:subscription_succeeded");
        cleanup_connection(&state, &app, &conn, &conn.socket_id).await;
        tokio::task::yield_now().await;

        let events: Vec<String> = drain(&mut observer_rx)
            .iter()
            .map(|v| v["event"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            events,
            vec![
                "pusher_internal:member_added".to_string(),
                "pusher_internal:member_removed".to_string()
            ]
        );
    }

    /// Presence whispers carry the sender's user_id (Pusher / Reverb), and
    /// client events never replace a cache channel's stored event.
    #[tokio::test]
    async fn client_events_carry_presence_user_id_and_are_not_cached() {
        let (state, app, alice, alice_handle, _alice_rx) = setup();
        let (bob, bob_handle, mut bob_rx) = connect(&state, &app);
        for (conn, handle, user) in [(&alice, &alice_handle, "alice"), (&bob, &bob_handle, "bob")] {
            join_presence(&state, &app, conn, handle, "presence-cache-room", user).await;
        }
        state
            .channels
            .find_channel(&app.id, "presence-cache-room")
            .unwrap()
            .set_cached_payload(Arc::from(r#"{"event":"server","data":"1"}"#));
        drain(&mut bob_rx);

        let frame = zatat_protocol::envelope::parse_inbound(
            r#"{"event":"client-typing","channel":"presence-cache-room","data":{"t":1}}"#,
        )
        .unwrap();
        assert!(
            handle_client_event(&state, &app, &alice, "client-typing", &frame)
                .await
                .is_none()
        );
        let received = drain(&mut bob_rx);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["event"], "client-typing");
        assert_eq!(received[0]["user_id"], "alice");
        assert_eq!(
            state
                .channels
                .find_channel(&app.id, "presence-cache-room")
                .unwrap()
                .cached_payload()
                .as_deref(),
            Some(r#"{"event":"server","data":"1"}"#)
        );
    }

    #[tokio::test]
    async fn subscriptions_per_connection_are_capped() {
        let (state, app, conn, handle, _rx) = setup();
        let cap = app.max_channels_per_connection as usize;
        for i in 0..cap {
            let frame = handle_subscribe(
                &state,
                &app,
                &conn,
                &handle,
                &json!({"channel": format!("ch-{i}")}),
            )
            .await
            .unwrap();
            assert!(frame.contains("subscription_succeeded"), "{frame}");
        }
        let over = handle_subscribe(
            &state,
            &app,
            &conn,
            &handle,
            &json!({"channel": "one-more"}),
        )
        .await
        .unwrap();
        assert!(over.contains("pusher:error"), "{over}");
        // Re-subscribing to a channel already held stays idempotent.
        let again = handle_subscribe(&state, &app, &conn, &handle, &json!({"channel": "ch-0"}))
            .await
            .unwrap();
        assert!(again.contains("subscription_succeeded"), "{again}");
        assert!(state.channels.find_channel(&app.id, "one-more").is_none());
    }

    /// Pusher protocol: an oversized watchlist keeps the first 100 entries,
    /// sign-in succeeds, and a 4302 error follows — the socket stays open.
    #[tokio::test]
    async fn oversized_watchlist_is_truncated_not_fatal() {
        let (state, app, conn, handle, mut rx) = setup();
        let watchlist: Vec<String> = (0..150).map(|i| format!("u{i}")).collect();
        let user_data = json!({"id": "alice", "watchlist": watchlist}).to_string();
        let auth = sign_user_auth(
            app.key.as_str(),
            conn.socket_id.as_str(),
            &user_data,
            &app.secret,
        );
        let response = handle_signin(
            &state,
            &app,
            &conn,
            &handle,
            &json!({"auth": auth, "user_data": user_data}),
        )
        .await
        .unwrap();
        assert!(response.contains("pusher:signin_success"), "{response}");
        assert_eq!(conn.watchlist().len(), 100);
        assert_eq!(conn.watchlist()[99], "u99");
        let queued = drain(&mut rx);
        let error = queued
            .iter()
            .find(|v| v["event"] == "pusher:error")
            .expect("4302 error queued");
        let data: Value = serde_json::from_str(error["data"].as_str().unwrap()).unwrap();
        assert_eq!(data["code"], 4302);
    }

    struct RecordingProvider(parking_lot::Mutex<Vec<Vec<u8>>>);

    #[async_trait::async_trait]
    impl zatat_scaling::PubSubProvider for RecordingProvider {
        async fn publish(&self, payload: Vec<u8>) {
            self.0.lock().push(payload);
        }
        async fn subscribe(&self) -> Result<tokio::sync::broadcast::Receiver<Vec<u8>>, String> {
            Ok(tokio::sync::broadcast::channel(1).1)
        }
    }

    /// Regression: bus notifications were queued after the transition lock
    /// was released, so a user's last disconnect racing a reconnect on the
    /// same node could reach peers as member_added *then* member_removed —
    /// peers would drop a user who is online. The last presence message on
    /// the bus must always match the final membership.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_leave_and_rejoin_reach_peers_in_order() {
        let config_path = std::env::temp_dir().join(format!(
            "zatat-order-{}.toml",
            SocketId::generate().as_str()
        ));
        std::fs::write(
            &config_path,
            "[[apps]]\nid = \"app\"\nkey = \"key\"\nsecret = \"secret\"\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        std::fs::remove_file(config_path).unwrap();
        let app = config.app_by_id(&AppId::from("app")).unwrap();
        let provider = Arc::new(RecordingProvider(parking_lot::Mutex::new(Vec::new())));
        let state = crate::state::ServerStateInner::with_provider(config, provider.clone(), true);

        const ROUNDS: usize = 300;
        for round in 0..ROUNDS {
            let channel = format!("presence-race-{round}");
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let (old, old_handle, _old_rx) = connect(&state, &app);
            join_presence(&state, &app, &old, &old_handle, &channel, "u").await;
            let (new, new_handle, _new_rx) = connect(&state, &app);
            let leave = {
                let (state, app, old, barrier) =
                    (state.clone(), app.clone(), old.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    cleanup_connection(&state, &app, &old, &old.socket_id).await;
                })
            };
            let join = {
                let (state, app, new, channel, barrier) = (
                    state.clone(),
                    app.clone(),
                    new.clone(),
                    channel.clone(),
                    barrier.clone(),
                );
                tokio::spawn(async move {
                    barrier.wait().await;
                    join_presence(&state, &app, &new, &new_handle, &channel, "u").await;
                })
            };
            leave.await.unwrap();
            join.await.unwrap();
        }
        // Let the publisher drain.
        for _ in 0..300 {
            if state.dispatcher.pending_publishes() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut last: std::collections::HashMap<String, String> = Default::default();
        for bytes in provider.0.lock().iter() {
            let env = zatat_scaling::message::parse(bytes).unwrap();
            match env.payload {
                zatat_scaling::ScalingPayload::MemberAdded { channel, .. } => {
                    last.insert(channel, "added".into());
                }
                zatat_scaling::ScalingPayload::MemberRemoved { channel, .. } => {
                    last.insert(channel, "removed".into());
                }
                _ => {}
            }
        }
        for round in 0..ROUNDS {
            let channel = format!("presence-race-{round}");
            assert!(state
                .channels
                .find_channel(&app.id, &channel)
                .unwrap()
                .has_user_id("u"));
            assert_eq!(
                last.get(&channel).map(String::as_str),
                Some("added"),
                "{channel}"
            );
        }
    }

    /// Regression: losing a user's last socket and publishing `offline` was
    /// not atomic with a new sign-in, so a reconnect could reach peers as
    /// online-then-offline and mark a connected user offline. The last
    /// online/offline message on the bus must match the final state.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_disconnect_and_signin_reach_peers_in_order() {
        let config_path = std::env::temp_dir().join(format!(
            "zatat-user-order-{}.toml",
            SocketId::generate().as_str()
        ));
        std::fs::write(
            &config_path,
            "[[apps]]\nid = \"app\"\nkey = \"key\"\nsecret = \"secret\"\n",
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        std::fs::remove_file(config_path).unwrap();
        let app = config.app_by_id(&AppId::from("app")).unwrap();
        let provider = Arc::new(RecordingProvider(parking_lot::Mutex::new(Vec::new())));
        let state = crate::state::ServerStateInner::with_provider(config, provider.clone(), true);

        const ROUNDS: usize = 300;
        let mut survivors = Vec::new();
        for round in 0..ROUNDS {
            let user = format!("user-{round}");
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let (old, old_handle, _old_rx) = connect(&state, &app);
            signin(&state, &app, &old, &old_handle, &user).await;
            let (new, new_handle, new_rx) = connect(&state, &app);
            let leave = {
                let (state, app, old, barrier) =
                    (state.clone(), app.clone(), old.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    cleanup_connection(&state, &app, &old, &old.socket_id).await;
                })
            };
            let join = {
                let (state, app, new, user, barrier) = (
                    state.clone(),
                    app.clone(),
                    new.clone(),
                    user.clone(),
                    barrier.clone(),
                );
                tokio::spawn(async move {
                    barrier.wait().await;
                    signin(&state, &app, &new, &new_handle, &user).await;
                })
            };
            leave.await.unwrap();
            join.await.unwrap();
            survivors.push((new, new_rx));
        }
        for _ in 0..300 {
            if state.dispatcher.pending_publishes() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut last: std::collections::HashMap<String, &str> = Default::default();
        for bytes in provider.0.lock().iter() {
            match zatat_scaling::message::parse(bytes).unwrap().payload {
                zatat_scaling::ScalingPayload::UserOnline { user_id, .. } => {
                    last.insert(user_id, "online");
                }
                zatat_scaling::ScalingPayload::UserOffline { user_id, .. } => {
                    last.insert(user_id, "offline");
                }
                _ => {}
            }
        }
        for round in 0..ROUNDS {
            let user = format!("user-{round}");
            assert!(state.channels.is_user_online(&app.id, &user));
            assert_eq!(last.get(&user).copied(), Some("online"), "{user}");
        }
    }
}
