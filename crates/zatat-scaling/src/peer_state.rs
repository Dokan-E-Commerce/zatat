// Per-peer caches used by the EventDispatcher to aggregate cross-node state
// for non-presence subscription counts and watchlist user sessions.
//
// Both caches follow the same discipline as PresenceCache: entries have an
// `inserted_at` timestamp; entries older than SNAPSHOT_TTL are treated as
// absent by queries and reaped by `gc_expired`. Live events refresh the
// timestamp so a healthy peer never looks stale.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::message::SNAPSHOT_TTL;

// ──────────────────────────────── sub-count cache ────────────────────────────

#[derive(Clone)]
struct CountEntry {
    count: usize,
    inserted_at: Instant,
}

type AppChannelKey = (String, String);

/// Per-peer subscription counts for non-presence channels. Keyed by
/// (app_id, channel) → (node_id → count).
pub struct PeerChannelCounts {
    inner: DashMap<AppChannelKey, RwLock<HashMap<String, CountEntry>>>,
}

impl Default for PeerChannelCounts {
    fn default() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }
}

impl PeerChannelCounts {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a peer's current count for `channel`. Returns the previous
    /// count (or 0 if unseen) so the caller can detect transitions.
    pub fn set(&self, app_id: &str, channel: &str, node_id: &str, count: usize) -> usize {
        let key = (app_id.to_string(), channel.to_string());
        let entry = self.inner.entry(key).or_default();
        let mut inner = entry.write();
        inner
            .insert(
                node_id.to_string(),
                CountEntry {
                    count,
                    inserted_at: Instant::now(),
                },
            )
            .map(|e| e.count)
            .unwrap_or(0)
    }

    /// Sum of live (non-stale) counts across all peers. Caller adds its
    /// local count separately for a global total.
    pub fn sum(&self, app_id: &str, channel: &str) -> usize {
        let key = (app_id.to_string(), channel.to_string());
        let Some(entry) = self.inner.get(&key) else {
            return 0;
        };
        let inner = entry.read();
        let now = Instant::now();
        inner
            .values()
            .filter(|e| now.duration_since(e.inserted_at) <= SNAPSHOT_TTL)
            .map(|e| e.count)
            .sum()
    }

    /// Expire peers past TTL. Returns (app_id, channel, dropped_node_id,
    /// stale_count) so the caller can re-emit subscription_count frames.
    pub fn gc_expired(&self) -> Vec<(String, String, String, usize)> {
        let mut out = Vec::new();
        let now = Instant::now();
        let keys: Vec<AppChannelKey> = self.inner.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            let Some(entry) = self.inner.get(&key) else {
                continue;
            };
            let mut inner = entry.write();
            let drop_nodes: Vec<String> = inner
                .iter()
                .filter(|(_, e)| now.duration_since(e.inserted_at) > SNAPSHOT_TTL)
                .map(|(n, _)| n.clone())
                .collect();
            for node in drop_nodes {
                if let Some(e) = inner.remove(&node) {
                    out.push((key.0.clone(), key.1.clone(), node, e.count));
                }
            }
        }
        out
    }

    /// Remove a peer's count explicitly (e.g. peer reported count=0).
    /// Returns the previous count.
    pub fn remove(&self, app_id: &str, channel: &str, node_id: &str) -> usize {
        let key = (app_id.to_string(), channel.to_string());
        let Some(entry) = self.inner.get(&key) else {
            return 0;
        };
        let mut inner = entry.write();
        inner.remove(node_id).map(|e| e.count).unwrap_or(0)
    }

    #[cfg(test)]
    pub fn backdate(&self, app_id: &str, channel: &str, node_id: &str, age_secs: u64) {
        let key = (app_id.to_string(), channel.to_string());
        if let Some(entry) = self.inner.get(&key) {
            let mut inner = entry.write();
            if let Some(e) = inner.get_mut(node_id) {
                e.inserted_at = Instant::now() - std::time::Duration::from_secs(age_secs);
            }
        }
    }
}

// ───────────────────────────── user-session cache ────────────────────────────

#[derive(Clone)]
struct UserSessionEntry {
    users: HashSet<String>,
    inserted_at: Instant,
}

/// Per-peer set of signed-in users. Keyed by app_id → (node_id → set).
pub struct PeerUserSessions {
    inner: DashMap<String, RwLock<HashMap<String, UserSessionEntry>>>,
}

impl Default for PeerUserSessions {
    fn default() -> Self {
        Self {
            inner: DashMap::new(),
        }
    }
}

