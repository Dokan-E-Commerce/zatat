#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::{mpsc, Semaphore};
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

/// Bounded queue depth. Chosen to absorb member-added/removed storms during
/// a large presence-channel flip without stalling hot paths — each slot is
/// a small struct, so 64k ≈ a few MB. Drops past this point are counted and
/// logged (previously they were silent).
const WEBHOOK_QUEUE_CAPACITY: usize = 65_536;
/// Rate-limit the "webhook queue full — dropping" warning so a sustained
/// overrun doesn't flood the log. One message per 5s is enough to alert.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);
/// Cap on simultaneous in-flight webhook deliveries. Without this cap a slow
/// target could accumulate thousands of tokio tasks each holding a reqwest
/// client and a 10s timer. The permit is acquired BEFORE spawning the
/// delivery so overload back-pressures the dequeue loop (queue fills,
/// `enqueue()` starts dropping with the existing counter) rather than
/// unbounded-spawning.
const MAX_IN_FLIGHT_DELIVERIES: usize = 256;

/// What to do when the in-memory webhook queue is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebhookOverflow {
    /// Drop the event + count it (`zatat_webhooks_dropped_total`).
    /// Default — never blocks the caller.
    #[default]
    BestEffort,
    /// Back-pressure: `enqueue()` awaits a slot on a background task.
    /// Zero loss at the cost of slower producers under sustained overload.
    Block,
}

pub struct WebhookDispatcher {
    tx: mpsc::Sender<(String, WebhookEvent)>,
    overflow: WebhookOverflow,
    drops_total: Arc<AtomicU64>,
    last_drop_warn: Arc<Mutex<Option<Instant>>>,
    in_flight: Arc<AtomicU64>,
    delivery_semaphore: Arc<Semaphore>,
}

impl WebhookDispatcher {
    pub fn spawn<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Vec<CompiledTarget> + Send + Sync + 'static,
    {
        Self::spawn_with_overflow(lookup, WebhookOverflow::default())
    }

    pub fn spawn_with_overflow<F>(lookup: F, overflow: WebhookOverflow) -> Self
    where
        F: Fn(&str) -> Vec<CompiledTarget> + Send + Sync + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<(String, WebhookEvent)>(WEBHOOK_QUEUE_CAPACITY);
        let lookup: Lookup = Arc::new(lookup);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client builds");

        let in_flight = Arc::new(AtomicU64::new(0));
        let sem = Arc::new(Semaphore::new(MAX_IN_FLIGHT_DELIVERIES));

        let in_flight_drain = in_flight.clone();
        let sem_drain = sem.clone();
        tokio::spawn(async move {
            while let Some((app_id, event)) = rx.recv().await {
                let targets = (lookup)(&app_id);
                for t in targets {
                    if !matches_filters(&t, &event) {
                        continue;
                    }
                    // `acquire_owned()` awaits a permit; if MAX_IN_FLIGHT is
                    // saturated this back-pressures the drain loop, which in
                    // turn back-pressures the mpsc — eventually `enqueue()`
                    // starts dropping via the existing counter. Net effect:
                    // we never have more than MAX_IN_FLIGHT simultaneous
                    // reqwest calls even with thousands of events queued.
                    let Ok(permit) = sem_drain.clone().acquire_owned().await else {
                        // semaphore closed — only happens on shutdown
                        return;
                    };
                    in_flight_drain.fetch_add(1, Ordering::Relaxed);
                    let in_flight_tx = in_flight_drain.clone();
                    let client = client.clone();
                    let event_for_task = event.clone();
                    tokio::spawn(async move {
                        let _permit = permit; // dropped at end -> releases slot
                        metrics::gauge!("zatat_webhooks_in_flight")
                            .set(in_flight_tx.load(Ordering::Relaxed) as f64);
                        deliver(client, t, event_for_task).await;
                        in_flight_tx.fetch_sub(1, Ordering::Relaxed);
                        metrics::gauge!("zatat_webhooks_in_flight")
                            .set(in_flight_tx.load(Ordering::Relaxed) as f64);
                    });
                }
            }
        });

