use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::future::join_all;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::warn;
use uuid::Uuid;

use zatat_channels::ChannelManager;
use zatat_core::application::AppArc;
use zatat_core::channel_name::ChannelKind;
use zatat_core::id::{AppId, SocketId};
use zatat_protocol::encryption::{
    decode_master_key, derive_shared_secret, encrypt_payload, looks_encrypted,
};
use zatat_protocol::envelope::encode_envelope_raw_data;

use crate::message::{
    AppRef, ChannelCount, ChannelMetric, MetricsQuery, PresenceSnapshotMember, ScalingEnvelope,
    ScalingPayload, SCALING_VERSION, SNAPSHOT_TTL,
};
use crate::peer_state::{PeerChannelCounts, PeerUserSessions};
use crate::presence_cache::PresenceCache;
use crate::provider::PubSubProvider;

/// Encrypts `private-encrypted-*` payloads server-side when the app has a
/// master key configured and the client did not already encrypt the payload.
/// Returns `None` when the event cannot be safely encrypted — the caller
/// must drop it rather than silently downgrade an encrypted channel to
/// plaintext.
fn maybe_encrypt(
    app: &AppArc,
    kind: ChannelKind,
    channel_name: &str,
    data: &str,
) -> Option<String> {
    if !kind.is_encrypted() {
        return Some(data.to_string());
    }
    if looks_encrypted(data) {
        return Some(data.to_string());
    }
    let Some(master_b64) = app.encryption_master_key.as_deref() else {
        warn!(app = %app.id, "no encryption_master_key configured for encrypted channel; dropping event");
        return None;
    };
    let master = match decode_master_key(master_b64) {
        Ok(k) => k,
        Err(err) => {
            warn!(app = %app.id, %err, "invalid encryption_master_key; dropping event");
            return None;
        }
    };
    let secret = derive_shared_secret(channel_name, &master);
    match encrypt_payload(data.as_bytes(), &secret) {
        Ok(s) => Some(s),
        Err(err) => {
            warn!(%err, "encryption failed; dropping event");
            None
        }
    }
}

/// Extracts the node id carried by a payload variant, if any, so
/// `handle_incoming` can refresh `peers_seen` before acting on the payload.
/// `Terminate` carries no node id (it targets a socket, not a peer) and
/// returns `None`.
fn payload_node_id(payload: &ScalingPayload) -> Option<&str> {
    match payload {
        ScalingPayload::Message { origin_node_id, .. }
        | ScalingPayload::TerminateUser { origin_node_id, .. }
        | ScalingPayload::ClientEvent { origin_node_id, .. }
        | ScalingPayload::UserEvent { origin_node_id, .. }
        | ScalingPayload::MemberAdded { origin_node_id, .. }
        | ScalingPayload::MemberRemoved { origin_node_id, .. }
        | ScalingPayload::SubscriptionCount { origin_node_id, .. }
        | ScalingPayload::UserOnline { origin_node_id, .. }
        | ScalingPayload::UserOffline { origin_node_id, .. } => Some(origin_node_id.as_str()),
        ScalingPayload::PresenceSnapshot { node_id, .. }
        | ScalingPayload::MetricsResponse { node_id, .. }
        | ScalingPayload::ChannelCountSnapshot { node_id, .. }
        | ScalingPayload::UserSessionSnapshot { node_id, .. } => Some(node_id.as_str()),
        ScalingPayload::MetricsRequest {
            requester_node_id, ..
        } => Some(requester_node_id.as_str()),
        ScalingPayload::Terminate { .. } => None,
    }
}

/// Per-request fan-in for `ask_fleet_for_channels`: each responding peer's
/// `MetricsResponse` is forwarded whole, tagged with its node id, so the
/// receive loop can track distinct responders and exit early.
type MetricsInflightTx = mpsc::UnboundedSender<(String, Vec<ChannelMetric>)>;

/// Bounded queue for the outbound publisher worker. Sized so a short
/// Redis hiccup (up to a few seconds at steady-state rates) can be absorbed
/// without drops; true sustained overload is still counted + logged rather
/// than allowed to OOM the process.
const PUBLISH_QUEUE_CAPACITY: usize = 32_768;
/// Throttle the "publish queue full — dropping" log so it doesn't flood.
const PUBLISH_DROP_WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// What to do when the outbound publisher queue is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublishOverflow {
    /// Drop the envelope + count it (`zatat_scaling_publish_drops_total`).
    /// Never blocks the caller. Safe default — callers of `publish_*` are
    /// often on the hot path (WS message dispatch, handler responses).
    #[default]
    BestEffort,
    /// Block the caller until a slot is available. Zero loss, but a sustained
    /// overload will slow every publish call. Choose this when cross-node
    /// delivery is a hard business dependency.
    Block,
}

pub struct EventDispatcher {
    channels: ChannelManager,
    #[allow(dead_code)]
    provider: Arc<dyn PubSubProvider>,
    scaling_enabled: bool,
    node_id: String,
    presence_cache: Arc<PresenceCache>,
    peer_channel_counts: Arc<PeerChannelCounts>,
    peer_user_sessions: Arc<PeerUserSessions>,
    /// Last time each peer node id was observed on the scaling bus (any
    /// payload variant that carries an origin/requester/node id). Refreshed
    /// in `handle_incoming` before the per-variant dispatch. Lets
    /// `ask_fleet_for_channels` know how many peers to expect a reply from,
    /// instead of always waiting out the full timeout.
    peers_seen: Arc<DashMap<String, Instant>>,
    metrics_inflight: Arc<DashMap<String, MetricsInflightTx>>,
    /// Outbound publisher channel. `None` when scaling is disabled.
    publish_tx: Option<mpsc::Sender<Vec<u8>>>,
    /// What to do when `publish_tx` is full.
    publish_overflow: PublishOverflow,
    /// Only `Some` when `publish_overflow` is `Block`. The producer side
    /// sends here synchronously (never blocks, never spawns); a single
    /// dedicated forwarder task drains it in order into the bounded
    /// `publish_tx` queue, awaiting a slot as needed. This preserves publish
    /// order and avoids the per-message `tokio::spawn` pattern that let a
    /// stalled consumer accumulate unbounded tasks.
    block_tx: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// Count of publish attempts that were dropped because the queue was
    /// full. Exposed via `publish_drops_total()` for tests + the
    /// `zatat_scaling_publish_drops_total` metric.
    publish_drops_total: Arc<std::sync::atomic::AtomicU64>,
    /// Throttles the drop log. We can afford to allocate a mutex here
    /// because drops are already the rare / bad path.
    last_publish_drop_warn: Arc<parking_lot::Mutex<Option<std::time::Instant>>>,
    /// Last time we logged a future-version drop; throttled so a persistent
    /// mixed-version fleet doesn't flood the log.
    last_future_version_warn: Arc<parking_lot::Mutex<Option<std::time::Instant>>>,
    future_version_drops: Arc<std::sync::atomic::AtomicU64>,
}

