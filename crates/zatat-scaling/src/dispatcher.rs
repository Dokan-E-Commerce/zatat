use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
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
    AppRef, ChannelMetric, MetricsQuery, PresenceSnapshotMember, ScalingEnvelope, ScalingPayload,
    SCALING_VERSION,
};
use crate::presence_cache::PresenceCache;
use crate::provider::PubSubProvider;

/// Encrypts `private-encrypted-*` payloads server-side when the app has a
/// master key configured and the client did not already encrypt the payload.
fn maybe_encrypt(app: &AppArc, kind: ChannelKind, channel_name: &str, data: &str) -> String {
    if !kind.is_encrypted() {
        return data.to_string();
    }
    let Some(master_b64) = app.encryption_master_key.as_deref() else {
        return data.to_string();
    };
    if looks_encrypted(data) {
        return data.to_string();
    }
    let master = match decode_master_key(master_b64) {
        Ok(k) => k,
        Err(err) => {
            warn!(app = %app.id, %err, "invalid encryption_master_key; broadcast passed through");
            return data.to_string();
        }
    };
    let secret = derive_shared_secret(channel_name, &master);
    match encrypt_payload(data.as_bytes(), &secret) {
        Ok(s) => s,
        Err(err) => {
            warn!(%err, "encryption failed; broadcast passed through");
            data.to_string()
        }
    }
}

pub struct EventDispatcher {
    channels: ChannelManager,
    provider: Arc<dyn PubSubProvider>,
    scaling_enabled: bool,
    node_id: String,
    presence_cache: Arc<PresenceCache>,
    metrics_inflight: Arc<DashMap<String, mpsc::UnboundedSender<ChannelMetric>>>,
}

impl EventDispatcher {
    pub fn new(
        channels: ChannelManager,
        provider: Arc<dyn PubSubProvider>,
        scaling_enabled: bool,
    ) -> Self {
        Self {
            channels,
            provider,
            scaling_enabled,
            node_id: Uuid::new_v4().to_string(),
            presence_cache: Arc::new(PresenceCache::new()),
            metrics_inflight: Arc::new(DashMap::new()),
        }
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn presence_cache(&self) -> Arc<PresenceCache> {
        self.presence_cache.clone()
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
        let data = maybe_encrypt(&app, kind, &channel_name, &data);

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
            let bytes = serde_json::to_vec(&env).unwrap_or_default();
            let provider = self.provider.clone();
            tokio::spawn(async move {
                let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
            });
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
        });
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
            let bytes = serde_json::to_vec(&env).unwrap_or_default();
            let provider = self.provider.clone();
            tokio::spawn(async move {
                let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
            });
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
        });
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
        });
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
        });
    }

    pub fn handle_incoming(
        &self,
        env: ScalingEnvelope,
        apps_by_id_lookup: impl Fn(&AppId) -> Option<AppArc>,
    ) {
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
                self.broadcast_locally(&app.id, &channel, &event, &data, except.as_ref());
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
                channels: metrics,
                ..
            } => {
                // Keep the sender registered: peers may each respond.
                // The originator unregisters when the wait window closes.
                if let Some(entry) = self.metrics_inflight.get(&request_id) {
                    let tx = entry.clone();
                    drop(entry);
                    for m in metrics {
                        let _ = tx.send(m);
                    }
                }
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(2), provider.publish(bytes)).await;
        });
    }

    /// Fires a MetricsRequest on the scaling bus and collects responses
    /// until `wait` elapses. Returns `None` when scaling is disabled.
    pub async fn ask_fleet_for_channels(
        &self,
        app: &AppArc,
        query: MetricsQuery,
        wait: Duration,
    ) -> Option<Vec<ChannelMetric>> {
        if !self.scaling_enabled {
            return None;
        }
        let request_id = Uuid::new_v4().to_string();
        let (tx, mut rx) = mpsc::unbounded_channel::<ChannelMetric>();
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
        let bytes = serde_json::to_vec(&env).unwrap_or_default();
        let _ = timeout(Duration::from_secs(2), self.provider.publish(bytes)).await;

        let mut out: Vec<ChannelMetric> = Vec::new();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(m)) => out.push(m),
                _ => break,
            }
        }
        self.metrics_inflight.remove(&request_id);
        Some(out)
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
