use std::collections::HashMap;
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

/// Why an event cannot be published to an encrypted channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// Plaintext for an encrypted channel, and no master key to encrypt it.
    MissingMasterKey,
    /// The configured `encryption_master_key` is not a valid 32-byte key.
    InvalidMasterKey(String),
    /// Encryption itself failed.
    EncryptionFailed(String),
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PublishError::MissingMasterKey => f.write_str(
                "plaintext data for an encrypted channel and no encryption_master_key is configured",
            ),
            PublishError::InvalidMasterKey(e) => write!(f, "invalid encryption_master_key: {e}"),
            PublishError::EncryptionFailed(e) => write!(f, "encryption failed: {e}"),
        }
    }
}

/// Encrypts `private-encrypted-*` payloads server-side when the app has a
/// master key configured and the client did not already encrypt the payload.
/// Fails when the event cannot be safely encrypted — the event must be
/// rejected rather than silently downgraded to plaintext.
fn maybe_encrypt(
    app: &AppArc,
    kind: ChannelKind,
    channel_name: &str,
    data: &str,
) -> Result<String, PublishError> {
    if !kind.is_encrypted() || looks_encrypted(data) {
        return Ok(data.to_string());
    }
    let master_b64 = app
        .encryption_master_key
        .as_deref()
        .ok_or(PublishError::MissingMasterKey)?;
    let master = decode_master_key(master_b64)
        .map_err(|err| PublishError::InvalidMasterKey(err.to_string()))?;
    let secret = derive_shared_secret(channel_name, &master);
    encrypt_payload(data.as_bytes(), &secret)
        .map_err(|err| PublishError::EncryptionFailed(err.to_string()))
}

/// Checks, without publishing, that `data` can be delivered on
/// `channel_name`. HTTP handlers call this for every event of a request
/// before dispatching any of them, so a bad request is rejected whole.
pub fn validate_publish(app: &AppArc, channel_name: &str, data: &str) -> Result<(), PublishError> {
    let kind = ChannelKind::from_name(channel_name);
    if !kind.is_encrypted() || looks_encrypted(data) {
        return Ok(());
    }
    let master_b64 = app
        .encryption_master_key
        .as_deref()
        .ok_or(PublishError::MissingMasterKey)?;
    decode_master_key(master_b64)
        .map(|_| ())
        .map_err(|err| PublishError::InvalidMasterKey(err.to_string()))
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
type MetricsInflightTx = mpsc::UnboundedSender<(String, Vec<ChannelMetric>, usize)>;

/// Bounded queue for the outbound publisher worker. Sized so a short
/// Redis hiccup (up to a few seconds at steady-state rates) can be absorbed
/// without drops; true sustained overload is still counted + logged rather
/// than allowed to OOM the process.
const PUBLISH_QUEUE_CAPACITY: usize = 32_768;
/// Throttle the "publish queue full — dropping" log so it doesn't flood.
const PUBLISH_DROP_WARN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Byte budget for `Block` mode's ordered staging buffer. Past this the
/// envelope is dropped and counted like a best-effort drop, so a Redis
/// outage degrades delivery instead of exhausting memory.
const BLOCK_STAGING_MAX_BYTES: usize = 256 * 1024 * 1024;

/// What to do when the outbound publisher queue is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PublishOverflow {
    /// Drop the envelope + count it (`zatat_scaling_publish_drops_total`).
    /// Never blocks the caller. Safe default — callers of `publish_*` are
    /// often on the hot path (WS message dispatch, handler responses).
    #[default]
    BestEffort,
    /// Absorb overload in an ordered staging buffer of up to
    /// `BLOCK_STAGING_MAX_BYTES` in front of the bounded queue; drop and
    /// count only once that is exhausted. Callers never wait. Rides out
    /// longer Redis stalls than `BestEffort` at the cost of memory; neither
    /// mode is durable across a crash.
    Block,
}

/// A fleet-wide transition a node may need to report (as a webhook) on
/// behalf of the fleet after its own local transition had to withhold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetTransition {
    MemberRemoved { channel: String, user_id: String },
    ChannelVacated { channel: String },
}

type TransitionSink = Arc<dyn Fn(&AppArc, FleetTransition) + Send + Sync>;
type WithheldKey = (String, String, Option<String>);

/// How long a withheld webhook stays claimable by a later peer message.
const WITHHELD_WINDOW: Duration = Duration::from_secs(60);

/// `Block` mode's staging buffer: an ordered queue with a byte budget.
struct BlockStaging {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    bytes: Arc<std::sync::atomic::AtomicUsize>,
    limit: usize,
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
    /// sends here synchronously (never blocks, never spawns) within a byte
    /// budget; a single dedicated forwarder task drains it in order into the
    /// bounded `publish_tx` queue, awaiting a slot as needed.
    block_tx: Option<BlockStaging>,
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
    /// Envelopes the publisher worker has taken off the queue but not yet
    /// finished publishing.
    publishing: Arc<std::sync::atomic::AtomicUsize>,
    /// Removal/vacated webhooks this node withheld because a peer still
    /// looked present/occupied, keyed by (app, channel, user).
    withheld: DashMap<WithheldKey, Instant>,
    transition_sink: parking_lot::RwLock<Option<TransitionSink>>,
    /// This node's outgoing sequence (see `ScalingEnvelope::seq`).
    seq: std::sync::atomic::AtomicU64,
    /// Highest sequence applied per (origin node, app, state key), so a
    /// snapshot captured before a live update cannot undo it.
    peer_seqs: DashMap<(String, String, String), (u64, Instant)>,
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
        let publishing = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let publish_tx = if scaling_enabled {
            let (tx, rx) = mpsc::channel::<Vec<u8>>(PUBLISH_QUEUE_CAPACITY);
            let provider_clone = provider.clone();
            tokio::spawn(run_publisher(rx, provider_clone, publishing.clone()));
            Some(tx)
        } else {
            None
        };

