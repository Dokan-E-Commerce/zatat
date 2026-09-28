use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::time;
use tracing::{debug, info, warn};

use zatat_core::application::AppArc;

use crate::state::ServerState;

const PRESENCE_HEARTBEAT: Duration = Duration::from_secs(5);
const PRESENCE_GC_INTERVAL: Duration = Duration::from_secs(5);
const CONNECTION_MAINTENANCE: Duration = Duration::from_secs(15);

pub async fn presence_snapshot_publisher(state: ServerState) {
    let mut tick = time::interval(PRESENCE_HEARTBEAT);
    tick.tick().await;

    loop {
        tick.tick().await;
        for app in state.config.apps().by_id.values() {
            publish_snapshots_for_app(&state, app).await;
        }
    }
}

async fn publish_snapshots_for_app(state: &ServerState, app: &AppArc) {
    let counts_seq = state.dispatcher.next_seq();
    let mut channel_counts: Vec<zatat_scaling::ChannelCount> = Vec::new();
    for channel in state.channels.channels(&app.id) {
        let channel_name = channel.name().as_str().to_string();
        if channel.kind().is_presence() {
            // The sequence is taken before capture: a join/leave published
            // after this point supersedes the snapshot on every peer.
            let seq = state.dispatcher.next_seq();
            let Some(local_roster) = channel.presence_snapshot() else {
                continue;
            };
            if local_roster.count == 0 {
                continue;
            }
            let members: Vec<zatat_scaling::PresenceSnapshotMember> = local_roster
                .hash
                .into_iter()
                .map(
                    |(user_id, user_info)| zatat_scaling::PresenceSnapshotMember {
                        user_id,
                        user_info: if user_info.is_null() {
                            None
                        } else {
                            Some(user_info)
                        },
                    },
                )
                .collect();
            debug!(
                app = %app.id,
                channel = %channel_name,
                members = members.len(),
                "publishing presence snapshot"
            );
            state
                .dispatcher
                .publish_presence_snapshot(app, channel_name, members, seq)
                .await;
        } else {
            // Always shared: fleet-wide channel stats and occupied/vacated
            // webhooks need peers' counts, not only subscription_count events.
            let count = channel.len();
            if count > 0 {
                channel_counts.push(zatat_scaling::ChannelCount {
                    channel: channel_name,
                    count,
                });
            }
        }
    }
    // Always sent, even empty: peers drop counts for channels it omits.
    state
        .dispatcher
        .publish_channel_count_snapshot(app, channel_counts, counts_seq);

    // Session snapshot — every user_id with at least one live local socket
    // (always sent: peers drop users it omits).
    let sessions_seq = state.dispatcher.next_seq();
    let user_ids = state.channels.local_user_ids(&app.id);
    state
        .dispatcher
        .publish_user_session_snapshot(app, user_ids, sessions_seq);
}

