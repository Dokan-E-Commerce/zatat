use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::time;
use tracing::{debug, info, warn};

use zatat_core::application::AppArc;

use crate::state::ServerState;

const PRESENCE_HEARTBEAT: Duration = Duration::from_secs(5);
const PRESENCE_GC_INTERVAL: Duration = Duration::from_secs(5);
const CONNECTION_MAINTENANCE: Duration = Duration::from_secs(60);

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
    for channel in state.channels.channels(&app.id) {
        if !channel.kind().is_presence() {
            continue;
        }
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
        let channel_name = channel.name().as_str().to_string();
        debug!(
            app = %app.id,
            channel = %channel_name,
            members = members.len(),
            "publishing presence snapshot"
        );
        state
            .dispatcher
            .publish_presence_snapshot(app, channel_name, members)
            .await;
    }
}

pub async fn presence_cache_gc(state: ServerState) {
    let mut tick = time::interval(PRESENCE_GC_INTERVAL);
    tick.tick().await;

    loop {
        tick.tick().await;
        let expired = state.dispatcher.presence_cache().gc_expired();
        if expired.is_empty() {
            continue;
        }
        for member in expired {
            let Some(app) = state
                .config
                .app_by_id(&zatat_core::id::AppId::from(member.app_id.as_str()))
            else {
                continue;
            };
            let still = state
                .dispatcher
                .presence_cache()
                .remote_members_for(member.app_id.as_str(), &member.channel);
            if still.iter().any(|m| m.user_id == member.user_id) {
                continue;
            }
            let frame = zatat_protocol::outbound::member_removed(&member.channel, &member.user_id);
            if let Some(ch) = state.channels.find_channel(&app.id, &member.channel) {
                let payload: Arc<str> = Arc::from(frame.into_boxed_str());
                ch.broadcast_protocol(payload, None);
            }
        }
    }
}

pub async fn connection_maintenance(_state: ServerState, tracker: ConnectionTracker) {
    let mut tick = time::interval(CONNECTION_MAINTENANCE);
    tick.tick().await;

    loop {
        tick.tick().await;
        let snapshot = tracker.snapshot();
        for entry in snapshot {
            if entry.is_stale() {
                let _ = entry.handle.try_send(zatat_connection::Outbound::Close {
                    code: 4201,
                    reason: "Pong reply not received in time".into(),
                });
            } else if entry.is_inactive() {
                let _ = entry.handle.try_send(zatat_connection::Outbound::Ping);
                entry.conn.mark_pinged();
            }
        }
    }
}

#[derive(Default, Clone)]
pub struct ConnectionTracker {
    inner: std::sync::Arc<parking_lot::RwLock<Vec<TrackedConnection>>>,
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
        self.inner.write().push(tracked);
    }

    pub fn unregister(&self, socket_id: &zatat_core::id::SocketId) {
        let mut w = self.inner.write();
        w.retain(|t| t.conn.socket_id.as_str() != socket_id.as_str());
    }

    pub fn snapshot(&self) -> Vec<TrackedConnection> {
        let r = self.inner.read();
        r.iter()
            .filter(|t| !t.handle.is_closed())
            .cloned()
            .collect()
    }
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
                        info!(file = %path.display(), "restart signal received, shutting down");
                        state.shutdown_now();
                        return;
                    }
                }
                Err(err) => warn!(file = %path.display(), %err, "could not read mtime"),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(file = %path.display(), %err, "could not stat restart signal file"),
        }
    }
}
