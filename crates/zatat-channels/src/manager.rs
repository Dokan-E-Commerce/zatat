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
        if let Some(max) = app.max_connections {
            if slot.connections.len() as u32 >= max {
                return Err(ChannelManagerError::ConnectionLimitReached);
            }
        }
        slot.connections
            .insert(handle.socket_id().as_str().to_string(), handle);
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
        if channel.is_empty() {
            slot.channels.remove(channel_name);
        }
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
    pub fn bind_user(&self, app_id: &AppId, user_id: &str, socket_id: &SocketId) {
        let slot = self.app_slot(app_id);
        let mut entry = slot.user_index.entry(user_id.to_string()).or_default();
        if !entry.iter().any(|s| s == socket_id.as_str()) {
            entry.push(socket_id.as_str().to_string());
        }
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