pub async fn presence_cache_gc(state: ServerState) {
    let mut tick = time::interval(PRESENCE_GC_INTERVAL);
    tick.tick().await;

    loop {
        tick.tick().await;

        // 1. Presence members that vanished (peer node crashed / stopped sending).
        let expired = state.dispatcher.presence_cache().gc_expired();
        for member in expired {
            let Some(app) = state
                .config
                .app_by_id(&zatat_core::id::AppId::from(member.app_id.as_str()))
            else {
                continue;
            };
            let Some(ch) = state.channels.find_channel(&app.id, &member.channel) else {
                continue;
            };
            // Decide and announce under the channel's transition lock so a
            // concurrent local join of the same user cannot be overtaken.
            let (gone, empty) = ch.with_transition(|| {
                let remaining = state
                    .dispatcher
                    .presence_cache()
                    .remote_members_for(member.app_id.as_str(), &member.channel);
                let still_remotely = remaining.iter().any(|m| m.user_id == member.user_id);
                if still_remotely || ch.has_user_id(&member.user_id) {
                    return (false, false);
                }
                let frame =
                    zatat_protocol::outbound::member_removed(&member.channel, &member.user_id);
                ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
                (true, ch.is_empty() && remaining.is_empty())
            });
            // The peer that held this user vanished without leaving; one
            // surviving node reports the fleet-wide transition.
            if gone && state.dispatcher.is_fleet_leader() {
                state.webhooks.enqueue(
                    app.id.as_str(),
                    zatat_webhooks::WebhookEvent::MemberRemoved {
                        channel: member.channel.clone(),
                        user_id: member.user_id.clone(),
                    },
                );
                if empty {
                    state.webhooks.enqueue(
                        app.id.as_str(),
                        zatat_webhooks::WebhookEvent::ChannelVacated {
                            channel: member.channel.clone(),
                        },
                    );
                }
            }
        }

        // 2. Non-presence sub counts from peers that went stale. Re-emit the
        //    updated total to any local subscribers on that channel.
        let expired_counts = state.dispatcher.peer_channel_counts().gc_expired();
        for (app_id, channel, _peer, stale_count) in expired_counts {
            let Some(app) = state
                .config
                .app_by_id(&zatat_core::id::AppId::from(app_id.as_str()))
            else {
                continue;
            };
            // A peer that held subscribers vanished; if nobody is left, one
            // surviving node reports the channel as vacated.
            let local = state.channels.find_channel(&app.id, &channel);
            if stale_count > 0
                && local.as_ref().is_none_or(|c| c.is_empty())
                && state
                    .dispatcher
                    .peer_channel_counts()
                    .sum(app_id.as_str(), &channel)
                    == 0
                && state.dispatcher.is_fleet_leader()
            {
                state.webhooks.enqueue(
                    app.id.as_str(),
                    zatat_webhooks::WebhookEvent::ChannelVacated {
                        channel: channel.clone(),
                    },
                );
            }
            let Some(ch) = local else {
                continue;
            };
            if ch.kind().is_presence() || !app.emit_subscription_count {
                continue;
            }
            let total = ch.len()
                + state
                    .dispatcher
                    .peer_channel_counts()
                    .sum(app_id.as_str(), &channel);
            let frame = zatat_protocol::outbound::subscription_count(&channel, total);
            let payload: Arc<str> = Arc::from(frame.into_boxed_str());
            ch.broadcast_protocol(payload, None);
        }

        // 3. Watchlist user sessions on peers that went stale. If a user no
        //    longer has any live connection anywhere, emit offline to local
        //    watchers.
        let expired_sessions = state.dispatcher.peer_user_sessions().gc_expired();
        for (app_id, _peer, user_id) in expired_sessions {
            let Some(app) = state
                .config
                .app_by_id(&zatat_core::id::AppId::from(app_id.as_str()))
            else {
                continue;
            };
            state.channels.with_user_transition(&app.id, &user_id, || {
                let still_locally = state.channels.is_user_online(&app.id, &user_id);
                let still_remotely = state.dispatcher.peer_user_sessions().is_present_excluding(
                    app_id.as_str(),
                    &user_id,
                    None,
                );
                if still_locally || still_remotely {
                    return;
                }
                let watchers = state.channels.watchers_of(&app.id, &user_id);
                if watchers.is_empty() {
                    return;
                }
                let frame = zatat_protocol::envelope::encode_envelope(
                    "pusher_internal:watchlist_events",
                    Some(&serde_json::json!({
                        "events": [{
                            "name": "offline",
                            "user_ids": [user_id.clone()],
                        }]
                    })),
                    None,
                );
                let payload: Arc<str> = Arc::from(frame.into_boxed_str());
                for watcher in watchers {
                    for h in state.channels.connections_for_user(&app.id, &watcher) {
                        let _ = h.try_send(zatat_connection::Outbound::Text(payload.clone()));
                    }
                }
            });
        }

        state.dispatcher.gc_peer_sequences();
    }
}

pub async fn connection_maintenance(state: ServerState, tracker: ConnectionTracker) {
    let mut tick = time::interval(CONNECTION_MAINTENANCE);
    tick.tick().await;

    loop {
        tick.tick().await;
        tracker.for_each(|entry| {
            if entry.is_stale() {
                let _ = entry.handle.try_send(zatat_connection::Outbound::Close {
                    code: 4201,
                    reason: "Pong reply not received in time".into(),
                });
            } else if entry.is_inactive() {
                let _ = entry.handle.try_send(zatat_connection::Outbound::Ping);
                entry.conn.mark_pinged();
            }
        });
        state.channels.gc_empty_cache_channels();
        for app_id in state.config.apps().by_id.keys() {
            metrics::gauge!(zatat_metrics::GAUGE_CHANNELS, "app" => app_id.as_str().to_string())
                .set(state.channels.channel_count(app_id) as f64);
        }
    }
}