        // In `Block` mode, a single dedicated forwarder task drains the
        // byte-budgeted staging queue into the bounded `publish_tx`, one item
        // at a time, preserving order without spawning a task per message.
        let block_tx = if publish_overflow == PublishOverflow::Block {
            publish_tx.clone().map(|bounded_tx| {
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let staged = bytes.clone();
                tokio::spawn(async move {
                    while let Some(item) = rx.recv().await {
                        let len = item.len();
                        let sent = bounded_tx.send(item).await;
                        let now = staged.fetch_sub(len, std::sync::atomic::Ordering::Relaxed) - len;
                        metrics::gauge!("zatat_scaling_publish_staging_bytes").set(now as f64);
                        if sent.is_err() {
                            break;
                        }
                    }
                });
                BlockStaging {
                    tx,
                    bytes,
                    limit: BLOCK_STAGING_MAX_BYTES,
                }
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
            publishing,
            withheld: DashMap::new(),
            transition_sink: parking_lot::RwLock::new(None),
            seq: std::sync::atomic::AtomicU64::new(0),
            peer_seqs: DashMap::new(),
        }
    }

    /// Next outgoing sequence number (never 0). Snapshot publishers take one
    /// *before* capturing state and pass it along.
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Whether state from `node` for `key` at `seq` is newer than what was
    /// already applied; records it if so. `seq == 0` (older peers) is
    /// always accepted.
    fn accept_seq(&self, node: &str, app: &AppArc, key: &str, seq: u64) -> bool {
        if seq == 0 {
            return true;
        }
        let map_key = (
            node.to_string(),
            app.id.as_str().to_string(),
            key.to_string(),
        );
        let mut entry = self.peer_seqs.entry(map_key).or_insert((0, Instant::now()));
        if seq <= entry.0 {
            return false;
        }
        *entry = (seq, Instant::now());
        true
    }

    /// Drops sequence bookkeeping for state untouched for a while (and for
    /// peers that went away). Called from the periodic GC task.
    pub fn gc_peer_sequences(&self) {
        let horizon = SNAPSHOT_TTL * 4;
        self.peer_seqs.retain(|_, (_, at)| at.elapsed() < horizon);
    }

    /// Where fleet transitions this node reports on behalf of the fleet go
    /// (the server wires this to the webhook dispatcher).
    pub fn set_transition_sink(
        &self,
        sink: impl Fn(&AppArc, FleetTransition) + Send + Sync + 'static,
    ) {
        *self.transition_sink.write() = Some(Arc::new(sink));
    }

    fn report_transition(&self, app: &AppArc, transition: FleetTransition) {
        let sink = self.transition_sink.read().clone();
        if let Some(sink) = sink {
            sink(app, transition);
        }
    }

    /// Records that this node withheld a `member_removed` (with `user_id`)
    /// or `channel_vacated` (without) webhook because a peer still looked
    /// present. If that peer then leaves at the same moment, one of the two
    /// nodes reports the transition when the other's update arrives.
    pub fn note_withheld(&self, app: &AppArc, channel: &str, user_id: Option<&str>) {
        let now = Instant::now();
        self.withheld
            .retain(|_, at| now.duration_since(*at) < WITHHELD_WINDOW);
        self.withheld.insert(
            (
                app.id.as_str().to_string(),
                channel.to_string(),
                user_id.map(str::to_string),
            ),
            now,
        );
    }

    fn claim_withheld(&self, app: &AppArc, channel: &str, user_id: Option<&str>) -> bool {
        let key = (
            app.id.as_str().to_string(),
            channel.to_string(),
            user_id.map(str::to_string),
        );
        self.withheld
            .remove(&key)
            .is_some_and(|(_, at)| at.elapsed() < WITHHELD_WINDOW)
    }

    /// Whether this node has the lowest id among live peers. Used to pick
    /// one reporter when a crashed peer's state expires.
    pub fn is_fleet_leader(&self) -> bool {
        let now = Instant::now();
        self.peers_seen.iter().all(|e| {
            now.duration_since(*e.value()) > SNAPSHOT_TTL
                || e.key().as_str() > self.node_id.as_str()
        })
    }

    /// Cross-node envelopes not yet handed to Redis (queued, staged or in
    /// the publisher's current batch).
    pub fn pending_publishes(&self) -> usize {
        let staged = self.block_tx.as_ref().map_or(0, |s| {
            usize::from(s.bytes.load(std::sync::atomic::Ordering::Relaxed) > 0)
        });
        self.publish_queue_depth()
            + staged
            + self.publishing.load(std::sync::atomic::Ordering::Relaxed)
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
    ) -> Result<(), PublishError> {
        let kind = ChannelKind::from_name(&channel_name);
        let data = match maybe_encrypt(&app, kind, &channel_name, &data) {
            Ok(data) => data,
            Err(err) => {
                warn!(app = %app.id, channel = %channel_name, %err, "event rejected");
                return Err(err);
            }
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
                seq: 0,
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
        Ok(())
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
        user_id: Option<String>,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
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
                user_id,
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
                seq: 0,
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
            seq: 0,
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
            seq: 0,
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
        seq: u64,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq,
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
            seq: 0,
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
    pub fn publish_member_removed(
        &self,
        app: &AppArc,
        channel: String,
        user_id: String,
        webhook_withheld: bool,
        vacated_withheld: bool,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MemberRemoved {
                origin_node_id: self.node_id.clone(),
                channel,
                user_id,
                webhook_withheld,
                vacated_withheld,
            },
        };
        self.spawn_publish(env);
    }

    pub fn publish_subscription_count(
        &self,
        app: &AppArc,
        channel: String,
        count: usize,
        vacated_withheld: bool,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::SubscriptionCount {
                origin_node_id: self.node_id.clone(),
                channel,
                count,
                vacated_withheld,
            },
        };
        self.spawn_publish(env);
    }

    pub fn publish_user_online(&self, app: &AppArc, user_id: String) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
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
            seq: 0,
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
    /// Publishes this node's complete set of non-presence channel counts
    /// (possibly empty: omitted channels are empty here) as of `seq`.
    pub fn publish_channel_count_snapshot(
        &self,
        app: &AppArc,
        counts: Vec<ChannelCount>,
        seq: u64,
    ) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq,
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
    /// Publishes this node's complete set of signed-in users (possibly
    /// empty) as of `seq`.
    pub fn publish_user_session_snapshot(&self, app: &AppArc, user_ids: Vec<String>, seq: u64) {
        if !self.scaling_enabled {
            return;
        }
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq,
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
    ///   - `Block`: hand off to the byte-budgeted staging queue, which the
    ///     single forwarder task drains into the bounded queue in order.
    ///     Drops (counted) only once `BLOCK_STAGING_MAX_BYTES` is staged.
    fn spawn_publish(&self, mut env: ScalingEnvelope) {
        let Some(tx) = &self.publish_tx else {
            return;
        };
        if env.seq == 0 {
            env.seq = self.next_seq();
        }
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        metrics::gauge!("zatat_scaling_publish_queue_depth").set(self.publish_queue_depth() as f64);
        match self.publish_overflow {
            PublishOverflow::BestEffort => match tx.try_send(bytes) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => {}
                Err(mpsc::error::TrySendError::Full(_)) => self.record_publish_drop(),
            },
            PublishOverflow::Block => {
                let Some(staging) = &self.block_tx else {
                    return;
                };
                let len = bytes.len();
                let reserved = staging.bytes.fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |used| (used + len <= staging.limit).then_some(used + len),
                );
                match reserved {
                    Ok(used) => {
                        metrics::gauge!("zatat_scaling_publish_staging_bytes")
                            .set((used + len) as f64);
                        if staging.tx.send(bytes).is_err() {
                            staging
                                .bytes
                                .fetch_sub(len, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    Err(_) => self.record_publish_drop(),
                }
            }
        }
    }

    fn record_publish_drop(&self) {
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
        let seq = env.seq;
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
                user_id,
            } => {
                if origin_node_id == self.node_id {
                    return;
                }
                let Some(ch) = self.channels.find_channel(&app.id, &channel) else {
                    return;
                };
                let user_id = user_id.filter(|_| ch.kind().is_presence());
                let frame = zatat_protocol::envelope::encode_client_event(
                    &event,
                    &data,
                    &channel,
                    user_id.as_deref(),
                );
                let except = SocketId::from_string(socket_id);
                ch.broadcast_client_event(Arc::from(frame.into_boxed_str()), Some(&except));
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
                self.apply_remote_presence_snapshot(&app, node_id, channel, members, seq);
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
                connections,
            } => {
                // Keep the sender registered: peers may each respond.
                // The originator unregisters when the wait window closes.
                if let Some(entry) = self.metrics_inflight.get(&request_id) {
                    let tx = entry.clone();
                    drop(entry);
                    let _ = tx.send((node_id, metrics, connections));
                }
            }
            ScalingPayload::MemberAdded {
                origin_node_id,
                channel,
                user_id,
                user_info,
            } => {
                if origin_node_id == self.node_id
                    || !self.accept_seq(&origin_node_id, &app, &format!("p:{channel}"), seq)
                {
                    return;
                }
                self.apply_remote_member_added(&app, origin_node_id, channel, user_id, user_info);
            }
            ScalingPayload::MemberRemoved {
                origin_node_id,
                channel,
                user_id,
                webhook_withheld,
                vacated_withheld,
            } => {
                if origin_node_id == self.node_id
                    || !self.accept_seq(&origin_node_id, &app, &format!("p:{channel}"), seq)
                {
                    return;
                }
                self.apply_remote_member_removed(
                    &app,
                    origin_node_id,
                    channel,
                    user_id,
                    webhook_withheld,
                    vacated_withheld,
                );
            }
            ScalingPayload::SubscriptionCount {
                origin_node_id,
                channel,
                count,
                vacated_withheld,
            } => {
                if origin_node_id == self.node_id
                    || !self.accept_seq(&origin_node_id, &app, &format!("c:{channel}"), seq)
                {
                    return;
                }
                self.apply_remote_subscription_count(
                    &app,
                    origin_node_id,
                    channel,
                    count,
                    vacated_withheld,
                );
            }
            ScalingPayload::UserOnline {
                origin_node_id,
                user_id,
            } => {
                if origin_node_id == self.node_id
                    || !self.accept_seq(&origin_node_id, &app, &format!("u:{user_id}"), seq)
                {
                    return;
                }
                self.apply_remote_user_online(&app, origin_node_id, user_id);
            }
            ScalingPayload::UserOffline {
                origin_node_id,
                user_id,
            } => {
                if origin_node_id == self.node_id
                    || !self.accept_seq(&origin_node_id, &app, &format!("u:{user_id}"), seq)
                {
                    return;
                }
                self.apply_remote_user_offline(&app, origin_node_id, user_id);
            }
            ScalingPayload::ChannelCountSnapshot { node_id, counts } => {
                if node_id == self.node_id {
                    return;
                }
                self.apply_remote_channel_count_snapshot(&app, node_id, counts, seq);
            }
            ScalingPayload::UserSessionSnapshot { node_id, user_ids } => {
                if node_id == self.node_id {
                    return;
                }
                self.apply_remote_user_session_snapshot(&app, node_id, user_ids, seq);
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
        let local = self.channels.find_channel(&app.id, &channel);
        let apply = || {
            // "Globally present before this event" = locally or on another peer.
            let locally_present = local
                .as_ref()
                .map(|c| c.has_user_id(&user_id))
                .unwrap_or(false);
            let remote_present = self.presence_cache.is_present_excluding(
                app.id.as_str(),
                &channel,
                &user_id,
                Some(&origin_node_id),
            );
            self.presence_cache.add_live(
                app.id.as_str(),
                &channel,
                &origin_node_id,
                user_id.clone(),
                user_info.clone(),
            );
            if !(locally_present || remote_present) {
                if let Some(ch) = &local {
                    let frame = zatat_protocol::outbound::member_added(
                        &channel,
                        &user_id,
                        user_info.as_ref(),
                    );
                    ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
                }
            }
        };
        // Under the local channel's transition lock, so this decision cannot
        // interleave with a local join/leave of the same user.
        match &local {
            Some(ch) => ch.with_transition(apply),
            None => apply(),
        }
    }

    fn apply_remote_member_removed(
        &self,
        app: &AppArc,
        origin_node_id: String,
        channel: String,
        user_id: String,
        webhook_withheld: bool,
        vacated_withheld: bool,
    ) {
        let local = self.channels.find_channel(&app.id, &channel);
        let apply = || {
            self.presence_cache
                .remove_live(app.id.as_str(), &channel, &origin_node_id, &user_id);
            let still_locally = local
                .as_ref()
                .map(|c| c.has_user_id(&user_id))
                .unwrap_or(false);
            let still_remotely =
                self.presence_cache
                    .is_present_excluding(app.id.as_str(), &channel, &user_id, None);
            let gone = !still_locally && !still_remotely;
            if gone {
                if let Some(ch) = &local {
                    let frame = zatat_protocol::outbound::member_removed(&channel, &user_id);
                    ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
                }
            }
            let empty = local.as_ref().is_none_or(|c| c.is_empty())
                && self
                    .presence_cache
                    .remote_members_for(app.id.as_str(), &channel)
                    .is_empty();
            (gone, empty)
        };
        let (gone, empty) = match &local {
            Some(ch) => ch.with_transition(apply),
            None => apply(),
        };
        // Both nodes withheld their webhook because each saw the other: the
        // node with the lower id reports the fleet-wide transition.
        let tie_break = self.node_id < origin_node_id;
        if gone
            && webhook_withheld
            && tie_break
            && self.claim_withheld(app, &channel, Some(&user_id))
        {
            self.report_transition(
                app,
                FleetTransition::MemberRemoved {
                    channel: channel.clone(),
                    user_id,
                },
            );
        }
        if empty && vacated_withheld && tie_break && self.claim_withheld(app, &channel, None) {
            self.report_transition(app, FleetTransition::ChannelVacated { channel });
        }
    }

    fn apply_remote_subscription_count(
        &self,
        app: &AppArc,
        origin_node_id: String,
        channel: String,
        count: usize,
        vacated_withheld: bool,
    ) {
        self.peer_channel_counts
            .set(app.id.as_str(), &channel, &origin_node_id, count);
        if count == 0 && vacated_withheld && self.node_id < origin_node_id {
            let locally_empty = self
                .channels
                .find_channel(&app.id, &channel)
                .is_none_or(|c| c.is_empty());
            let fleet_empty = self.peer_channel_counts.sum(app.id.as_str(), &channel) == 0;
            if locally_empty && fleet_empty && self.claim_withheld(app, &channel, None) {
                self.report_transition(
                    app,
                    FleetTransition::ChannelVacated {
                        channel: channel.clone(),
                    },
                );
            }
        }

        // Emit subscription_count_updated with the new global total to
        // every local subscriber on this node.
        let Some(ch) = self.channels.find_channel(&app.id, &channel) else {
            return;
        };
        // Counts are also shared for fleet-wide occupied/vacated webhooks;
        // only apps that opted in get subscription_count frames.
        if ch.kind().is_presence() || !app.emit_subscription_count {
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
        self.channels.with_user_transition(&app.id, &user_id, || {
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
        });
    }

    fn apply_remote_user_offline(&self, app: &AppArc, origin_node_id: String, user_id: String) {
        self.channels.with_user_transition(&app.id, &user_id, || {
            self.peer_user_sessions
                .remove(app.id.as_str(), &origin_node_id, &user_id);
            let still_locally = self.channels.is_user_online(&app.id, &user_id);
            let still_remotely =
                self.peer_user_sessions
                    .is_present_excluding(app.id.as_str(), &user_id, None);
            if !still_locally && !still_remotely {
                self.emit_local_watchlist_event(&app.id, &user_id, "offline");
            }
        });
    }

    /// A peer's full roster for one presence channel. Missed live updates
    /// are corrected here: local subscribers get `member_added` /
    /// `member_removed` for every user whose fleet-wide presence the
    /// snapshot changes, under the channel's transition lock.
    fn apply_remote_presence_snapshot(
        &self,
        app: &AppArc,
        node_id: String,
        channel: String,
        members: Vec<PresenceSnapshotMember>,
        seq: u64,
    ) {
        if !self.accept_seq(&node_id, app, &format!("p:{channel}"), seq) {
            // Captured before a live update we already applied.
            self.presence_cache
                .touch(app.id.as_str(), &channel, &node_id);
            return;
        }
        let local = self.channels.find_channel(&app.id, &channel);
        let apply = || {
            // Diff what clients were told: every stored roster, including
            // peers past their TTL that GC has not reaped (GC announces those
            // itself). Comparing TTL-filtered rosters would silently forget an
            // expired entry's members instead of announcing their removal.
            let before = self
                .presence_cache
                .announced_user_ids(app.id.as_str(), &channel);
            let info_by_user: HashMap<String, Option<serde_json::Value>> = members
                .iter()
                .map(|m| (m.user_id.clone(), m.user_info.clone()))
                .collect();
            self.presence_cache
                .insert_snapshot(app.id.as_str(), &channel, &node_id, members);
            let Some(ch) = &local else { return };
            let after = self
                .presence_cache
                .announced_user_ids(app.id.as_str(), &channel);
            for user_id in before.difference(&after) {
                if !ch.has_user_id(user_id) {
                    let frame = zatat_protocol::outbound::member_removed(&channel, user_id);
                    ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
                }
            }
            for user_id in after.difference(&before) {
                if !ch.has_user_id(user_id) {
                    let info = info_by_user.get(user_id).cloned().flatten();
                    let frame =
                        zatat_protocol::outbound::member_added(&channel, user_id, info.as_ref());
                    ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
                }
            }
        };
        match &local {
            Some(ch) => ch.with_transition(apply),
            None => apply(),
        }
    }

    fn apply_remote_channel_count_snapshot(
        &self,
        app: &AppArc,
        node_id: String,
        counts: Vec<ChannelCount>,
        seq: u64,
    ) {
        // The snapshot is the peer's complete set as of `seq`: channels it
        // omits are empty there. Per channel, it only applies if no newer
        // live count from that peer was applied already.
        let present: std::collections::HashSet<&str> =
            counts.iter().map(|c| c.channel.as_str()).collect();
        for c in &counts {
            if self.accept_seq(&node_id, app, &format!("c:{}", c.channel), seq) {
                self.peer_channel_counts
                    .set(app.id.as_str(), &c.channel, &node_id, c.count);
            } else {
                self.peer_channel_counts
                    .touch(app.id.as_str(), &c.channel, &node_id);
            }
        }
        let dropped = self.peer_channel_counts.retain_node_channels(
            app.id.as_str(),
            &node_id,
            &present,
            |channel| self.accept_seq(&node_id, app, &format!("c:{channel}"), seq),
        );
        if !app.emit_subscription_count {
            return;
        }
        // Re-emit fleet totals to local subscribers: for the channels the
        // snapshot lists, and for those it dropped by omitting them.
        let affected = counts.into_iter().map(|c| c.channel).chain(dropped);
        for channel in affected {
            let Some(ch) = self.channels.find_channel(&app.id, &channel) else {
                continue;
            };
            if ch.kind().is_presence() {
                continue;
            }
            ch.with_transition(|| {
                let total = ch.len() + self.peer_channel_counts.sum(app.id.as_str(), &channel);
                let frame = zatat_protocol::outbound::subscription_count(&channel, total);
                ch.broadcast_protocol(Arc::from(frame.into_boxed_str()), None);
            });
        }
    }

    /// A peer's complete set of signed-in users. Users whose fleet-wide
    /// online state it changes (e.g. after a missed live update) produce
    /// watchlist events for local watchers.
    fn apply_remote_user_session_snapshot(
        &self,
        app: &AppArc,
        node_id: String,
        user_ids: Vec<String>,
        seq: u64,
    ) {
        self.peer_user_sessions.touch(app.id.as_str(), &node_id);
        let reported: std::collections::HashSet<String> = user_ids.into_iter().collect();
        let previous = self.peer_user_sessions.users_of(app.id.as_str(), &node_id);
        for user_id in reported.difference(&previous) {
            if !self.accept_seq(&node_id, app, &format!("u:{user_id}"), seq) {
                continue;
            }
            self.apply_remote_user_online(app, node_id.clone(), user_id.clone());
        }
        for user_id in previous.difference(&reported) {
            if !self.accept_seq(&node_id, app, &format!("u:{user_id}"), seq) {
                continue;
            }
            self.apply_remote_user_offline(app, node_id.clone(), user_id.clone());
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
        let channels = if query.connections_only {
            Vec::new()
        } else {
            self.channels.channels(&app.id)
        };
        let metrics: Vec<ChannelMetric> = channels
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
            seq: 0,
            app: AppRef {
                id: app.id.as_str().to_string(),
                key: app.key.as_str().to_string(),
            },
            payload: ScalingPayload::MetricsResponse {
                request_id,
                node_id: self.node_id.clone(),
                channels: metrics,
                connections: self.channels.connection_count(&app.id),
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
        self.ask_fleet(app, query, wait)
            .await
            .map(|(channels, _)| channels)
    }

    /// Sum of the app's live connections on every peer (not this node).
    /// `None` when scaling is disabled.
    pub async fn ask_fleet_for_connections(&self, app: &AppArc, wait: Duration) -> Option<usize> {
        let query = MetricsQuery {
            filter_by_prefix: None,
            info: None,
            connections_only: true,
        };
        self.ask_fleet(app, query, wait)
            .await
            .map(|(_, connections)| connections)
    }

    async fn ask_fleet(
        &self,
        app: &AppArc,
        query: MetricsQuery,
        wait: Duration,
    ) -> Option<(Vec<ChannelMetric>, usize)> {
        if !self.scaling_enabled {
            return None;
        }
        let expected = self.live_peer_count();
        let request_id = Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Vec<ChannelMetric>, usize)>();
        self.metrics_inflight.insert(request_id.clone(), tx);

        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
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
        let mut connections = 0usize;
        let mut responders: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some((node_id, metrics, peer_connections))) => {
                    out.extend(metrics);
                    connections += peer_connections;
                    responders.insert(node_id);
                    if expected > 0 && responders.len() >= expected {
                        break;
                    }
                }
                _ => break,
            }
        }
        self.metrics_inflight.remove(&request_id);
        Some((out, connections))
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
async fn run_publisher(
    mut rx: mpsc::Receiver<Vec<u8>>,
    provider: Arc<dyn PubSubProvider>,
    publishing: Arc<std::sync::atomic::AtomicUsize>,
) {
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(64);
    loop {
        let n = rx.recv_many(&mut buf, 64).await;
        if n == 0 {
            break;
        }
        let batch_size = buf.len();
        publishing.store(batch_size, std::sync::atomic::Ordering::Relaxed);
        let start = std::time::Instant::now();
        let publishes = buf.drain(..).map(|bytes| provider.publish(bytes));
        if timeout(Duration::from_secs(5), join_all(publishes))
            .await
            .is_err()
        {
            // The batch's fate is unknown: some publishes may still land
            // after the futures are dropped, the rest are lost.
            metrics::counter!("zatat_scaling_publish_timeouts_total").increment(batch_size as u64);
            warn!(batch_size, "scaling publish batch timed out after 5s");
        }
        publishing.store(0, std::sync::atomic::Ordering::Relaxed);
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

    fn mk_app_counting_subscriptions() -> AppArc {
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
            .expect("app builds")
            .with_subscription_count(true),
        )
    }

    fn env_from(seq: u64, payload: ScalingPayload) -> ScalingEnvelope {
        ScalingEnvelope {
            version: SCALING_VERSION,
            seq,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload,
        }
    }

    fn presence_subscriber(
        channels: &ChannelManager,
        app: &AppArc,
        channel: &str,
    ) -> mpsc::Receiver<Outbound> {
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, rx) = mpsc::channel::<Outbound>(64);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        let member = zatat_protocol::presence::PresenceMember {
            user_id: "local".into(),
            user_info: None,
        };
        channels.subscribe(
            app,
            &ChannelName::new(channel),
            socket_id,
            handle,
            Some(member),
        );
        rx
    }

    fn drain_events(rx: &mut mpsc::Receiver<Outbound>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Ok(Outbound::Text(t)) = rx.try_recv() {
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            let data: serde_json::Value =
                serde_json::from_str(v["data"].as_str().unwrap_or("null")).unwrap();
            out.push((
                v["event"].as_str().unwrap().to_string(),
                data["user_id"].as_str().unwrap_or("").to_string(),
            ));
        }
        out
    }

    /// Regression: a snapshot replaced a peer's roster silently, so after a
    /// missed live update local clients kept a wrong member list forever.
    /// The snapshot's differences must reach local subscribers.
    #[tokio::test]
    async fn presence_snapshot_corrects_local_rosters() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app();
        let mut rx = presence_subscriber(&channels, &app, "presence-room");
        let member = |id: &str| PresenceSnapshotMember {
            user_id: id.into(),
            user_info: None,
        };
        dispatcher.handle_incoming(
            env_from(
                1,
                ScalingPayload::PresenceSnapshot {
                    node_id: "n1".into(),
                    channel: "presence-room".into(),
                    members: vec![member("alice")],
                },
            ),
            |_| Some(app.clone()),
        );
        assert_eq!(
            drain_events(&mut rx),
            vec![("pusher_internal:member_added".into(), "alice".into())]
        );
        // Alice left and Charlie joined on n1, but both live events were lost.
        dispatcher.handle_incoming(
            env_from(
                7,
                ScalingPayload::PresenceSnapshot {
                    node_id: "n1".into(),
                    channel: "presence-room".into(),
                    members: vec![member("charlie")],
                },
            ),
            |_| Some(app.clone()),
        );
        assert_eq!(
            drain_events(&mut rx),
            vec![
                ("pusher_internal:member_removed".into(), "alice".into()),
                ("pusher_internal:member_added".into(), "charlie".into()),
            ]
        );
    }

    /// Regression: a snapshot arriving after the sender's entry passed its
    /// TTL (but before GC reaped it) was diffed against a TTL-filtered
    /// roster, so the expired entry's members were never announced as
    /// removed and unchanged members were announced again.
    #[tokio::test]
    async fn snapshot_after_peer_ttl_still_announces_removals_once() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app();
        let mut rx = presence_subscriber(&channels, &app, "presence-room");
        let member = |id: &str| PresenceSnapshotMember {
            user_id: id.into(),
            user_info: None,
        };
        let snapshot = |seq, members| {
            dispatcher.handle_incoming(
                env_from(
                    seq,
                    ScalingPayload::PresenceSnapshot {
                        node_id: "n1".into(),
                        channel: "presence-room".into(),
                        members,
                    },
                ),
                |_| Some(app.clone()),
            )
        };
        snapshot(1, vec![member("alice"), member("bob")]);
        let mut first = drain_events(&mut rx);
        first.sort();
        assert_eq!(
            first,
            vec![
                ("pusher_internal:member_added".into(), "alice".into()),
                ("pusher_internal:member_added".into(), "bob".into()),
            ]
        );
        dispatcher.presence_cache().backdate(
            "app-1",
            "presence-room",
            "n1",
            SNAPSHOT_TTL.as_secs() + 1,
        );
        snapshot(2, vec![member("bob"), member("charlie")]);
        assert_eq!(
            drain_events(&mut rx),
            vec![
                ("pusher_internal:member_removed".into(), "alice".into()),
                ("pusher_internal:member_added".into(), "charlie".into()),
            ]
        );
        // GC finds nothing left to expire: no duplicate removal later.
        assert!(dispatcher.presence_cache().gc_expired().is_empty());
    }

    /// Regression: a snapshot that omitted a channel fixed the server's
    /// count but sent subscribers no `subscription_count` correction.
    #[tokio::test]
    async fn omitted_channel_in_count_snapshot_corrects_subscribers() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app_counting_subscriptions();
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(16);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        channels.subscribe(&app, &ChannelName::new("room"), socket_id, handle, None);
        let snapshot = |seq, counts| {
            dispatcher.handle_incoming(
                env_from(
                    seq,
                    ScalingPayload::ChannelCountSnapshot {
                        node_id: "n1".into(),
                        counts,
                    },
                ),
                |_| Some(app.clone()),
            )
        };
        let count_of = |rx: &mut mpsc::Receiver<Outbound>| {
            let Ok(Outbound::Text(t)) = rx.try_recv() else {
                panic!("expected a subscription_count frame");
            };
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            let data: serde_json::Value =
                serde_json::from_str(v["data"].as_str().unwrap()).unwrap();
            data["subscription_count"].as_u64().unwrap()
        };
        snapshot(
            1,
            vec![ChannelCount {
                channel: "room".into(),
                count: 2,
            }],
        );
        assert_eq!(count_of(&mut rx), 3);
        snapshot(2, Vec::new());
        assert_eq!(count_of(&mut rx), 1);
    }

