use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SCALING_VERSION: u8 = 2;
pub const SNAPSHOT_TTL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScalingEnvelope {
    #[serde(rename = "v")]
    pub version: u8,
    pub app: AppRef,
    #[serde(flatten)]
    pub payload: ScalingPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRef {
    pub id: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScalingPayload {
    Message {
        #[serde(default)]
        origin_node_id: String,
        channel: String,
        event: String,
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        except_socket_id: Option<String>,
    },
    Terminate {
        socket_id: String,
    },
    TerminateUser {
        #[serde(default)]
        origin_node_id: String,
        user_id: String,
    },
    ClientEvent {
        #[serde(default)]
        origin_node_id: String,
        channel: String,
        event: String,
        data: String,
        socket_id: String,
    },
    PresenceSnapshot {
        node_id: String,
        channel: String,
        members: Vec<PresenceSnapshotMember>,
    },
    UserEvent {
        #[serde(default)]
        origin_node_id: String,
        user_id: String,
        event: String,
        data: String,
    },
    MetricsRequest {
        request_id: String,
        requester_node_id: String,
        query: MetricsQuery,
    },
    MetricsResponse {
        request_id: String,
        node_id: String,
        channels: Vec<ChannelMetric>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsQuery {
    #[serde(default)]
    pub filter_by_prefix: Option<String>,
    #[serde(default)]
    pub info: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMetric {
    pub name: String,
    pub occupied: bool,
    pub subscription_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_count: Option<usize>,
    #[serde(default)]
    pub has_cached_payload: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presence_user_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresenceSnapshotMember {
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_info: Option<Value>,
}

pub fn parse(bytes: &[u8]) -> Result<ScalingEnvelope, serde_json::Error> {
    let env: ScalingEnvelope = serde_json::from_slice(bytes)?;
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_message_payload() {
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "a".into(),
                key: "k".into(),
            },
            payload: ScalingPayload::Message {
                origin_node_id: "n1".into(),
                channel: "c".into(),
                event: "e".into(),
                data: "{\"x\":1}".into(),
                except_socket_id: None,
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: ScalingEnvelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.payload, ScalingPayload::Message { .. }));
    }

    #[test]
    fn round_trip_terminate() {
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "a".into(),
                key: "k".into(),
            },
            payload: ScalingPayload::Terminate {
                socket_id: "1.2".into(),
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: ScalingEnvelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(back.payload, ScalingPayload::Terminate { .. }));
    }

    #[test]
    fn round_trip_presence_snapshot() {
        let env = ScalingEnvelope {
            version: SCALING_VERSION,
            app: AppRef {
                id: "a".into(),
                key: "k".into(),
            },
            payload: ScalingPayload::PresenceSnapshot {
                node_id: "n1".into(),
                channel: "presence-room".into(),
                members: vec![PresenceSnapshotMember {
                    user_id: "u".into(),
                    user_info: None,
                }],
            },
        };
        let s = serde_json::to_string(&env).unwrap();
        let back: ScalingEnvelope = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back.payload,
            ScalingPayload::PresenceSnapshot { .. }
        ));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(b"{\"not\":\"valid\"}").is_err());
    }

    #[test]
    fn version_field_is_not_validated_by_serde() {
        let s = r#"{"v":99,"app":{"id":"a","key":"k"},"type":"terminate","socket_id":"1.2"}"#;
        let back: ScalingEnvelope = serde_json::from_str(s).unwrap();
        assert_eq!(back.version, 99);
    }
}
