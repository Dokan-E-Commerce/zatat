use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use parking_lot::RwLock;

use zatat_connection::{ConnectionHandle, Outbound};
use zatat_core::channel_name::{ChannelKind, ChannelName};
use zatat_core::id::SocketId;
use zatat_protocol::presence::{PresenceData, PresenceMember};

pub struct Member {
    pub handle: ConnectionHandle,
    pub presence: Option<PresenceMember>,
}

struct CachedPayload {
    payload: Arc<str>,
    inserted_at: Instant,
}

pub struct Channel {
    name: ChannelName,
    kind: ChannelKind,
    members: DashMap<String, Member>,
    user_refcounts: Option<DashMap<String, usize>>,
    cached_payload: Option<RwLock<Option<CachedPayload>>>,
    cache_ttl: Option<Duration>,
}

impl Channel {
    pub fn new(name: ChannelName) -> Self {
        Self::with_cache_ttl(name, None)
    }

    pub fn with_cache_ttl(name: ChannelName, cache_ttl: Option<Duration>) -> Self {
        let kind = name.kind();
        Self {
            name,
            kind,
            members: DashMap::new(),
            user_refcounts: if kind.is_presence() {
                Some(DashMap::new())
            } else {
                None
            },
            cached_payload: if kind.is_cache() {
                Some(RwLock::new(None))
            } else {
                None
            },
            cache_ttl,
        }
    }

    pub fn name(&self) -> &ChannelName {
        &self.name
    }

    pub fn kind(&self) -> ChannelKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn contains(&self, socket_id: &str) -> bool {
        self.members.contains_key(socket_id)
    }

    /// Distinct `user_id` count on presence channels; `len()` otherwise.
    pub fn user_count(&self) -> usize {
        match &self.user_refcounts {
            Some(refs) => refs.len(),
            None => self.len(),
        }
    }

    /// Whether `user_id` has at least one local connection in this presence
    /// channel. Returns `false` on non-presence channels.
    pub fn has_user_id(&self, user_id: &str) -> bool {
        self.user_refcounts
            .as_ref()
            .map(|refs| refs.contains_key(user_id))
            .unwrap_or(false)
    }

    pub fn has_cached_payload(&self) -> bool {
        self.cached_payload().is_some()
    }

    pub fn cached_payload(&self) -> Option<Arc<str>> {
        let lock = self.cached_payload.as_ref()?;
        let read = lock.read();
        let entry = read.as_ref()?;
        if let Some(ttl) = self.cache_ttl {
            if entry.inserted_at.elapsed() > ttl {
                drop(read);
                *lock.write() = None;
                return None;
            }
        }
        Some(entry.payload.clone())
    }

    pub fn set_cached_payload(&self, payload: Arc<str>) {
        if let Some(lock) = &self.cached_payload {
            *lock.write() = Some(CachedPayload {
                payload,
                inserted_at: Instant::now(),
            });
        }
    }

    pub fn cache_ttl(&self) -> Option<Duration> {
        self.cache_ttl
    }

    pub fn presence_snapshot(&self) -> Option<PresenceData> {
        if !self.kind.is_presence() {
            return None;
        }
        let members: Vec<Option<PresenceMember>> = self
            .members
            .iter()
            .map(|e| e.value().presence.clone())
            .collect();
        Some(PresenceData::from_members(members))
    }

    pub fn subscribe(
        &self,
        socket_id: SocketId,
        handle: ConnectionHandle,
        presence: Option<PresenceMember>,
    ) -> SubscribeResult {
        // Re-subscribing the same socket must be a no-op; some SDKs retry.
        if self.members.contains_key(socket_id.as_str()) {
            return SubscribeResult {
                was_new: false,
                user_added: false,
                member_count: self.len(),
            };
        }

        let mut user_added = false;
        if let (Some(refs), Some(pm)) = (&self.user_refcounts, presence.as_ref()) {
            let mut entry = refs.entry(pm.user_id.clone()).or_insert(0);
            if *entry == 0 {
                user_added = true;
            }
            *entry += 1;
        }
        self.members
            .insert(socket_id.as_str().to_string(), Member { handle, presence });
        SubscribeResult {
            was_new: true,
            user_added,
            member_count: self.len(),
        }
    }

    pub fn unsubscribe(&self, socket_id: &SocketId) -> UnsubscribeOutcome {
        match self.members.remove(socket_id.as_str()) {
            Some((_, member)) => {
                let mut user_removed = None;
                if let (Some(refs), Some(pm)) = (&self.user_refcounts, member.presence.as_ref()) {
                    let reached_zero = if let Some(mut n) = refs.get_mut(&pm.user_id) {
                        *n = n.saturating_sub(1);
                        *n == 0
                    } else {
                        false
                    };
                    if reached_zero && refs.remove_if(&pm.user_id, |_, n| *n == 0).is_some() {
                        user_removed = Some(pm.user_id.clone());
                    }
                }
                UnsubscribeOutcome {
                    was_member: true,
                    user_removed,
                    member_count: self.len(),
                    presence: member.presence,
                }
            }
            None => UnsubscribeOutcome {
                was_member: false,
                user_removed: None,
                member_count: self.len(),
                presence: None,
            },
        }
    }