    /// Regression: a snapshot captured before a live leave but published
    /// after it resurrected the user on peers. Its sequence predates the
    /// leave, so it must be ignored — for presence and for channel counts.
    #[tokio::test]
    async fn stale_snapshots_do_not_undo_newer_live_updates() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app();
        let mut rx = presence_subscriber(&channels, &app, "presence-room");
        let incoming = |seq, payload| {
            dispatcher.handle_incoming(env_from(seq, payload), |_| Some(app.clone()))
        };
        incoming(
            1,
            ScalingPayload::MemberAdded {
                origin_node_id: "n1".into(),
                channel: "presence-room".into(),
                user_id: "alice".into(),
                user_info: None,
            },
        );
        incoming(
            5,
            ScalingPayload::MemberRemoved {
                origin_node_id: "n1".into(),
                channel: "presence-room".into(),
                user_id: "alice".into(),
                webhook_withheld: false,
                vacated_withheld: false,
            },
        );
        drain_events(&mut rx);
        incoming(
            3,
            ScalingPayload::PresenceSnapshot {
                node_id: "n1".into(),
                channel: "presence-room".into(),
                members: vec![PresenceSnapshotMember {
                    user_id: "alice".into(),
                    user_info: None,
                }],
            },
        );
        assert!(
            drain_events(&mut rx).is_empty(),
            "stale snapshot must not re-add alice"
        );
        assert!(dispatcher
            .presence_cache()
            .remote_members_for("app-1", "presence-room")
            .is_empty());

