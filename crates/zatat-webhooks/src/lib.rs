#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::mpsc;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(default)]
    pub event_types: Vec<String>,
    #[serde(default)]
    pub filter_by_prefix: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CompiledTarget {
    pub app_id: String,
    pub app_key: String,
    pub app_secret: String,
    pub url: String,
    pub event_filter: Vec<String>,
    pub channel_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WebhookEvent {
    ChannelOccupied {
        channel: String,
    },
    ChannelVacated {
        channel: String,
    },
    MemberAdded {
        channel: String,
        user_id: String,
    },
    MemberRemoved {
        channel: String,
        user_id: String,
    },
    ClientEvent {
        channel: String,
        event: String,
        data: String,
        socket_id: Option<String>,
        user_id: Option<String>,
    },
    CacheMiss {
        channel: String,
    },
    SubscriptionCount {
        channel: String,
        count: usize,
    },
}

impl WebhookEvent {
    pub fn name(&self) -> &'static str {
        match self {
            WebhookEvent::ChannelOccupied { .. } => "channel_occupied",
            WebhookEvent::ChannelVacated { .. } => "channel_vacated",
            WebhookEvent::MemberAdded { .. } => "member_added",
            WebhookEvent::MemberRemoved { .. } => "member_removed",
            WebhookEvent::ClientEvent { .. } => "client_event",
            WebhookEvent::CacheMiss { .. } => "cache_miss",
            WebhookEvent::SubscriptionCount { .. } => "subscription_count",
        }
    }

    pub fn channel(&self) -> &str {
        match self {
            WebhookEvent::ChannelOccupied { channel } => channel,
            WebhookEvent::ChannelVacated { channel } => channel,
            WebhookEvent::MemberAdded { channel, .. } => channel,
            WebhookEvent::MemberRemoved { channel, .. } => channel,
            WebhookEvent::ClientEvent { channel, .. } => channel,
            WebhookEvent::CacheMiss { channel } => channel,
            WebhookEvent::SubscriptionCount { channel, .. } => channel,
        }
    }

    fn to_event_json(&self) -> Value {
        let name = self.name();
        match self {
            WebhookEvent::ChannelOccupied { channel } => {
                json!({ "name": name, "channel": channel })
            }
            WebhookEvent::ChannelVacated { channel } => json!({ "name": name, "channel": channel }),
            WebhookEvent::MemberAdded { channel, user_id } => {
                json!({ "name": name, "channel": channel, "user_id": user_id })
            }
            WebhookEvent::MemberRemoved { channel, user_id } => {
                json!({ "name": name, "channel": channel, "user_id": user_id })
            }
            WebhookEvent::ClientEvent {
                channel,
                event,
                data,
                socket_id,
                user_id,
            } => {
                let mut obj = serde_json::Map::new();
                obj.insert("name".into(), Value::String(name.into()));
                obj.insert("channel".into(), Value::String(channel.clone()));
                obj.insert("event".into(), Value::String(event.clone()));
                obj.insert("data".into(), Value::String(data.clone()));
                if let Some(s) = socket_id {
                    obj.insert("socket_id".into(), Value::String(s.clone()));
                }
                if let Some(u) = user_id {
                    obj.insert("user_id".into(), Value::String(u.clone()));
                }
                Value::Object(obj)
            }
            WebhookEvent::CacheMiss { channel } => json!({ "name": name, "channel": channel }),
            WebhookEvent::SubscriptionCount { channel, count } => {
                json!({ "name": name, "channel": channel, "subscription_count": count })
            }
        }
    }
}

type Lookup = Arc<dyn Fn(&str) -> Vec<CompiledTarget> + Send + Sync>;

pub struct WebhookDispatcher {
    tx: mpsc::Sender<(String, WebhookEvent)>,
}

impl WebhookDispatcher {
    pub fn spawn<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Vec<CompiledTarget> + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<(String, WebhookEvent)>(1024);
        let lookup: Lookup = Arc::new(lookup);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");
        tokio::spawn(async move {
            while let Some((app_id, event)) = rx.recv().await {
                let targets = (lookup)(&app_id);
                for t in targets {
                    if !matches_filters(&t, &event) {
                        continue;
                    }
                    let client = client.clone();
                    tokio::spawn(deliver(client, t, event.clone()));
                }
            }
        });
        Self { tx }
    }

    pub fn enqueue(&self, app_id: &str, event: WebhookEvent) {
        let _ = self.tx.try_send((app_id.to_string(), event));
    }
}

fn matches_filters(target: &CompiledTarget, event: &WebhookEvent) -> bool {
    if !target.event_filter.is_empty() && !target.event_filter.iter().any(|n| n == event.name()) {
        return false;
    }
    if let Some(prefix) = &target.channel_prefix {
        if !event.channel().starts_with(prefix) {
            return false;
        }
    }
    true
}

async fn deliver(client: reqwest::Client, target: CompiledTarget, event: WebhookEvent) {
    let envelope = json!({
        "time_ms": now_millis(),
        "events": [event.to_event_json()],
    });
    let body = serde_json::to_vec(&envelope).unwrap_or_default();
    let signature = sign_body(&target.app_secret, &body);
    let mut attempts = 0u32;
    let mut backoff_ms = 500u64;
    loop {
        attempts += 1;
        let res = client
            .post(&target.url)
            .header("Content-Type", "application/json")
            .header("X-Pusher-Key", &target.app_key)
            .header("X-Pusher-Signature", &signature)
            .body(body.clone())
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                debug!(app = %target.app_id, url = %target.url, "webhook delivered");
                return;
            }
            Ok(r) => {
                warn!(app = %target.app_id, url = %target.url, status = %r.status(), "webhook non-2xx");
            }
            Err(err) => {
                warn!(app = %target.app_id, url = %target.url, %err, "webhook transport error");
            }
        }
        if attempts >= 4 {
            warn!(app = %target.app_id, url = %target.url, "webhook giving up after 4 attempts");
            return;
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
    }
}

fn sign_body(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_round_trip() {
        let body = b"{\"events\":[]}";
        let sig = sign_body("s", body);
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn envelope_has_time_ms_and_events() {
        let ev = WebhookEvent::ChannelOccupied {
            channel: "x".into(),
        };
        let env = json!({ "time_ms": 1, "events": [ev.to_event_json()] });
        assert_eq!(env["events"][0]["name"], "channel_occupied");
        assert_eq!(env["events"][0]["channel"], "x");
    }
}