impl EventDispatcher {
    pub fn new(
        channels: ChannelManager,
        provider: Arc<dyn PubSubProvider>,
        scaling_enabled: bool,
    ) -> Self {
        Self::with_overflow(
            channels,
            provider,
            scaling_enabled,
            PublishOverflow::default(),
        )
    }

    pub fn with_overflow(
        channels: ChannelManager,
        provider: Arc<dyn PubSubProvider>,
        scaling_enabled: bool,
        publish_overflow: PublishOverflow,
    ) -> Self {
        // Single long-lived publisher task: drains a bounded MPSC and
        // calls provider.publish serially. Avoids the per-message
        // tokio::spawn pattern that amplified bursts by creating N in-flight
        // publishes + N scheduler entries.
        let publish_tx = if scaling_enabled {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(PUBLISH_QUEUE_CAPACITY);
            let provider_clone = provider.clone();
            tokio::spawn(run_publisher(rx, provider_clone));
            Some(tx)
        } else {
            None
        };

        // In `Block` mode, a single dedicated forwarder task drains an
        // unbounded queue into the bounded `publish_tx`, one item at a time,
        // preserving order without spawning a task per message.
        let block_tx = if publish_overflow == PublishOverflow::Block {
            publish_tx.clone().map(|bounded_tx| {
                let (unbounded_tx, mut unbounded_rx) = mpsc::unbounded_channel::<Vec<u8>>();
                tokio::spawn(async move {
                    while let Some(item) = unbounded_rx.recv().await {
                        if bounded_tx.send(item).await.is_err() {
                            break;
                        }
                    }
                });
                unbounded_tx
            })
        } else {
            None
        };

        Self {
            channels,
            provider,
            scaling_enabled,
            node_id: Uuid::new_v4().to_string(),
            presence_cache: Arc::new(PresenceCache::new()),
            peer_channel_counts: Arc::new(PeerChannelCounts::new()),
            peer_user_sessions: Arc::new(PeerUserSessions::new()),
            peers_seen: Arc::new(DashMap::new()),
            metrics_inflight: Arc::new(DashMap::new()),
            publish_tx,
            publish_overflow,
            block_tx,
            publish_drops_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_publish_drop_warn: Arc::new(parking_lot::Mutex::new(None)),
            last_future_version_warn: Arc::new(parking_lot::Mutex::new(None)),
            future_version_drops: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Total bytes-envelopes that were dropped because the publisher
    /// queue was full. Zero in healthy steady state.
    pub fn publish_drops_total(&self) -> u64 {
        self.publish_drops_total
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Current depth of the publisher queue. 0 in the steady state; if this
    /// climbs toward PUBLISH_QUEUE_CAPACITY you've found the bottleneck.
    pub fn publish_queue_depth(&self) -> usize {
        match &self.publish_tx {
            Some(tx) => PUBLISH_QUEUE_CAPACITY - tx.capacity(),
            None => 0,
        }
    }

    pub fn future_version_drops(&self) -> u64 {
        self.future_version_drops
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn log_future_version(&self, their_version: u8) {
        let n = self
            .future_version_drops
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        metrics::counter!("zatat_scaling_future_version_drops_total").increment(1);
        let mut last = self.last_future_version_warn.lock();
        let now = std::time::Instant::now();
        let should_warn = match *last {
            None => true,
            Some(t) => now.duration_since(t) >= std::time::Duration::from_secs(30),
        };
        if should_warn {
            *last = Some(now);
            drop(last);
            warn!(
                our_version = SCALING_VERSION,
                their_version,
                total_drops = n,
                "scaling bus: peer sent a newer protocol version; dropping payload"
            );
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn presence_cache(&self) -> Arc<PresenceCache> {
        self.presence_cache.clone()
    }

    pub fn peer_channel_counts(&self) -> Arc<PeerChannelCounts> {
        self.peer_channel_counts.clone()
    }

    pub fn peer_user_sessions(&self) -> Arc<PeerUserSessions> {
        self.peer_user_sessions.clone()
    }

    /// Number of distinct peer nodes seen on the scaling bus within
    /// `SNAPSHOT_TTL`. Presence heartbeats land every 5s and the TTL is 15s,
    /// so a live fleet member is reflected here within one heartbeat.
    pub fn live_peer_count(&self) -> usize {
        let now = Instant::now();
        self.peers_seen
            .iter()
            .filter(|e| now.duration_since(*e.value()) <= SNAPSHOT_TTL)
            .count()
    }

    pub fn channels(&self) -> &ChannelManager {
        &self.channels
    }

    pub async fn dispatch_message(
        &self,
        app: AppArc,
        channel_name: String,
        event: String,
        data: String,
        except_socket_id: Option<SocketId>,
    ) {
        let kind = ChannelKind::from_name(&channel_name);
        let Some(data) = maybe_encrypt(&app, kind, &channel_name, &data) else {
            return;
        };

        // Pass the AppArc so cache-* channels can be created on demand to
        // retain the payload for late subscribers.
        self.broadcast_locally_with_app(
            &app.id,
            Some(&app),
            &channel_name,
            &event,
            &data,
            except_socket_id.as_ref(),
        );

        if self.scaling_enabled {
            let env = ScalingEnvelope {
                version: SCALING_VERSION,
                app: AppRef {
                    id: app.id.as_str().to_string(),
                    key: app.key.as_str().to_string(),
                },
                payload: ScalingPayload::Message {
                    origin_node_id: self.node_id.clone(),
                    channel: channel_name,
                    event,
                    data,
                    except_socket_id: except_socket_id.map(|s| s.as_str().to_string()),
                },
            };
            self.spawn_publish(env);
        }
    }

    /// Emits a `client-*` event to the scaling bus. Does NOT broadcast
    /// locally — the caller has already done that.
    pub async fn publish_client_event(
        &self,
        app: &AppArc,
        channel: String,
        event: String,
        data: String,
        socket_id: SocketId,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::ClientEvent {
                origin_node_id: self.node_id.clone(),
                channel,
                event,
                data,
                socket_id: socket_id.as_str().to_string(),
            },
        };
        self.spawn_publish(env);
    }

    pub async fn dispatch_user_event(
        &self,
        app: AppArc,
        user_id: String,
        event: String,
        data: String,
    ) {
        self.deliver_user_event_locally(&app.id, &user_id, &event, &data);
        if self.scaling_enabled {
            let env = ScalingEnvelope {
                version: SCALING_VERSION,
                app: AppRef {
                    id: app.id.as_str().to_string(),
                    key: app.key.as_str().to_string(),
                },
                payload: ScalingPayload::UserEvent {
                    origin_node_id: self.node_id.clone(),
                    user_id,
                    event,
                    data,
                },
            };
            self.spawn_publish(env);
        }
    }

    pub async fn publish_terminate(&self, app: &AppArc, socket_id: SocketId) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::Terminate {
                socket_id: socket_id.as_str().to_string(),
            },
        };
        self.spawn_publish(env);
    }

    /// Tell every peer to close every socket bound to `user_id`. The local
    /// closes are the caller's responsibility; this only propagates.
    pub async fn publish_terminate_user(&self, app: &AppArc, user_id: String) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::TerminateUser {
                origin_node_id: self.node_id.clone(),
                user_id,
            },
        };
        self.spawn_publish(env);
    }

    pub async fn publish_presence_snapshot(
        &self,
        app: &AppArc,
        channel: String,
        members: Vec<PresenceSnapshotMember>,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::PresenceSnapshot {
                node_id: self.node_id.clone(),
                channel,
                members,
            },
        };
        self.spawn_publish(env);
    }

