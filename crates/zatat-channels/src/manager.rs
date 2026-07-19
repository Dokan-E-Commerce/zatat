use std::collections::HashSet;
use std::sync::Arc;

use dashmap::DashMap;
use zatat_connection::ConnectionHandle;
use zatat_core::application::AppArc;
use zatat_core::channel_name::ChannelName;
use zatat_core::id::{AppId, SocketId};
use zatat_protocol::presence::PresenceMember;

use crate::channel::{Channel, UnsubscribeOutcome};

#[derive(Debug)]
pub enum ChannelManagerError {
    ConnectionLimitReached,
}

impl std::fmt::Display for ChannelManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelManagerError::ConnectionLimitReached => {
                f.write_str("connection limit reached for app")
            }
        }
    }
}

impl std::error::Error for ChannelManagerError {}

pub struct SubscribeOutcome {
    pub was_new: bool,
    pub user_added: bool,
    pub member_count: usize,
    pub presence_snapshot: Option<zatat_protocol::presence::PresenceData>,
    pub cached_payload: Option<Arc<str>>,
    pub kind: zatat_core::channel_name::ChannelKind,
}

#[derive(Default)]
pub struct AppChannels {
    pub connections: DashMap<String, ConnectionHandle>,
    pub channels: DashMap<String, Arc<Channel>>,
    pub user_index: DashMap<String, Vec<String>>, // user_id → Vec<socket_id>
    pub watchers: DashMap<String, HashSet<String>>, // watched_user_id → HashSet<watcher_user_id>
}

#[derive(Default, Clone)]
pub struct ChannelManager {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    apps: DashMap<String, Arc<AppChannels>>,
}

