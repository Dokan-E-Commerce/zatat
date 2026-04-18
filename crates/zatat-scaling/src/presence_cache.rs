use std::collections::HashMap;
use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;
use serde_json::Value;

use crate::message::{PresenceSnapshotMember, SNAPSHOT_TTL};

#[derive(Clone, Debug)]
pub struct ExpiredMember {
    pub app_id: String,
    pub channel: String,
    pub user_id: String,
}

/// Per-peer presence roster for one (app, channel). Live `member_added` and
/// `member_removed` bus events mutate `members` in-place; a full snapshot
/// replaces `members` wholesale.
struct PeerSnapshot {
    inserted_at: Instant,
    members: HashMap<String, Option<Value>>,
}

impl PeerSnapshot {
    fn new(members: impl IntoIterator<Item = PresenceSnapshotMember>) -> Self {
        Self {
            inserted_at: Instant::now(),
            members: members
                .into_iter()
                .map(|m| (m.user_id, m.user_info))
                .collect(),
        }
    }

    fn touch(&mut self) {
        self.inserted_at = Instant::now();
    }
}

type AppChannelKey = (String, String);

pub struct PresenceCache {
    peers: DashMap<AppChannelKey, RwLock<HashMap<String, PeerSnapshot>>>,
}

impl Default for PresenceCache {
    fn default() -> Self {
        Self {
            peers: DashMap::new(),
        }
    }
}