    /// Announce a presence user_id that just joined locally. Peers dedupe
    /// against their own global view and only emit `member_added` if this
    /// is the first time the user has appeared in the channel.
    pub fn publish_member_added(
        &self,
        app: &AppArc,
        channel: String,
        user_id: String,
        user_info: Option<serde_json::Value>,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MemberAdded {
                origin_node_id: self.node_id.clone(),
                channel,
                user_id,
                user_info,
            },
        };
        self.spawn_publish(env);
    }

    /// Announce a presence user_id that just left locally. Peers only emit
    /// `member_removed` if the user no longer exists anywhere globally.
    pub fn publish_member_removed(&self, app: &AppArc, channel: String, user_id: String) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MemberRemoved {
                origin_node_id: self.node_id.clone(),
                channel,
                user_id,
            },
        };
        self.spawn_publish(env);
    }

    /// Publish this node's current local subscription count for a
    /// non-presence channel. Peers sum their own local count with the
    /// aggregated peer counts to emit the fleet-wide total.
    pub fn publish_subscription_count(&self, app: &AppArc, channel: String, count: usize) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::SubscriptionCount {
                origin_node_id: self.node_id.clone(),
                channel,
                count,
            },
        };
        self.spawn_publish(env);
    }

    /// A user's first socket on this node came up. Peers emit watchlist
    /// `online` only on the global 0→1 transition.
    pub fn publish_user_online(&self, app: &AppArc, user_id: String) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::UserOnline {
                origin_node_id: self.node_id.clone(),
                user_id,
            },
        };
        self.spawn_publish(env);
    }

    /// A user's last socket on this node went away. Peers emit watchlist
    /// `offline` only on the global 1→0 transition.
    pub fn publish_user_offline(&self, app: &AppArc, user_id: String) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::UserOffline {
                origin_node_id: self.node_id.clone(),
                user_id,
            },
        };
        self.spawn_publish(env);
    }

    /// Reconciliation snapshot: every non-presence channel's local sub count.
    pub fn publish_channel_count_snapshot(&self, app: &AppArc, counts: Vec<ChannelCount>) {
        if !self.scaling_enabled || counts.is_empty() {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::ChannelCountSnapshot {
                node_id: self.node_id.clone(),
                counts,
            },
        };
        self.spawn_publish(env);
    }

    /// Reconciliation snapshot: every user_id with at least one local socket.
    pub fn publish_user_session_snapshot(&self, app: &AppArc, user_ids: Vec<String>) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::UserSessionSnapshot {
                node_id: self.node_id.clone(),
                user_ids,
            },
        };
        self.spawn_publish(env);
    }

    /// Enqueue an envelope for the publisher worker. Behavior when the
    /// queue is full depends on `publish_overflow`:
    ///   - `BestEffort` (default): drop + count + throttled warn.
    ///   - `Block`: hand off to the unbounded `block_tx`, which the single
    ///     forwarder task drains into the bounded queue in order. Zero loss,
    ///     and — unlike a per-message `tokio::spawn` — publish order is
    ///     preserved and a stalled consumer can't accumulate unbounded tasks.
    fn spawn_publish(&self, env: ScalingEnvelope) {
        let Some(tx) = &self.publish_tx else {
            return;
        };
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        metrics::gauge!("zatat_scaling_publish_queue_depth").set(self.publish_queue_depth() as f64);
        match self.publish_overflow {
            PublishOverflow::BestEffort => {
                if let Err(e) = tx.try_send(bytes) {
                    if matches!(e, mpsc::error::TrySendError::Closed(_)) {
                        return;
                    }
                    let total = self
                        .publish_drops_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    metrics::counter!("zatat_scaling_publish_drops_total").increment(1);
                    let now = std::time::Instant::now();
                    let mut last = self.last_publish_drop_warn.lock();
                    let should_warn = match *last {
                        None => true,
                        Some(t) => now.duration_since(t) >= PUBLISH_DROP_WARN_INTERVAL,
                    };
                    if should_warn {
                        *last = Some(now);
                        drop(last);
                        warn!(
                            total_drops = total,
                            queue_capacity = PUBLISH_QUEUE_CAPACITY,
                            "scaling publisher queue FULL — dropping bus payloads; \
                             Redis is slow or the consumer side can't keep up"
                        );
                    }
                }
            }
            PublishOverflow::Block => {
                if let Some(block_tx) = &self.block_tx {
                    let _ = block_tx.send(bytes);
                }
            }
        }
    }

    pub fn handle_incoming(
        &self,
        env: ScalingEnvelope,
        apps_by_id_lookup: impl Fn(&AppId) -> Option<AppArc>,
    ) {
        // Forward-compat guard: a peer running a NEWER protocol version
        // may have added variants or changed field semantics we don't
        // understand. Drop the payload rather than risk acting on it.
        // Throttled warn so a sustained mixed-version fleet doesn't flood logs.
        if env.version > SCALING_VERSION {
            self.log_future_version(env.version);
            return;
        }
        if let Some(id) = payload_node_id(&env.payload) {
            if id != self.node_id {
                self.peers_seen.insert(id.to_string(), Instant::now());
            }
        }
        let Some(app) = apps_by_id_lookup(&AppId::from(env.app.id.as_str())) else {
            return;
        };
        match env.payload {
            ScalingPayload::Message {
                origin_node_id,
                channel,
                event,
                data,
                except_socket_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                let except = except_socket_id.map(SocketId::from_string);
                // Pass the AppArc so get_or_create_cache_channel materializes
                // the channel on this peer and stores the cached payload for
                // late subscribers — without this, late subs on peer nodes
                // would get cache_miss even though the origin node has the data.
                self.broadcast_locally_with_app(
                    &app.id,
                    Some(&app),
                    &channel,
                    &event,
                    &data,
                    except.as_ref(),
                );
            }
            ScalingPayload::ClientEvent {
                origin_node_id,
                channel,
                event,
                data,
                socket_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                let except = SocketId::from_string(socket_id);
                self.broadcast_locally(&app.id, &channel, &event, &data, Some(&except));
            }
            ScalingPayload::Terminate { socket_id } => {
                let sid = SocketId::from_string(socket_id);
                if let Some(h) = self.channels.handle_for_socket(&app.id, &sid) {
                    let _ = h.try_send(zatat_connection::Outbound::Close {
                        code: 4009,
                        reason: "terminated".into(),
                    });
                }
            }
            ScalingPayload::TerminateUser {
                origin_node_id,
                user_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                for h in self.channels.connections_for_user(&app.id, &user_id) {
                    let _ = h.try_send(zatat_connection::Outbound::Close {
                        code: 4009,
                        reason: "terminated".into(),
                    });
                }
            }
            ScalingPayload::PresenceSnapshot {
                node_id,
                channel,
                members,
            } => {
                if node_id == self.node_id {
                    return;
                }
                self.presence_cache
                    .insert_snapshot(app.id.as_str(), &channel, &node_id, members);
            }
            ScalingPayload::UserEvent {
                origin_node_id,
                user_id,
                event,
                data,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.deliver_user_event_locally(&app.id, &user_id, &event, &data);
            }
            ScalingPayload::MetricsRequest {
                request_id,
                requester_node_id,
                query,
            } => {
                if requester_node_id == self.node_id {
                    return;
                }
                self.respond_to_metrics_request(&app, request_id, query);
            }
            ScalingPayload::MetricsResponse {
                request_id,
                node_id,
                channels: metrics,
            } => {
                // Keep the sender registered: peers may each respond.
                // The originator unregisters when the wait window closes.
                if let Some(entry) = self.metrics_inflight.get(&request_id) {
                    let tx = entry.clone();
                    drop(entry);
                    let _ = tx.send((node_id, metrics));
                }
            }
            ScalingPayload::MemberAdded {
                origin_node_id,
                channel,
                user_id,
                user_info,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.apply_remote_member_added(&app, origin_node_id, channel, user_id, user_info);
            }
            ScalingPayload::MemberRemoved {
                origin_node_id,
                channel,
                user_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.apply_remote_member_removed(&app, origin_node_id, channel, user_id);
            }
            ScalingPayload::SubscriptionCount {
                origin_node_id,
                channel,
                count,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.apply_remote_subscription_count(&app, origin_node_id, channel, count);
            }
            ScalingPayload::UserOnline {
                origin_node_id,
                user_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.apply_remote_user_online(&app, origin_node_id, user_id);
            }
            ScalingPayload::UserOffline {
                origin_node_id,
                user_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                self.apply_remote_user_offline(&app, origin_node_id, user_id);
            }
            ScalingPayload::ChannelCountSnapshot { node_id, counts } => {
                if node_id == self.node_id {
                    return;
                }
                self.apply_remote_channel_count_snapshot(&app, node_id, counts);
            }
            ScalingPayload::UserSessionSnapshot { node_id, user_ids } => {
                if node_id == self.node_id {
                    return;
                }
                self.apply_remote_user_session_snapshot(&app, node_id, user_ids);
            }
        }
    }

    fn apply_remote_member_added(
        &self,
        app: &AppArc,
        origin_node_id: String,
        channel: String,
        user_id: String,
        user_info: Option<serde_json::Value>,
    ) {
        // "Globally present before this event" = locally or on another peer.
        let locally_present = self
            .channels
            .find_channel(&app.id, &channel)
            .map(|c| c.has_user_id(&user_id))
            .unwrap_or(false);
        let remote_present = self.presence_cache.is_present_excluding(
            app.id.as_str(),
            &channel,
            &user_id,
            Some(&origin_node_id),
        );
        let was_globally_present = locally_present || remote_present;

        self.presence_cache.add_live(
            app.id.as_str(),
            &channel,
            &origin_node_id,
            user_id.clone(),
            user_info.clone(),
        );

        if !was_globally_present {
            let frame =
                zatat_protocol::outbound::member_added(&channel, &user_id, user_info.as_ref());
            if let Some(ch) = self.channels.find_channel(&app.id, &channel) {
                let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                ch.broadcast_protocol(arc, None);
            }
        }
    }

    fn apply_remote_member_removed(
        &self,
        app: &AppArc,
        origin_node_id: String,
        channel: String,
        user_id: String,
    ) {
        self.presence_cache
            .remove_live(app.id.as_str(), &channel, &origin_node_id, &user_id);

        let still_locally = self
            .channels
            .find_channel(&app.id, &channel)
            .map(|c| c.has_user_id(&user_id))
            .unwrap_or(false);
        let still_remotely =
            self.presence_cache
                .is_present_excluding(app.id.as_str(), &channel, &user_id, None);

        if !still_locally && !still_remotely {
            let frame = zatat_protocol::outbound::member_removed(&channel, &user_id);
            if let Some(ch) = self.channels.find_channel(&app.id, &channel) {
                let arc: Arc<str> = Arc::from(frame.into_boxed_str());
                ch.broadcast_protocol(arc, None);
            }
        }
    }

    fn apply_remote_subscription_count(
        &self,
        app: &AppArc,
        origin_node_id: String,
        channel: String,
        count: usize,
    ) {
        self.peer_channel_counts
            .set(app.id.as_str(), &channel, &origin_node_id, count);

        // Emit subscription_count_updated with the new global total to
        // every local subscriber on this node.
        let Some(ch) = self.channels.find_channel(&app.id, &channel) else {
            return;
        };
        if ch.kind().is_presence() {
            return;
        }
        let local_count = ch.len();
        let peer_sum = self.peer_channel_counts.sum(app.id.as_str(), &channel);
        let total = local_count + peer_sum;
        let frame = zatat_protocol::outbound::subscription_count(&channel, total);
        let arc: Arc<str> = Arc::from(frame.into_boxed_str());
        ch.broadcast_protocol(arc, None);
    }

    fn apply_remote_user_online(&self, app: &AppArc, origin_node_id: String, user_id: String) {
        let was_globally_online = self.channels.is_user_online(&app.id, &user_id)
            || self.peer_user_sessions.is_present_excluding(
                app.id.as_str(),
                &user_id,
                Some(&origin_node_id),
            );
        self.peer_user_sessions
            .add(app.id.as_str(), &origin_node_id, user_id.clone());

        if !was_globally_online {
            self.emit_local_watchlist_event(&app.id, &user_id, "online");
        }
    }

    fn apply_remote_user_offline(&self, app: &AppArc, origin_node_id: String, user_id: String) {
        self.peer_user_sessions
            .remove(app.id.as_str(), &origin_node_id, &user_id);

        let still_locally = self.channels.is_user_online(&app.id, &user_id);
        let still_remotely =
            self.peer_user_sessions
                .is_present_excluding(app.id.as_str(), &user_id, None);
        if !still_locally && !still_remotely {
            self.emit_local_watchlist_event(&app.id, &user_id, "offline");
        }
    }

    fn apply_remote_channel_count_snapshot(
        &self,
        app: &AppArc,
        node_id: String,
        counts: Vec<ChannelCount>,
    ) {
        // Snapshot carries only channels the peer still subscribes to. Any
        // channel this peer previously reported but omits now is either
        // gone or count=0; drop it (otherwise stale counts would linger).
        let present: std::collections::HashSet<&str> =
            counts.iter().map(|c| c.channel.as_str()).collect();
        // Collect which channels we currently track for this peer (via sum
        // query — we need to iterate peers map, but PeerChannelCounts
        // doesn't expose that; fallback: just re-set every channel the
        // snapshot carries, and leave others alone. TTL reclaims stragglers.)
        for c in &counts {
            self.peer_channel_counts
                .set(app.id.as_str(), &c.channel, &node_id, c.count);
        }
        let _ = present; // intentionally unused; reserved for future stricter reconciliation
                         // Re-emit totals for channels we know about locally.
        for c in counts {
            let Some(ch) = self.channels.find_channel(&app.id, &c.channel) else {
                continue;
            };
            if ch.kind().is_presence() {
                continue;
            }
            let total = ch.len() + self.peer_channel_counts.sum(app.id.as_str(), &c.channel);
            let frame = zatat_protocol::outbound::subscription_count(&c.channel, total);
            let arc: Arc<str> = Arc::from(frame.into_boxed_str());
            ch.broadcast_protocol(arc, None);
        }
    }

    fn apply_remote_user_session_snapshot(
        &self,
        app: &AppArc,
        node_id: String,
        user_ids: Vec<String>,
    ) {
        // Diff the new vs. previous set so we can drive watchlist events
        // for users that disappeared from this peer (or newly appeared).
        let before: std::collections::HashSet<String> = {
            // Take existing users for this peer by querying via is_present_excluding
            // would be O(N²); instead gather them via the cache's internal
            // state after snapshot replace is cheaper. We replace
            // unconditionally; any that vanished will be detected by
            // comparing our local authoritative view.
            std::collections::HashSet::new()
        };
        let _ = before; // placeholder; see comment below
        self.peer_user_sessions
            .replace_snapshot(app.id.as_str(), &node_id, user_ids.clone());

        // Rather than diffing, we re-assert online for every user that is
        // globally online and NOT already reflected in local state (subtle:
        // `emit_local_watchlist_event` is idempotent from the peer's POV
        // because downstream watchers will receive the event and Pusher
        // semantics allow re-assertion). To avoid spam, only emit for users
        // that aren't present locally (local state would have already fired
        // its own online event) and aren't on any other peer.
        for user_id in &user_ids {
            if self.channels.is_user_online(&app.id, user_id) {
                continue;
            }
            // Already observed by any other peer? — suppress (they'd have fired earlier).
            if self.peer_user_sessions.is_present_excluding(
                app.id.as_str(),
                user_id,
                Some(&node_id),
            ) {
                continue;
            }
            // This user's only presence is on this one peer; emit online.
            self.emit_local_watchlist_event(&app.id, user_id, "online");
        }
    }

    fn emit_local_watchlist_event(&self, app_id: &AppId, user_id: &str, event_name: &str) {
        let watchers = self.channels.watchers_of(app_id, user_id);
        if watchers.is_empty() {
            return;
        }
        let frame = zatat_protocol::envelope::encode_envelope(
            "pusher_internal:watchlist_events",
            Some(&serde_json::json!({
                "events": [{
                    "name": event_name,
                    "user_ids": [user_id],
                }]
            })),
            None,
        );
        let arc: Arc<str> = Arc::from(frame.into_boxed_str());
        for watcher in watchers {
            for h in self.channels.connections_for_user(app_id, &watcher) {
                let _ = h.try_send(zatat_connection::Outbound::Text(arc.clone()));
            }
        }
    }

    fn respond_to_metrics_request(&self, app: &AppArc, request_id: String, query: MetricsQuery) {
        let prefix = query.filter_by_prefix.as_deref();
        let metrics: Vec<ChannelMetric> = self
            .channels
            .channels(&app.id)
            .into_iter()
            .filter(|ch| match prefix {
                Some(p) => ch.name().as_str().starts_with(p),
                None => true,
            })
            .map(|ch| {
                let stats = ch.stats();
                let presence_ids: Vec<String> = if ch.kind().is_presence() {
                    ch.members_iter()
                        .into_iter()
                        .filter_map(|(_, _, pm)| pm.map(|m| m.user_id))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                };
                ChannelMetric {
                    name: ch.name().as_str().to_string(),
                    occupied: stats.occupied,
                    subscription_count: stats.subscription_count,
                    user_count: stats.user_count,
                    has_cached_payload: stats.has_cached_payload,
                    presence_user_ids: presence_ids,
                }
            })
            .collect();
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MetricsResponse {
                request_id,
                node_id: self.node_id.clone(),
                channels: metrics,
            },
        };
        self.spawn_publish(env);
    }

    /// Fires a MetricsRequest on the scaling bus and collects responses
    /// until `wait` elapses. Returns `None` when scaling is disabled.
    ///
    /// Exits early once every peer known to be live (seen on the bus within
    /// `SNAPSHOT_TTL`) has answered, instead of always waiting out `wait`.
    /// At startup — before any peer has been observed — `live_peer_count()`
    /// is 0 and we conservatively fall back to the full wait, since we can't
    /// yet tell whether the fleet has zero peers or we simply haven't heard
    /// from them.
    pub async fn ask_fleet_for_channels(
        &self,
        app: &AppArc,
        query: MetricsQuery,
        wait: Duration,
    ) -> Option<Vec<ChannelMetric>> {
        if !self.scaling_enabled {
            return None;
        }
        let expected = self.live_peer_count();
        let request_id = Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Vec<ChannelMetric>)>();
        self.metrics_inflight.insert(request_id.clone(), tx);

        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MetricsRequest {
                request_id: request_id.clone(),
                requester_node_id: self.node_id.clone(),
                query,
            },
        };
        self.spawn_publish(env);

        let mut out: Vec<ChannelMetric> = Vec::new();
        let mut responders: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some((node_id, metrics))) => {
                    out.extend(metrics);
                    responders.insert(node_id);
                    if expected > 0 && responders.len() >= expected {
                        break;
                    }
                }
                _ => break,
            }
        }
        self.metrics_inflight.remove(&request_id);
        Some(out)
    }

    /// Test-only escape hatch to fetch the currently pending
    /// `ask_fleet_for_channels` request id(s) without threading it through
    /// the return value. Used to simulate a peer's `MetricsResponse` landing
    /// mid-wait.
    #[cfg(test)]
    pub fn inflight_request_ids(&self) -> Vec<String> {
        self.metrics_inflight
            .iter()
            .map(|e| e.key().clone())
            .collect()
    }

    pub fn broadcast_locally(
        &self,
        app_id: &AppId,
        channel: &str,
        event: &str,
        data: &str,
        except: Option<&SocketId>,
    ) {
        self.broadcast_locally_with_app(app_id, None, channel, event, data, except);
    }

    /// Same as `broadcast_locally`, but with the `AppArc` available so a
    /// cache channel can be created on demand (Pusher stores the last
    /// payload even when there are no subscribers at publish time).
    pub fn broadcast_locally_with_app(
        &self,
        app_id: &AppId,
        app: Option<&AppArc>,
        channel: &str,
        event: &str,
        data: &str,
        except: Option<&SocketId>,
    ) {
        if let Some(user_id) = channel.strip_prefix("#server-to-user-") {
            self.deliver_user_event_locally(app_id, user_id, event, data);
            return;
        }
        let ch = match app {
            Some(a) => self
                .channels
                .get_or_create_cache_channel(a, channel)
                .or_else(|| self.channels.find_channel(app_id, channel)),
            None => self.channels.find_channel(app_id, channel),
        };
        let Some(ch) = ch else { return };
        let frame = encode_envelope_raw_data(event, Some(data), Some(channel));
        let arc: Arc<str> = Arc::from(frame.into_boxed_str());
        ch.broadcast(arc, except);
    }

    pub fn deliver_user_event_locally(
        &self,
        app_id: &AppId,
        user_id: &str,
        event: &str,
        data: &str,
    ) {
        let channel_name = format!("#server-to-user-{user_id}");
        let frame = encode_envelope_raw_data(event, Some(data), Some(&channel_name));
        let arc: Arc<str> = Arc::from(frame.into_boxed_str());
        for handle in self.channels.connections_for_user(app_id, user_id) {
            let _ = handle.try_send(zatat_connection::Outbound::Text(arc.clone()));
        }
    }
}