impl PeerUserSessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `user_id` as present on `node_id`. Returns true if the user was
    /// newly added for that peer.
    pub fn add(&self, app_id: &str, node_id: &str, user_id: String) -> bool {
        let entry = self.inner.entry(app_id.to_string()).or_default();
        let mut inner = entry.write();
        let peer = inner
            .entry(node_id.to_string())
            .or_insert_with(|| UserSessionEntry {
                users: HashSet::new(),
                inserted_at: Instant::now(),
            });
        peer.inserted_at = Instant::now();
        peer.users.insert(user_id)
    }

    /// Remove `user_id` from `node_id`'s set. Returns true if present.
    pub fn remove(&self, app_id: &str, node_id: &str, user_id: &str) -> bool {
        let Some(entry) = self.inner.get(app_id) else {
            return false;
        };
        let mut inner = entry.write();
        let Some(peer) = inner.get_mut(node_id) else {
            return false;
        };
        peer.inserted_at = Instant::now();
        peer.users.remove(user_id)
    }

    /// Replace `node_id`'s set wholesale (periodic snapshot).
    pub fn replace_snapshot(&self, app_id: &str, node_id: &str, users: Vec<String>) {
        let entry = self.inner.entry(app_id.to_string()).or_default();
        let mut inner = entry.write();
        inner.insert(
            node_id.to_string(),
            UserSessionEntry {
                users: users.into_iter().collect(),
                inserted_at: Instant::now(),
            },
        );
    }

    /// Whether any peer (other than `except_node`) reports this user.
    /// Stale peers are ignored.
    pub fn is_present_excluding(
        &self,
        app_id: &str,
        user_id: &str,
        except_node: Option<&str>,
    ) -> bool {
        let Some(entry) = self.inner.get(app_id) else {
            return false;
        };
        let inner = entry.read();
        let now = Instant::now();
        for (node, peer) in inner.iter() {
            if matches!(except_node, Some(n) if n == node) {
                continue;
            }
            if now.duration_since(peer.inserted_at) > SNAPSHOT_TTL {
                continue;
            }
            if peer.users.contains(user_id) {
                return true;
            }
        }
        false
    }

    /// Reaps stale peers and returns the user_ids each stale peer had, so
    /// the caller can emit watchlist `offline` events.
    pub fn gc_expired(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        let now = Instant::now();
        let apps: Vec<String> = self.inner.iter().map(|e| e.key().clone()).collect();
        for app in apps {
            let Some(entry) = self.inner.get(&app) else {
                continue;
            };
            let mut inner = entry.write();
            let drop_nodes: Vec<String> = inner
                .iter()
                .filter(|(_, e)| now.duration_since(e.inserted_at) > SNAPSHOT_TTL)
                .map(|(n, _)| n.clone())
                .collect();
            for node in drop_nodes {
                if let Some(e) = inner.remove(&node) {
                    for user_id in e.users {
                        out.push((app.clone(), node.clone(), user_id));
                    }
                }
            }
        }
        out
    }

    #[cfg(test)]
    pub fn backdate(&self, app_id: &str, node_id: &str, age_secs: u64) {
        if let Some(entry) = self.inner.get(app_id) {
            let mut inner = entry.write();
            if let Some(e) = inner.get_mut(node_id) {
                e.inserted_at = Instant::now() - std::time::Duration::from_secs(age_secs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_sum_across_peers() {
        let c = PeerChannelCounts::new();
        c.set("a", "ch", "n1", 3);
        c.set("a", "ch", "n2", 5);
        assert_eq!(c.sum("a", "ch"), 8);
    }

    #[test]
    fn counts_replace_previous() {
        let c = PeerChannelCounts::new();
        c.set("a", "ch", "n1", 3);
        c.set("a", "ch", "n1", 10);
        assert_eq!(c.sum("a", "ch"), 10);
    }

    #[test]
    fn counts_expire_on_ttl() {
        let c = PeerChannelCounts::new();
        c.set("a", "ch", "n1", 3);
        c.backdate("a", "ch", "n1", SNAPSHOT_TTL.as_secs() + 1);
        assert_eq!(c.sum("a", "ch"), 0);
        let expired = c.gc_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].3, 3);
    }

    #[test]
    fn sessions_track_per_peer_user_set() {
        let c = PeerUserSessions::new();
        assert!(c.add("a", "n1", "u1".into()));
        assert!(!c.add("a", "n1", "u1".into()));
        assert!(c.is_present_excluding("a", "u1", None));
        assert!(!c.is_present_excluding("a", "u1", Some("n1")));
    }

    #[test]
    fn sessions_remove_tracked() {
        let c = PeerUserSessions::new();
        c.add("a", "n1", "u1".into());
        assert!(c.remove("a", "n1", "u1"));
        assert!(!c.is_present_excluding("a", "u1", None));
    }

    #[test]
    fn sessions_snapshot_replaces() {
        let c = PeerUserSessions::new();
        c.add("a", "n1", "u1".into());
        c.replace_snapshot("a", "n1", vec!["u2".into(), "u3".into()]);
        assert!(!c.is_present_excluding("a", "u1", None));
        assert!(c.is_present_excluding("a", "u2", None));
        assert!(c.is_present_excluding("a", "u3", None));
    }

    #[test]
    fn sessions_expire_on_ttl() {
        let c = PeerUserSessions::new();
        c.add("a", "n1", "u1".into());
        c.backdate("a", "n1", SNAPSHOT_TTL.as_secs() + 1);
        assert!(!c.is_present_excluding("a", "u1", None));
        let expired = c.gc_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].2, "u1");
    }
}