#[derive(Default, Clone)]
pub struct ConnectionTracker {
    inner: std::sync::Arc<dashmap::DashMap<String, TrackedConnection>>,
}

#[derive(Clone)]
pub struct TrackedConnection {
    pub conn: std::sync::Arc<zatat_connection::Connection>,
    pub handle: zatat_connection::ConnectionHandle,
}

impl TrackedConnection {
    pub fn is_inactive(&self) -> bool {
        self.conn.is_inactive()
    }
    pub fn is_stale(&self) -> bool {
        self.conn.is_stale()
    }
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, tracked: TrackedConnection) {
        self.inner
            .insert(tracked.conn.socket_id.as_str().to_string(), tracked);
    }

    pub fn unregister(&self, socket_id: &zatat_core::id::SocketId) {
        self.inner.remove(socket_id.as_str());
    }

    pub fn for_each(&self, mut f: impl FnMut(&TrackedConnection)) {
        for t in self.inner.iter() {
            if t.handle.is_closed() {
                continue;
            }
            f(t.value());
        }
    }
}

/// Closes connections that an apps reload invalidated: every connection of
/// a removed or re-keyed app, and connections whose origin the app no
/// longer allows. Other settings (secret, limits, …) are read live by the
/// connection handler, so their connections stay up.
pub fn apply_apps_reload(state: &ServerState, reload: &zatat_config::AppsReload) {
    let close = |handle: &zatat_connection::ConnectionHandle, code: u16, reason: &str| {
        let _ = handle.try_send(zatat_connection::Outbound::Close {
            code,
            reason: reason.into(),
        });
    };
    for app_id in &reload.revoked {
        let handles = state.channels.connection_handles(app_id);
        warn!(app = %app_id, connections = handles.len(), "app removed or re-keyed; closing its connections");
        for handle in &handles {
            close(handle, 4001, "Application no longer exists for this key");
        }
    }
    for app_id in &reload.origins_changed {
        let Some(app) = state.config.app_by_id(app_id) else {
            continue;
        };
        let mut closed = 0usize;
        tracker_for_each_of_app(state, app_id, |tracked| {
            let allowed = match tracked.conn.origin.as_deref() {
                Some(origin) => app.origin_is_allowed(crate::router::url_host_or_self(origin)),
                None => true,
            };
            if !allowed {
                closed += 1;
                close(
                    &tracked.handle,
                    4009,
                    "Origin is no longer allowed for this application",
                );
            }
        });
        if closed > 0 {
            warn!(app = %app_id, closed, "allowed origins changed; closed connections from disallowed origins");
        }
    }
}

fn tracker_for_each_of_app(
    state: &ServerState,
    app_id: &zatat_core::id::AppId,
    mut f: impl FnMut(&TrackedConnection),
) {
    state.tracker.for_each(|tracked| {
        if &tracked.conn.app.id == app_id {
            f(tracked);
        }
    });
}

pub async fn restart_signal_watcher(state: ServerState) {
    let path = PathBuf::from(&state.config.server.restart_signal_file);
    let interval = state
        .config
        .server
        .restart_poll_interval
        .max(Duration::from_secs(1));
    let started_at = SystemTime::now();
    let mut tick = time::interval(interval);
    tick.tick().await;

    loop {
        tick.tick().await;
        match tokio::fs::metadata(&path).await {
            Ok(meta) => match meta.modified() {
                Ok(mtime) => {
                    if mtime > started_at {
                        // Starts the same drain as SIGTERM; the listener's
                        // shutdown future waits on it, so the process exits
                        // and the supervisor (systemd/docker) restarts it.
                        info!(file = %path.display(), "restart signal received, shutting down");
                        state.shutdown_now();
                        std::future::pending::<()>().await;
                    }
                }
                Err(err) => warn!(file = %path.display(), %err, "could not read mtime"),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(file = %path.display(), %err, "could not stat restart signal file"),
        }
    }
}