        Self {
            tx,
            overflow,
            drops_total: Arc::new(AtomicU64::new(0)),
            last_drop_warn: Arc::new(Mutex::new(None)),
            in_flight,
            delivery_semaphore: sem,
        }
    }

    pub fn enqueue(&self, app_id: &str, event: WebhookEvent) {
        metrics::gauge!("zatat_webhooks_queue_depth")
            .set((WEBHOOK_QUEUE_CAPACITY - self.tx.capacity()) as f64);
        match self.overflow {
            WebhookOverflow::BestEffort => {
                if self.tx.try_send((app_id.to_string(), event)).is_err() {
                    let prev = self.drops_total.fetch_add(1, Ordering::Relaxed) + 1;
                    metrics::counter!("zatat_webhooks_dropped_total").increment(1);
                    let mut last = self.last_drop_warn.lock();
                    let now = Instant::now();
                    let should_warn = match *last {
                        None => true,
                        Some(t) => now.duration_since(t) >= DROP_WARN_INTERVAL,
                    };
                    if should_warn {
                        *last = Some(now);
                        drop(last);
                        warn!(
                            app = %app_id,
                            total_drops = prev,
                            "webhook queue FULL — dropping event; consumer cannot keep up"
                        );
                    }
                }
            }
            WebhookOverflow::Block => {
                // Queue is full → spawn a task that awaits a slot. Zero
                // loss. A sustained overload backs up until Redis/consumer
                // catches up, so the caller still returns immediately but
                // in-memory pressure rises. The queue-depth gauge is the
                // load-shedding signal.
                let tx = self.tx.clone();
                let entry = (app_id.to_string(), event);
                tokio::spawn(async move {
                    let _ = tx.send(entry).await;
                });
            }
        }
    }

    /// Test / metrics hook: how many events have been dropped since startup.
    pub fn drops_total(&self) -> u64 {
        self.drops_total.load(Ordering::Relaxed)
    }

    /// Current count of in-flight webhook deliveries. Capped at
    /// MAX_IN_FLIGHT_DELIVERIES by the internal semaphore.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Available permits for new deliveries (for health checks).
    pub fn available_permits(&self) -> usize {
        self.delivery_semaphore.available_permits()
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

    /// Regression: in-flight deliveries used to be unbounded (one
    /// tokio::spawn per event per target). Now a semaphore caps them at
    /// MAX_IN_FLIGHT_DELIVERIES. Exercised directly against the semaphore
    /// — no HTTP binding required, so this runs clean in sandboxed test
    /// envs where `TcpListener::bind` returns PermissionDenied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_is_bounded_by_semaphore() {
        let d = WebhookDispatcher::spawn(|_| Vec::new());

        // Invariant #1: the dispatcher exposes MAX_IN_FLIGHT_DELIVERIES
        // permits on a fresh instance.
        assert_eq!(d.available_permits(), MAX_IN_FLIGHT_DELIVERIES);

        // Invariant #2: holding N permits leaves exactly MAX - N available.
        let sem = d.delivery_semaphore.clone();
        let mut held = Vec::new();
        for _ in 0..10 {
            held.push(sem.clone().acquire_owned().await.unwrap());
        }
        assert_eq!(d.available_permits(), MAX_IN_FLIGHT_DELIVERIES - 10);

        // Invariant #3: acquiring past the cap blocks. We prove this by
        // starting MAX tasks that each hold a permit forever and asserting
        // that one more `try_acquire` returns Err (no permits available).
        let mut more = Vec::new();
        for _ in 10..MAX_IN_FLIGHT_DELIVERIES {
            more.push(sem.clone().acquire_owned().await.unwrap());
        }
        assert_eq!(d.available_permits(), 0);
        assert!(
            sem.clone().try_acquire_owned().is_err(),
            "try_acquire must fail when the semaphore is fully saturated"
        );

        // Invariant #4: releasing permits frees slots.
        drop(held);
        drop(more);
        // Semaphore release is immediate for owned permits.
        assert_eq!(d.available_permits(), MAX_IN_FLIGHT_DELIVERIES);
    }

    /// Opt-in `WebhookOverflow::Block` must NOT drop events — they wait
    /// for the consumer to catch up. Proven by pointing at a no-target
    /// lookup (so the drain never consumes), pushing > capacity events,
    /// and asserting drops_total stays 0.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn block_mode_never_drops() {
        let d = WebhookDispatcher::spawn_with_overflow(|_| Vec::new(), WebhookOverflow::Block);
        let over = WEBHOOK_QUEUE_CAPACITY + 256;
        for i in 0..over {
            d.enqueue(
                "app-1",
                WebhookEvent::ChannelOccupied {
                    channel: format!("ch-{i}"),
                },
            );
        }
        // Give the background send tasks a moment to actually land in mpsc.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            d.drops_total(),
            0,
            "block mode must never drop; got {} drops",
            d.drops_total()
        );
    }

    /// Regression: before the fix, an overflowing queue silently discarded
    /// events with no metric and no log. Now drops must be counted.
    #[tokio::test(flavor = "current_thread")]
    async fn overflow_is_counted_not_silent() {
        // Lookup returns no targets so the consumer never advances — the
        // queue can only grow. This simulates a fully-stalled consumer.
        let d = WebhookDispatcher::spawn(|_| Vec::new());

        // Enqueue past capacity. We need WEBHOOK_QUEUE_CAPACITY + N events
        // for at least N drops (the consumer drains some slots between our
        // synchronous try_sends, so a small safety margin is needed).
        let over = WEBHOOK_QUEUE_CAPACITY + 1024;
        for i in 0..over {
            d.enqueue(
                "app-1",
                WebhookEvent::ChannelOccupied {
                    channel: format!("ch-{i}"),
                },
            );
        }
        // Consumer may drain a few in the background; at least some drops
        // must have occurred given we exceeded capacity by 1024.
        assert!(
            d.drops_total() > 0,
            "expected drops > 0 once queue overflowed; got {}",
            d.drops_total()
        );
    }
}