        incoming(
            9,
            ScalingPayload::SubscriptionCount {
                origin_node_id: "n1".into(),
                channel: "room".into(),
                count: 0,
                vacated_withheld: false,
            },
        );
        incoming(
            8,
            ScalingPayload::ChannelCountSnapshot {
                node_id: "n1".into(),
                counts: vec![ChannelCount {
                    channel: "room".into(),
                    count: 2,
                }],
            },
        );
        assert_eq!(dispatcher.peer_channel_counts().sum("app-1", "room"), 0);
        // A newer snapshot applies, including dropping channels it omits.
        incoming(
            10,
            ScalingPayload::ChannelCountSnapshot {
                node_id: "n1".into(),
                counts: vec![ChannelCount {
                    channel: "room".into(),
                    count: 4,
                }],
            },
        );
        assert_eq!(dispatcher.peer_channel_counts().sum("app-1", "room"), 4);
        incoming(
            11,
            ScalingPayload::ChannelCountSnapshot {
                node_id: "n1".into(),
                counts: Vec::new(),
            },
        );
        assert_eq!(dispatcher.peer_channel_counts().sum("app-1", "room"), 0);
    }

    /// Two nodes that leave at the same moment each withhold their
    /// member_removed / channel_vacated webhook because each still sees the
    /// other. When the peer's update arrives, exactly one node (the lower
    /// id) reports the transitions, and only once.
    #[tokio::test]
    async fn withheld_removal_is_reported_once_by_the_lower_node_id() {
        let dispatcher = EventDispatcher::new(
            ChannelManager::new(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let reported = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = reported.clone();
        dispatcher.set_transition_sink(move |_, t| sink.lock().push(t));
        let app = mk_app();
        let removed = |origin: &str| ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::MemberRemoved {
                origin_node_id: origin.into(),
                channel: "presence-room".into(),
                user_id: "u".into(),
                webhook_withheld: true,
                vacated_withheld: true,
            },
        };

        // Peer with a lower id: it reports, not us.
        dispatcher.note_withheld(&app, "presence-room", Some("u"));
        dispatcher.note_withheld(&app, "presence-room", None);
        dispatcher.handle_incoming(removed(""), |_| Some(app.clone()));
        assert!(reported.lock().is_empty());

        // Peer with a higher id ('~' sorts after any uuid): we report both.
        dispatcher.handle_incoming(removed("~"), |_| Some(app.clone()));
        assert_eq!(
            *reported.lock(),
            vec![
                FleetTransition::MemberRemoved {
                    channel: "presence-room".into(),
                    user_id: "u".into()
                },
                FleetTransition::ChannelVacated {
                    channel: "presence-room".into()
                },
            ]
        );
        // The withheld record is consumed: a duplicate message reports nothing.
        dispatcher.handle_incoming(removed("~~"), |_| Some(app.clone()));
        assert_eq!(reported.lock().len(), 2);
        // Without a local withheld record there is nothing to report.
        let fresh = EventDispatcher::new(
            ChannelManager::new(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let reported_fresh = Arc::new(parking_lot::Mutex::new(0));
        let sink = reported_fresh.clone();
        fresh.set_transition_sink(move |_, _| *sink.lock() += 1);
        fresh.handle_incoming(removed("~"), |_| Some(app.clone()));
        assert_eq!(*reported_fresh.lock(), 0);
    }

    /// A client event relayed from a peer keeps the sender's presence
    /// user_id and never replaces the cache channel's stored event.
    #[tokio::test]
    async fn remote_client_event_carries_user_id_and_skips_cache() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app();
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(8);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        let member = zatat_protocol::presence::PresenceMember {
            user_id: "bob".into(),
            user_info: None,
        };
        channels.subscribe(
            &app,
            &ChannelName::new("presence-cache-room"),
            socket_id,
            handle,
            Some(member),
        );
        let ch = channels
            .find_channel(&app.id, "presence-cache-room")
            .unwrap();
        ch.set_cached_payload(Arc::from(r#"{"event":"server"}"#));
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::ClientEvent {
                origin_node_id: "other-node".into(),
                channel: "presence-cache-room".into(),
                event: "client-typing".into(),
                data: "{}".into(),
                socket_id: "9.9".into(),
                user_id: Some("alice".into()),
            },
        };
        dispatcher.handle_incoming(env, |_id| Some(app.clone()));
        let Ok(Outbound::Text(frame)) = rx.try_recv() else {
            panic!("expected relayed client event");
        };
        let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(frame["event"], "client-typing");
        assert_eq!(frame["user_id"], "alice");
        assert_eq!(
            ch.cached_payload().as_deref(),
            Some(r#"{"event":"server"}"#)
        );
    }

    /// Peers share counts for every app (fleet-wide occupied/vacated
    /// webhooks need them), but only apps that enabled subscription counting
    /// may receive `pusher_internal:subscription_count` frames.
    #[tokio::test]
    async fn remote_subscription_count_is_silent_unless_app_opted_in() {
        let channels = ChannelManager::new();
        let dispatcher = EventDispatcher::new(
            channels.clone(),
            std::sync::Arc::new(LocalOnlyProvider),
            true,
        );
        let app = mk_app();
        let socket_id = SocketId::from_string("1.1".into());
        let (tx, mut rx) = mpsc::channel::<Outbound>(8);
        let handle = ConnectionHandle::from_parts(
            socket_id.clone(),
            tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );
        channels.subscribe(&app, &ChannelName::new("room"), socket_id, handle, None);
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            seq: 0,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::SubscriptionCount {
                origin_node_id: "other-node".into(),
                channel: "room".into(),
                count: 3,
                vacated_withheld: false,
            },
        };
        dispatcher.handle_incoming(env, |_id| Some(app.clone()));
        assert!(
            rx.try_recv().is_err(),
            "app did not opt into subscription counts"
        );
        assert_eq!(dispatcher.peer_channel_counts().sum("app-1", "room"), 3);
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
            seq: 0,
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
            seq: 0,
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

        assert_eq!(
            validate_publish(&app, channel_name.as_str(), "top secret"),
            Err(PublishError::MissingMasterKey)
        );
        assert_eq!(
            validate_publish(
                &app,
                channel_name.as_str(),
                r#"{"nonce":"n","ciphertext":"c"}"#
            ),
            Ok(())
        );
        assert_eq!(
            validate_publish(&app, "private-plain", "top secret"),
            Ok(())
        );
        let result = dispatcher
            .dispatch_message(
                app,
                channel_name.as_str().to_string(),
                "my-event".to_string(),
                "top secret".to_string(),
                None,
            )
            .await;
        assert_eq!(result, Err(PublishError::MissingMasterKey));

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

    /// Opt-in `PublishOverflow::Block` must not drop events that fit its
    /// staging budget. With a slow provider + burst > capacity, best-effort
    /// drops; block mode does not.
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
            "block mode must not drop payloads within its staging budget"
        );
    }

    /// Block mode's staging buffer is byte-bounded: once the budget is used
    /// up, further envelopes are dropped and counted instead of growing
    /// memory without limit behind a stalled Redis.
    #[tokio::test]
    async fn block_mode_staging_is_byte_bounded() {
        let channels = ChannelManager::new();
        let provider = std::sync::Arc::new(SlowProvider { delay_ms: 60_000 });
        let mut dispatcher =
            EventDispatcher::with_overflow(channels, provider, true, PublishOverflow::Block);
        dispatcher.block_tx.as_mut().unwrap().limit = 64 * 1024;
        let app = mk_app();

        let burst = PUBLISH_QUEUE_CAPACITY + 64 + 5_000;
        for _ in 0..burst {
            dispatcher
                .publish_terminate(&app, SocketId::from_string("1.2".into()))
                .await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let staged = dispatcher
            .block_tx
            .as_ref()
            .unwrap()
            .bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(staged <= 64 * 1024, "staged {staged} bytes past the budget");
        assert!(
            dispatcher.publish_drops_total() > 0,
            "overflow past the staging budget must be counted"
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
        let app = mk_app_counting_subscriptions();

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
            seq: 0,
            app: AppRef {
                id: "app-1".into(),
                key: "dev-key".into(),
            },
            payload: ScalingPayload::SubscriptionCount {
                origin_node_id: "other-node".into(),
                channel: "cache-room".into(),
                count: 3,
                vacated_withheld: false,
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
            seq: 0,
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
            seq: 0,
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
                seq: 0,
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
                    connections: 1,
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
                    connections_only: false,
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