impl PresenceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the peer's roster wholesale. Used for periodic snapshots.
    pub fn insert_snapshot(
        &self,
        app_id: &str,
        channel: &str,
        node_id: &str,
        members: Vec<PresenceSnapshotMember>,
    ) {
        let key = (app_id.to_string(), channel.to_string());
        let entry = self.peers.entry(key).or_default();
        let mut inner = entry.write();
        inner.insert(node_id.to_string(), PeerSnapshot::new(members));
    }

    /// Add one user to the peer's live roster (live MemberAdded event).
    /// Returns true if the user was newly added to this peer's set.
    pub fn add_live(
        &self,
        app_id: &str,
        channel: &str,
        node_id: &str,
        user_id: String,
        user_info: Option<Value>,
    ) -> bool {
        let key = (app_id.to_string(), channel.to_string());
        let entry = self.peers.entry(key).or_default();
        let mut inner = entry.write();
        let peer = inner
            .entry(node_id.to_string())
            .or_insert_with(|| PeerSnapshot::new(Vec::new()));
        peer.touch();
        peer.members.insert(user_id, user_info).is_none()
    }

    /// Remove one user from the peer's live roster. Returns true if the
    /// user was actually present on this peer.
    pub fn remove_live(&self, app_id: &str, channel: &str, node_id: &str, user_id: &str) -> bool {
        let key = (app_id.to_string(), channel.to_string());
        let Some(entry) = self.peers.get(&key) else {
            return false;
        };
        let mut inner = entry.write();
        let Some(peer) = inner.get_mut(node_id) else {
            return false;
        };
        peer.touch();
        peer.members.remove(user_id).is_some()
    }

    /// Whether any peer (other than `except_node`) reports this user in
    /// the channel. Stale peers (past TTL) are ignored.
    pub fn is_present_excluding(
        &self,
        app_id: &str,
        channel: &str,
        user_id: &str,
        except_node: Option<&str>,
    ) -> bool {
        let key = (app_id.to_string(), channel.to_string());
        let Some(entry) = self.peers.get(&key) else {
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
            if peer.members.contains_key(user_id) {
                return true;
            }
        }
        false
    }

    pub fn remote_members_for(&self, app_id: &str, channel: &str) -> Vec<PresenceSnapshotMember> {
        let key = (app_id.to_string(), channel.to_string());
        let Some(entry) = self.peers.get(&key) else {
            return Vec::new();
        };
        let inner = entry.read();
        let now = Instant::now();
        let mut out: Vec<PresenceSnapshotMember> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for peer in inner.values() {
            if now.duration_since(peer.inserted_at) > SNAPSHOT_TTL {
                continue;
            }
            for (user_id, user_info) in &peer.members {
                if seen.insert(user_id.clone()) {
                    out.push(PresenceSnapshotMember {
                        user_id: user_id.clone(),
                        user_info: user_info.clone(),
                    });
                }
            }
        }
        out
    }

    /// Sweeps expired peer snapshots and returns the members that vanished,
    /// so the WS layer can fire `member_removed` for them.
    pub fn gc_expired(&self) -> Vec<ExpiredMember> {
        let mut expired: Vec<ExpiredMember> = Vec::new();
        let now = Instant::now();
        let keys: Vec<AppChannelKey> = self.peers.iter().map(|e| e.key().clone()).collect();
        for key in keys {
            let Some(entry) = self.peers.get(&key) else {
                continue;
            };
            let mut inner = entry.write();
            let drop_peers: Vec<String> = inner
                .iter()
                .filter(|(_, snap)| now.duration_since(snap.inserted_at) > SNAPSHOT_TTL)
                .map(|(n, _)| n.clone())
                .collect();
            for node in drop_peers {
                if let Some(snap) = inner.remove(&node) {
                    for user_id in snap.members.into_keys() {
                        expired.push(ExpiredMember {
                            app_id: key.0.clone(),
                            channel: key.1.clone(),
                            user_id,
                        });
                    }
                }
            }
        }
        expired
    }

    #[cfg(test)]
    pub(crate) fn backdate(&self, app_id: &str, channel: &str, node_id: &str, age_secs: u64) {
        let key = (app_id.to_string(), channel.to_string());
        if let Some(entry) = self.peers.get(&key) {
            let mut inner = entry.write();
            if let Some(snap) = inner.get_mut(node_id) {
                snap.inserted_at = Instant::now() - std::time::Duration::from_secs(age_secs);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_member(id: &str) -> PresenceSnapshotMember {
        PresenceSnapshotMember {
            user_id: id.into(),
            user_info: None,
        }
    }

    #[test]
    fn first_snapshot_has_no_diff() {
        let c = PresenceCache::new();
        c.insert_snapshot("a", "presence-x", "n1", vec![mk_member("u1")]);
        let got = c.remote_members_for("a", "presence-x");
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn remote_members_merges_across_peers() {
        let c = PresenceCache::new();
        c.insert_snapshot("a", "presence-x", "n1", vec![mk_member("u1")]);
        c.insert_snapshot("a", "presence-x", "n2", vec![mk_member("u2")]);
        let got = c.remote_members_for("a", "presence-x");
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn remote_members_scoped_by_app_and_channel() {
        let c = PresenceCache::new();
        c.insert_snapshot("a", "presence-x", "n1", vec![mk_member("u1")]);
        assert!(c.remote_members_for("a", "presence-y").is_empty());
        assert!(c.remote_members_for("b", "presence-x").is_empty());
    }

    #[test]
    fn diff_detects_departed_user() {
        let c = PresenceCache::new();
        c.insert_snapshot("a", "presence-x", "n1", vec![mk_member("u1")]);
        c.backdate("a", "presence-x", "n1", SNAPSHOT_TTL.as_secs() + 1);
        let expired = c.gc_expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].user_id, "u1");
    }

    #[test]
    fn live_add_and_remove_tracked() {
        let c = PresenceCache::new();
        let added = c.add_live("a", "presence-x", "n1", "u1".into(), None);
        assert!(added);
        assert!(c.is_present_excluding("a", "presence-x", "u1", None));
        assert!(!c.is_present_excluding("a", "presence-x", "u1", Some("n1")));
        let removed = c.remove_live("a", "presence-x", "n1", "u1");
        assert!(removed);
        assert!(!c.is_present_excluding("a", "presence-x", "u1", None));
    }

    #[test]
    fn live_add_is_idempotent() {
        let c = PresenceCache::new();
        assert!(c.add_live("a", "presence-x", "n1", "u1".into(), None));
        assert!(!c.add_live("a", "presence-x", "n1", "u1".into(), None));
    }

    #[test]
    fn remove_live_returns_false_when_absent() {
        let c = PresenceCache::new();
        assert!(!c.remove_live("a", "presence-x", "n1", "u1"));
    }

    #[test]
    fn snapshot_replaces_live_entries() {
        let c = PresenceCache::new();
        c.add_live("a", "presence-x", "n1", "u1".into(), None);
        c.add_live("a", "presence-x", "n1", "u2".into(), None);
        c.insert_snapshot("a", "presence-x", "n1", vec![mk_member("u3")]);
        let members = c.remote_members_for("a", "presence-x");
        let ids: Vec<_> = members.iter().map(|m| m.user_id.as_str()).collect();
        assert_eq!(ids, vec!["u3"]);
    }
}