impl ChannelManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
        }
    }

    fn apps(&self) -> &DashMap<String, Arc<AppChannels>> {
        &self.inner.apps
    }

    fn app_slot(&self, app_id: &AppId) -> Arc<AppChannels> {
        self.inner
            .apps
            .entry(app_id.as_str().to_string())
            .or_insert_with(|| Arc::new(AppChannels::default()))
            .clone()
    }

    pub fn register_connection(
        &self,
        app: AppArc,
        handle: ConnectionHandle,
    ) -> Result<(), ChannelManagerError> {
        let slot = self.app_slot(&app.id);
        let socket_key = handle.socket_id().as_str().to_string();
        slot.connections.insert(socket_key.clone(), handle);
        if let Some(max) = app.max_connections {
            if slot.connections.len() as u32 > max {
                slot.connections.remove(&socket_key);
                return Err(ChannelManagerError::ConnectionLimitReached);
            }
        }
        Ok(())
    }

    pub fn unregister_connection(&self, app_id: &AppId, socket_id: &SocketId) {
        if let Some(slot) = self.apps().get(app_id.as_str()) {
            slot.connections.remove(socket_id.as_str());
        }
    }

    pub fn connection_count(&self, app_id: &AppId) -> usize {
        self.apps()
            .get(app_id.as_str())
            .map(|s| s.connections.len())
            .unwrap_or(0)
    }

    /// Find a live connection by socket id. None if this node doesn't host it.
    pub fn handle_for_socket(
        &self,
        app_id: &AppId,
        socket_id: &SocketId,
    ) -> Option<ConnectionHandle> {
        let slot = self.apps().get(app_id.as_str())?;
        slot.connections.get(socket_id.as_str()).map(|h| h.clone())
    }

    pub fn subscribe(
        &self,
        app: &AppArc,
        name: &ChannelName,
        socket_id: SocketId,
        handle: ConnectionHandle,
        presence: Option<PresenceMember>,
    ) -> SubscribeOutcome {
        let slot = self.app_slot(&app.id);
        let ttl = app.cache_ttl_seconds.map(std::time::Duration::from_secs);
        let name_owned = name.clone();
        let channel = slot
            .channels
            .entry(name.as_str().to_string())
            .or_insert_with(|| Arc::new(Channel::with_cache_ttl(name_owned, ttl)))
            .clone();
        let r = channel.subscribe(socket_id, handle, presence);
        let presence_snapshot = if channel.kind().is_presence() {
            channel.presence_snapshot()
        } else {
            None
        };
        let cached_payload = if channel.kind().is_cache() {
            channel.cached_payload()
        } else {
            None
        };
        SubscribeOutcome {
            was_new: r.was_new,
            user_added: r.user_added,
            member_count: r.member_count,
            presence_snapshot,
            cached_payload,
            kind: channel.kind(),
        }
    }

    pub fn unsubscribe(
        &self,
        app_id: &AppId,
        channel_name: &str,
        socket_id: &SocketId,
    ) -> Option<UnsubscribeOutcome> {
        let slot = self.apps().get(app_id.as_str())?;
        let channel = slot.channels.get(channel_name)?.clone();
        let outcome = channel.unsubscribe(socket_id);
        slot.channels.remove_if(channel_name, |_, ch| ch.is_empty());
        Some(outcome)
    }

    pub fn find_channel(&self, app_id: &AppId, channel_name: &str) -> Option<Arc<Channel>> {
        let slot = self.apps().get(app_id.as_str())?;
        slot.channels.get(channel_name).map(|c| c.clone())
    }

    /// Cache channels retain the last payload even when empty, so a call
    /// publishing with no subscribers still has somewhere to store it.
    /// No-op for non-cache channels.
    pub fn get_or_create_cache_channel(
        &self,
        app: &AppArc,
        channel_name: &str,
    ) -> Option<Arc<Channel>> {
        let kind = zatat_core::channel_name::ChannelKind::from_name(channel_name);
        if !kind.is_cache() {
            return self.find_channel(&app.id, channel_name);
        }
        let slot = self.app_slot(&app.id);
        let ttl = app.cache_ttl_seconds.map(std::time::Duration::from_secs);
        let name = ChannelName::new(channel_name.to_string());
        let ch = slot
            .channels
            .entry(channel_name.to_string())
            .or_insert_with(|| Arc::new(Channel::with_cache_ttl(name, ttl)))
            .clone();
        Some(ch)
    }

    /// Removes cache channels that have no subscribers and no live cached
    /// payload. Publishing to a `cache-*` channel with zero subscribers
    /// creates the channel via `get_or_create_cache_channel`, and nothing
    /// else ever removes it, so this must run periodically to bound growth.
    /// Returns the number of channels removed.
    pub fn gc_empty_cache_channels(&self) -> usize {
        let mut removed = 0;
        for app_slot in self.apps().iter() {
            let cache_channel_names: Vec<String> = app_slot
                .channels
                .iter()
                .filter(|e| e.value().kind().is_cache())
                .map(|e| e.key().clone())
                .collect();
            for name in cache_channel_names {
                if app_slot
                    .channels
                    .remove_if(&name, |_, ch| {
                        ch.is_empty() && ch.cached_payload().is_none()
                    })
                    .is_some()
                {
                    removed += 1;
                }
            }
        }
        removed
    }

    pub fn channels(&self, app_id: &AppId) -> Vec<Arc<Channel>> {
        match self.apps().get(app_id.as_str()) {
            Some(slot) => slot.channels.iter().map(|e| e.value().clone()).collect(),
            None => Vec::new(),
        }
    }

    pub fn channel_stats(
        &self,
        app_id: &AppId,
        channel_name: &str,
    ) -> Option<crate::channel::ChannelStats> {
        self.find_channel(app_id, channel_name).map(|c| c.stats())
    }

    /// Idempotent — safe to call repeatedly for the same (user, socket).
    /// Returns `true` if this was the first local socket for `user_id`.
    pub fn bind_user(&self, app_id: &AppId, user_id: &str, socket_id: &SocketId) -> bool {
        let slot = self.app_slot(app_id);
        let mut entry = slot.user_index.entry(user_id.to_string()).or_default();
        let was_empty = entry.is_empty();
        if !entry.iter().any(|s| s == socket_id.as_str()) {
            entry.push(socket_id.as_str().to_string());
        }
        was_empty
    }

    /// List every user_id with at least one local socket, for session snapshots.
    pub fn local_user_ids(&self, app_id: &AppId) -> Vec<String> {
        self.apps()
            .get(app_id.as_str())
            .map(|slot| {
                slot.user_index
                    .iter()
                    .filter(|e| !e.value().is_empty())
                    .map(|e| e.key().clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns `true` if the removed socket was that user's last one.
    pub fn unbind_user(&self, app_id: &AppId, user_id: &str, socket_id: &SocketId) -> bool {
        let Some(slot) = self.apps().get(app_id.as_str()) else {
            return false;
        };
        let mut was_last = false;
        let mut remove_key = false;
        if let Some(mut entry) = slot.user_index.get_mut(user_id) {
            entry.retain(|s| s != socket_id.as_str());
            if entry.is_empty() {
                was_last = true;
                remove_key = true;
            }
        }
        if remove_key {
            slot.user_index.remove(user_id);
        }
        was_last
    }

    pub fn connections_for_user(&self, app_id: &AppId, user_id: &str) -> Vec<ConnectionHandle> {
        let Some(slot) = self.apps().get(app_id.as_str()) else {
            return Vec::new();
        };
        let Some(socket_ids) = slot.user_index.get(user_id).map(|v| v.clone()) else {
            return Vec::new();
        };
        socket_ids
            .into_iter()
            .filter_map(|sid| slot.connections.get(&sid).map(|h| h.clone()))
            .collect()
    }

    pub fn is_user_online(&self, app_id: &AppId, user_id: &str) -> bool {
        self.apps()
            .get(app_id.as_str())
            .and_then(|slot| slot.user_index.get(user_id).map(|v| !v.is_empty()))
            .unwrap_or(false)
    }

    pub fn add_watcher(&self, app_id: &AppId, watched: &str, watcher: &str) {
        let slot = self.app_slot(app_id);
        slot.watchers
            .entry(watched.to_string())
            .or_default()
            .insert(watcher.to_string());
    }

    pub fn watchers_of(&self, app_id: &AppId, watched: &str) -> Vec<String> {
        self.apps()
            .get(app_id.as_str())
            .and_then(|slot| {
                slot.watchers
                    .get(watched)
                    .map(|s| s.iter().cloned().collect())
            })
            .unwrap_or_default()
    }

    pub fn remove_all_watches_for(&self, app_id: &AppId, watcher_user_id: &str) {
        let Some(slot) = self.apps().get(app_id.as_str()) else {
            return;
        };
        let to_drop: Vec<String> = slot.watchers.iter().map(|e| e.key().clone()).collect();
        for watched in to_drop {
            let mut empty = false;
            if let Some(mut s) = slot.watchers.get_mut(&watched) {
                s.remove(watcher_user_id);
                empty = s.is_empty();
            }
            if empty {
                slot.watchers.remove(&watched);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zatat_core::application::{AcceptClientEventsFrom, Application};
    use zatat_core::id::AppKey;

    fn mk_app(cache_ttl_seconds: Option<u64>) -> AppArc {
        Arc::new(
            Application::new(
                AppId::from("app"),
                AppKey::from("key"),
                "secret".into(),
                30,
                30,
                10_000,
                None,
                AcceptClientEventsFrom::Members,
                None,
                vec!["*".into()],
            )
            .unwrap()
            .with_cache_ttl_seconds(cache_ttl_seconds),
        )
    }

    #[test]
    fn gc_empty_cache_channels_removes_expired_unoccupied() {
        let mgr = ChannelManager::new();
        let app = mk_app(Some(0));

        let ch = mgr
            .get_or_create_cache_channel(&app, "cache-gc-test")
            .expect("cache channel created");
        ch.set_cached_payload(Arc::from("{\"hello\":1}".to_string().into_boxed_str()));
        assert!(mgr.find_channel(&app.id, "cache-gc-test").is_some());

        std::thread::sleep(std::time::Duration::from_millis(20));

        let removed = mgr.gc_empty_cache_channels();
        assert_eq!(removed, 1);
        assert!(mgr.find_channel(&app.id, "cache-gc-test").is_none());
    }

    #[test]
    fn gc_empty_cache_channels_keeps_occupied_and_live_payload() {
        let mgr = ChannelManager::new();
        let app = mk_app(None); // no TTL -> payload never expires

        // Occupied cache channel: has a subscriber, no payload set.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let handle = ConnectionHandle::from_parts(
            SocketId::from_string("sock-1".into()),
            tx,
            Arc::new(tokio::sync::Notify::new()),
        );
        mgr.subscribe(
            &app,
            &ChannelName::new("cache-occupied".to_string()),
            SocketId::from_string("sock-1".into()),
            handle,
            None,
        );

        // Empty channel with a live (non-expired) payload.
        let ch = mgr
            .get_or_create_cache_channel(&app, "cache-live-payload")
            .expect("cache channel created");
        ch.set_cached_payload(Arc::from("{\"hello\":1}".to_string().into_boxed_str()));

        let removed = mgr.gc_empty_cache_channels();
        assert_eq!(removed, 0);
        assert!(mgr.find_channel(&app.id, "cache-occupied").is_some());
        assert!(mgr.find_channel(&app.id, "cache-live-payload").is_some());
    }

    /// Regression: `register_connection` used to check-then-insert
    /// (`len() >= max` before inserting), which let concurrent registrations
    /// race past the check together and land above `max_connections`. The
    /// fix inserts optimistically and rolls back if the post-insert count
    /// overshoots. Runs many iterations with real OS threads racing to
    /// register so the invariant is exercised, not just asserted once.
    #[test]
    fn concurrent_register_never_exceeds_max_connections() {
        const MAX: u32 = 8;
        const THREADS: usize = 16;

        for iteration in 0..50 {
            let mgr = ChannelManager::new();
            let app = Arc::new(
                Application::new(
                    AppId::from("app"),
                    AppKey::from("key"),
                    "secret".into(),
                    30,
                    30,
                    10_000,
                    Some(MAX),
                    AcceptClientEventsFrom::Members,
                    None,
                    vec!["*".into()],
                )
                .unwrap(),
            );

            let handles: Vec<_> = (0..THREADS)
                .map(|i| {
                    let mgr = mgr.clone();
                    let app = app.clone();
                    std::thread::spawn(move || {
                        let (tx, _rx) = tokio::sync::mpsc::channel(8);
                        let handle = ConnectionHandle::from_parts(
                            SocketId::from_string(format!("1.{i}")),
                            tx,
                            Arc::new(tokio::sync::Notify::new()),
                        );
                        mgr.register_connection(app, handle)
                    })
                })
                .collect();

            let results: Vec<_> = handles
                .into_iter()
                .map(|h| h.join().expect("registration thread must not panic"))
                .collect();
            let ok_count = results.iter().filter(|r| r.is_ok()).count();
            let count = mgr.connection_count(&app.id);

            assert!(
                count <= MAX as usize,
                "iteration {iteration}: connection_count {count} exceeded max_connections {MAX}"
            );
            assert_eq!(
                ok_count, count,
                "iteration {iteration}: Ok registrations ({ok_count}) must equal the final connection_count ({count})"
            );
        }
    }
}