/// Long-lived publisher worker: drains the outbound queue in batches of up
/// to 64 and fires `provider.publish` for the whole batch at once via
/// `join_all`. `join_all`'s first poll pass polls the futures in order, so
/// fred (which multiplexes over one connection and enqueues a command the
/// moment its future is first polled) still issues the underlying PUBLISHes
/// in enqueue order — publish order is preserved even though the round
/// trips now overlap instead of running one-at-a-time. Each batch is bounded
/// by a 5s timeout so a stalled Redis doesn't park the worker forever.
/// Latency is recorded per-batch as `zatat_scaling_publish_latency_seconds`;
/// `zatat_scaling_publish_batch_size` tracks how much pipelining is
/// actually happening.
async fn run_publisher(mut rx: mpsc::Receiver<Vec<u8>>, provider: Arc<dyn PubSubProvider>) {
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(64);
    loop {
        let n = rx.recv_many(&mut buf, 64).await;
        if n == 0 {
            break;
        }
        let batch_size = buf.len();
        let start = std::time::Instant::now();
        let publishes = buf.drain(..).map(|bytes| provider.publish(bytes));
        let _ = timeout(Duration::from_secs(5), join_all(publishes)).await;
        let elapsed = start.elapsed();
        metrics::histogram!("zatat_scaling_publish_latency_seconds").record(elapsed.as_secs_f64());
        metrics::histogram!("zatat_scaling_publish_batch_size").record(batch_size as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{AppRef, ScalingEnvelope, ScalingPayload};
    use crate::provider::LocalOnlyProvider;
    use zatat_channels::ChannelManager;
    use zatat_connection::{ConnectionHandle, Outbound};
    use zatat_core::application::{AcceptClientEventsFrom, Application};
    use zatat_core::channel_name::ChannelName;

    fn mk_app() -> AppArc {
        std::sync::Arc::new(
            Application::new(
                "app-1".into(),
                "dev-key".into(),
                "dev-secret".into(),
                60,
                30,
                10_000,
                None,
                AcceptClientEventsFrom::Members,
                None,
                Vec::new(),
            )
            .expect("app builds"),
        )
    }

    /// Regression: before the guard, a peer emitting a future SCALING_VERSION
    /// could have its payload deserialized and acted on if the variants still
    /// matched. Now any v > ours must be dropped, counted, and logged.
    #[tokio::test]
    async fn future_scaling_version_is_dropped_and_counted() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(LocalOnlyProvider);
        let dispatcher = EventDispatcher::new(channels, provider, true);
        assert_eq!(dispatcher.future_version_drops(), 0);

        // Forge an envelope with a version one higher than what we know.
        let env = ScalingEnvelope {
            version: SCALING_VERSION + 1,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::Terminate {
                socket_id: "1.2".into(),
            },
        };
        let app = mk_app();
        dispatcher.handle_incoming(env, |_id| Some(app.clone()));
        assert_eq!(
            dispatcher.future_version_drops(),
            1,
            "future version must be dropped + counted"
        );

        // Our current version must still be handled.
        let env_ok = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::Terminate {
                socket_id: "1.2".into(),
            },
        };
        dispatcher.handle_incoming(env_ok, |_id| Some(app.clone()));
        assert_eq!(
            dispatcher.future_version_drops(),
            1,
            "current version must not count as a drop"
        );
    }

    /// Regression: `maybe_encrypt` used to fall back to plaintext when the
    /// app had no `encryption_master_key`, silently downgrading a
    /// `private-encrypted-*` channel. Now the event must be dropped instead
    /// of reaching any local subscriber.
    #[tokio::test]
    async fn dispatch_drops_encrypted_event_without_master_key() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(LocalOnlyProvider);
        let dispatcher = EventDispatcher::new(channels.clone(), provider, false);
        let app = mk_app();
        assert!(app.encryption_master_key.is_none());

        let channel_name = ChannelName::new("private-encrypted-secret");
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(8);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        channels.subscribe(&app, &channel_name, socket_id, handle, None);

        dispatcher
            .dispatch_message(
                app,
                channel_name.as_str().to_string(),
                "my-event".to_string(),
                "top secret".to_string(),
                None,
            )
            .await;

        assert!(
            rx.try_recv().is_err(),
            "subscriber must not receive a private-encrypted event when no master key is configured"
        );
    }

    /// Stand-in publisher that sleeps per call — lets us fill the bounded
    /// queue deterministically in the drop test below.
    struct SlowProvider {
        delay_ms: u64,
    }
    #[async_trait::async_trait]
    impl PubSubProvider for SlowProvider {
        async fn publish(&self, _payload: Vec<u8>) {
            tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        }
        async fn subscribe(&self) -> Result<tokio::sync::broadcast::Receiver<Vec<u8>>, String> {
            let (_tx, rx) = tokio::sync::broadcast::channel(1);
            Ok(rx)
        }
    }

    /// Opt-in `PublishOverflow::Block` must NOT drop events. With a slow
    /// provider + burst > capacity, best-effort drops; block mode does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_mode_never_drops_publish() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(SlowProvider { delay_ms: 100 });
        let dispatcher =
            EventDispatcher::with_overflow(channels, provider, true, PublishOverflow::Block);
        let app = mk_app();

        let burst = PUBLISH_QUEUE_CAPACITY + 512;
        for _ in 0..burst {
            dispatcher
                .publish_terminate(&app, SocketId::from_string("1.2".into()))
                .await;
        }
        // Brief pause so the background sends land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            dispatcher.publish_drops_total(),
            0,
            "block mode must never drop publisher payloads"
        );
    }

    /// Regression: publishes used to `tokio::spawn` per message, so a slow
    /// provider could pile up unbounded tasks. Now a single bounded queue
    /// drains them — overflowing it increments `publish_drops_total` rather
    /// than growing memory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn publisher_queue_overflow_is_counted() {
        let channels = ChannelManager::new();
        // 100ms per publish — slow enough that filling capacity is trivial.
        let provider = std::sync::Arc::new(SlowProvider { delay_ms: 100 });
        let dispatcher = EventDispatcher::new(channels, provider, true);
        let app = mk_app();

        assert_eq!(dispatcher.publish_drops_total(), 0);
        assert_eq!(dispatcher.publish_queue_depth(), 0);

        // Burst well above PUBLISH_QUEUE_CAPACITY. With the old
        // per-message-spawn model this would just create N tasks; with the
        // bounded queue, anything past capacity must be counted as dropped.
        let burst = PUBLISH_QUEUE_CAPACITY + 512;
        for _ in 0..burst {
            dispatcher
                .publish_terminate(&app, SocketId::from_string("1.2".into()))
                .await;
        }

        assert!(
            dispatcher.publish_drops_total() > 0,
            "expected some drops when burst ({burst}) exceeds capacity ({PUBLISH_QUEUE_CAPACITY}); got 0"
        );
    }

    /// Regression: `apply_remote_subscription_count` used to call
    /// `ch.broadcast(...)`, which — on a cache channel — overwrites the
    /// cached payload with the protocol frame itself. A late subscriber
    /// would then replay `pusher_internal:subscription_count` as if it were
    /// the last real event payload. It must use `broadcast_protocol` so the
    /// cache is left untouched.
    #[tokio::test]
    async fn remote_subscription_count_does_not_overwrite_cache_payload() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(LocalOnlyProvider);
        let dispatcher = EventDispatcher::new(channels.clone(), provider, true);
        let app = mk_app();

        let channel_name = ChannelName::new("cache-room");
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(8);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        channels.subscribe(&app, &channel_name, socket_id, handle, None);

        let cached: Arc<str> = Arc::from(
            "{\"event\":\"e\",\"data\":\"1\"}"
                .to_string()
                .into_boxed_str(),
        );
        channels
            .find_channel(&app.id, "cache-room")
            .expect("channel exists after subscribe")
            .set_cached_payload(cached.clone());

        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::SubscriptionCount {
                origin_node_id: "other-node".into(),
                channel: "cache-room".into(),
                count: 3,
            },
        };
        dispatcher.handle_incoming(env, |_id| Some(app.clone()));

        assert!(
            rx.try_recv().is_ok(),
            "subscriber must still receive the subscription_count frame"
        );
        assert_eq!(
            channels
                .find_channel(&app.id, "cache-room")
                .expect("channel still exists")
                .cached_payload(),
            Some(cached),
            "remote subscription_count must not overwrite the cached payload"
        );
    }

    /// Same regression as above, for `apply_remote_member_added` on a
    /// presence-cache channel: the `member_added` protocol frame must not
    /// clobber the channel's cached payload.
    #[tokio::test]
    async fn remote_member_added_does_not_overwrite_presence_cache_payload() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(LocalOnlyProvider);
        let dispatcher = EventDispatcher::new(channels.clone(), provider, true);
        let app = mk_app();

        let channel_name = ChannelName::new("presence-cache-room");
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(8);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        channels.subscribe(&app, &channel_name, socket_id, handle, None);

        let cached: Arc<str> = Arc::from(
            "{\"event\":\"e\",\"data\":\"1\"}"
                .to_string()
                .into_boxed_str(),
        );
        channels
            .find_channel(&app.id, "presence-cache-room")
            .expect("channel exists after subscribe")
            .set_cached_payload(cached.clone());

        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::MemberAdded {
                origin_node_id: "other-node".into(),
                channel: "presence-cache-room".into(),
                user_id: "u9".into(),
                user_info: None,
            },
        };
        dispatcher.handle_incoming(env, |_id| Some(app.clone()));

        assert!(
            rx.try_recv().is_ok(),
            "subscriber must still receive the member_added frame"
        );
        assert_eq!(
            channels
                .find_channel(&app.id, "presence-cache-room")
                .expect("channel still exists")
                .cached_payload(),
            Some(cached),
            "remote member_added must not overwrite the cached payload"
        );
    }

    /// Stand-in publisher that records every published payload at future
    /// entry — i.e. synchronously, before the first `.await` point — and
    /// only then sleeps briefly. Since `join_all` polls a batch's futures in
    /// order on its first poll pass, and each future's body runs
    /// synchronously up to its first await, the recorded order reflects
    /// enqueue order regardless of how the sleeps subsequently resolve.
    struct RecordingProvider {
        recorded: Arc<parking_lot::Mutex<Vec<Vec<u8>>>>,
    }
    #[async_trait::async_trait]
    impl PubSubProvider for RecordingProvider {
        async fn publish(&self, payload: Vec<u8>) {
            self.recorded.lock().push(payload);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        async fn subscribe(&self) -> Result<tokio::sync::broadcast::Receiver<Vec<u8>>, String> {
            let (_tx, rx) = tokio::sync::broadcast::channel(1);
            Ok(rx)
        }
    }

    /// Regression: `Block` mode used to `tokio::spawn` a task per publish,
    /// which gave no ordering guarantee between publishes racing to acquire
    /// a slot in the bounded queue. It now routes through a single dedicated
    /// forwarder task, so publish order must be preserved end-to-end.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn block_mode_preserves_publish_order() {
        let channels = ChannelManager::new();
        let recorded = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let provider = std::sync::Arc::new(RecordingProvider {
            recorded: recorded.clone(),
        });
        let dispatcher =
            EventDispatcher::with_overflow(channels, provider, true, PublishOverflow::Block);
        let app = mk_app();

        const N: usize = 200;
        for i in 0..N {
            dispatcher
                .publish_terminate(&app, SocketId::from_string(format!("1.{i}")))
                .await;
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if recorded.lock().len() >= N {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for the recorder to observe all {N} publishes"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let recorded = recorded.lock();
        assert_eq!(recorded.len(), N);
        for (i, bytes) in recorded.iter().enumerate() {
            let env: ScalingEnvelope =
                serde_json::from_slice(bytes).expect("recorded payload must be a valid envelope");
            match env.payload {
                ScalingPayload::Terminate { socket_id } => {
                    assert_eq!(
                        socket_id,
                        format!("1.{i}"),
                        "publish order must be preserved under Block overflow"
                    );
                }
                other => panic!("unexpected payload at position {i}: {other:?}"),
            }
        }
    }

    /// Regression: `ask_fleet_for_channels` used to always wait out the
    /// full `wait` duration because it had no idea how many peers existed.
    /// Once a peer has been observed (via any bus payload carrying its node
    /// id — here a `PresenceSnapshot`), `live_peer_count()` reflects it, and
    /// the fleet query returns as soon as that many distinct nodes have
    /// responded rather than waiting for the deadline.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ask_fleet_for_channels_returns_early_once_known_peers_respond() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(LocalOnlyProvider);
        let dispatcher = Arc::new(EventDispatcher::new(channels, provider, true));
        let app = mk_app();

        let snapshot_env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::PresenceSnapshot {
                node_id: "n1".into(),
                channel: "presence-room".into(),
                members: Vec::new(),
            },
        };
        dispatcher.handle_incoming(snapshot_env, |_id| Some(app.clone()));
        assert_eq!(dispatcher.live_peer_count(), 1);

        let responder = dispatcher.clone();
        let responder_app = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let request_id = responder
                .inflight_request_ids()
                .into_iter()
                .next()
                .expect("ask_fleet_for_channels request should be in flight");
            let response_env = ScalingEnvelope {
                version: SCALING_VERSION,
                app: AppRef {
                    id: "app-1".into(),
                    key: "dev-key".into(),
                },
                payload: ScalingPayload::MetricsResponse {
                    request_id,
                    node_id: "n1".into(),
                    channels: vec![ChannelMetric {
                        name: "room".into(),
                        occupied: true,
                        subscription_count: 1,
                        user_count: None,
                        has_cached_payload: false,
                        presence_user_ids: Vec::new(),
                    }],
                },
            };
            responder.handle_incoming(response_env, |_id| Some(responder_app.clone()));
        });

        let start = std::time::Instant::now();
        let result = dispatcher
            .ask_fleet_for_channels(
                &app,
                MetricsQuery {
                    filter_by_prefix: None,
                    info: None,
                },
                std::time::Duration::from_millis(300),
            )
            .await
            .expect("scaling enabled");
        let elapsed = start.elapsed();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "room");
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "expected early return once the known peer responded, took {elapsed:?}"
        );
    }
}
