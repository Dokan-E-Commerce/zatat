use std::time::Instant;

use dashmap::DashMap;
use parking_lot::RwLock;

use crate::message::{PresenceSnapshotMember, SNAPSHOT_TTL};

#[derive(Clone, Debug)]
pub struct ExpiredMember {
    pub app_id: String,
    pub channel: String,
    pub user_id: String,
}

struct PeerSnapshot {
    inserted_at: Instant,
    members: Vec<PresenceSnapshotMember>,
}

type AppChannelKey = (String, String);

pub struct PresenceCache {
    peers: DashMap<AppChannelKey, RwLock<std::collections::HashMap<String, PeerSnapshot>>>,
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
        inner.insert(
            node_id.to_string(),
            PeerSnapshot {
                inserted_at: Instant::now(),
                members,
            },
        );
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
            for m in &peer.members {
                if seen.insert(m.user_id.clone()) {
                    out.push(m.clone());
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
                    for m in snap.members {
                        expired.push(ExpiredMember {
                            app_id: key.0.clone(),
                            channel: key.1.clone(),
                            user_id: m.user_id,
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
}