    /// User-event broadcast; on cache channels the payload is also stored
    /// for replay to late subscribers.
    pub fn broadcast(&self, payload: Arc<str>, except: Option<&SocketId>) {
        self.send_to_members(&payload, except);
        if self.kind.is_cache() {
            self.set_cached_payload(payload);
        }
    }

    /// Broadcast for `pusher_internal:*` / `pusher:error` / `cache_miss` /
    /// `subscription_count`. These MUST NOT be cached — they are
    /// per-subscriber state, not a shared payload to replay.
    pub fn broadcast_protocol(&self, payload: Arc<str>, except: Option<&SocketId>) {
        self.send_to_members(&payload, except);
    }

    fn send_to_members(&self, payload: &Arc<str>, except: Option<&SocketId>) {
        for entry in self.members.iter() {
            if let Some(ex) = except {
                if entry.key() == ex.as_str() {
                    continue;
                }
            }
            // try_send kicks the connection on Full — see ConnectionHandle.
            let _ = entry
                .value()
                .handle
                .try_send(Outbound::Text(payload.clone()));
        }
    }

    pub fn stats(&self) -> ChannelStats {
        ChannelStats {
            occupied: !self.is_empty(),
            subscription_count: self.len(),
            user_count: if self.kind.is_presence() {
                Some(self.user_count())
            } else {
                None
            },
            has_cached_payload: self.has_cached_payload(),
        }
    }

    pub fn members_iter(&self) -> Vec<(String, ConnectionHandle, Option<PresenceMember>)> {
        self.members
            .iter()
            .map(|e| {
                (
                    e.key().clone(),
                    e.value().handle.clone(),
                    e.value().presence.clone(),
                )
            })
            .collect()
    }
}

pub struct SubscribeResult {
    pub was_new: bool,
    pub user_added: bool,
    pub member_count: usize,
}

pub struct UnsubscribeOutcome {
    pub was_member: bool,
    pub user_removed: Option<String>,
    pub member_count: usize,
    pub presence: Option<PresenceMember>,
}

#[derive(Debug, Clone)]
pub struct ChannelStats {
    pub occupied: bool,
    pub subscription_count: usize,
    pub user_count: Option<usize>,
    pub has_cached_payload: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_payload_expires_after_ttl() {
        let ch = Channel::with_cache_ttl(
            ChannelName::new("cache-ttl-test".to_string()),
            Some(Duration::from_millis(30)),
        );
        ch.set_cached_payload(Arc::from("{\"hello\":1}".to_string().into_boxed_str()));
        assert!(ch.cached_payload().is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            ch.cached_payload().is_none(),
            "payload should have expired after TTL",
        );
    }

    #[test]
    fn cached_payload_never_expires_when_ttl_is_none() {
        let ch = Channel::with_cache_ttl(ChannelName::new("cache-no-ttl".to_string()), None);
        ch.set_cached_payload(Arc::from("{\"hello\":1}".to_string().into_boxed_str()));
        std::thread::sleep(Duration::from_millis(30));
        assert!(ch.cached_payload().is_some());
    }

    /// Regression: `unsubscribe` used to decrement the user's refcount, then
    /// separately re-read it (`refs.get`) and `refs.remove` it, in two
    /// non-atomic steps. Two sockets for the same presence user_id
    /// unsubscribing concurrently could both observe the count at zero and
    /// both report `user_removed`, double-firing `member_removed` upstream.
    /// The fix folds the zero-check into `remove_if` so exactly one
    /// unsubscribe wins the race. Runs many iterations with real OS threads
    /// to shake out the race.
    #[test]
    fn concurrent_unsubscribe_reports_user_removed_exactly_once() {
        for iteration in 0..200 {
            let ch = Arc::new(Channel::new(ChannelName::new("presence-x".to_string())));

            let presence = PresenceMember {
                user_id: "u1".to_string(),
                user_info: None,
            };

            let socket_a = SocketId::from_string("1.1".into());
            let (tx_a, _rx_a) = tokio::sync::mpsc::channel(8);
            let handle_a = ConnectionHandle::from_parts(
                socket_a.clone(),
                tx_a,
                Arc::new(tokio::sync::Notify::new()),
            );
            ch.subscribe(socket_a.clone(), handle_a, Some(presence.clone()));

            let socket_b = SocketId::from_string("1.2".into());
            let (tx_b, _rx_b) = tokio::sync::mpsc::channel(8);
            let handle_b = ConnectionHandle::from_parts(
                socket_b.clone(),
                tx_b,
                Arc::new(tokio::sync::Notify::new()),
            );
            ch.subscribe(socket_b.clone(), handle_b, Some(presence));

            let ch_a = ch.clone();
            let ch_b = ch.clone();
            let t_a = std::thread::spawn(move || ch_a.unsubscribe(&socket_a));
            let t_b = std::thread::spawn(move || ch_b.unsubscribe(&socket_b));

            let outcome_a = t_a.join().expect("thread a must not panic");
            let outcome_b = t_b.join().expect("thread b must not panic");

            let removed_count = [&outcome_a.user_removed, &outcome_b.user_removed]
                .into_iter()
                .filter(|r| r.is_some())
                .count();
            assert_eq!(
                removed_count, 1,
                "iteration {iteration}: expected exactly one user_removed, got {removed_count}"
            );
        }
    }
}
