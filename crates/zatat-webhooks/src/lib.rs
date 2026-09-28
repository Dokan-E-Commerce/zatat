#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
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

    /// Rough heap footprint, for the `Block` staging byte budget.
    fn approx_size(&self) -> usize {
        let payload = match self {
            WebhookEvent::MemberAdded { user_id, .. }
            | WebhookEvent::MemberRemoved { user_id, .. } => user_id.len(),
            WebhookEvent::ClientEvent {
                event,
                data,
                socket_id,
                user_id,
                ..
            } => {
                event.len()
                    + data.len()
                    + socket_id.as_ref().map_or(0, String::len)
                    + user_id.as_ref().map_or(0, String::len)
            }
            _ => 0,
        };
        std::mem::size_of::<Self>() + self.channel().len() + payload
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
/// Pusher retries a failed webhook with exponential backoff for 5 minutes.
const RETRY_WINDOW: Duration = Duration::from_secs(300);
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);
/// Deliveries waiting to retry hold no delivery slot, only memory; past
/// this many a newly failing delivery is abandoned (and counted).
const MAX_PENDING_RETRIES: usize = 10_000;
/// Byte budget for `Block` mode's ordered staging buffer.
const BLOCK_STAGING_MAX_BYTES: usize = 64 * 1024 * 1024;

/// What to do when the in-memory webhook queue is full.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WebhookOverflow {
    /// Drop the event + count it (`zatat_webhooks_dropped_total`).
    /// Default — never blocks the caller.
    #[default]
    BestEffort,
    /// Absorb overload in an ordered staging buffer of up to
    /// `BLOCK_STAGING_MAX_BYTES` in front of the bounded queue; drop and
    /// count only once that is exhausted. `enqueue()` never waits.
    Block,
}

/// `Block` mode's staging buffer: an ordered queue with a byte budget.
struct BlockStaging {
    tx: mpsc::UnboundedSender<(String, WebhookEvent)>,
    bytes: Arc<AtomicUsize>,
    limit: usize,
}

pub struct WebhookDispatcher {
    tx: mpsc::Sender<(String, WebhookEvent)>,
    overflow: WebhookOverflow,
    /// Only `Some` when `overflow` is `Block`. The producer side sends here
    /// synchronously (never blocks, never spawns) within a byte budget; a
    /// single dedicated forwarder task drains it in order into the bounded
    /// `tx` queue, awaiting a slot as needed.
    block_tx: Option<BlockStaging>,
    drops_total: Arc<AtomicU64>,
    last_drop_warn: Arc<Mutex<Option<Instant>>>,
    in_flight: Arc<AtomicU64>,
    delivery_semaphore: Arc<Semaphore>,
    /// 1 while the drain loop holds an event it has not handed off yet.
    dequeued: Arc<AtomicUsize>,
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
        let pending_retries = Arc::new(AtomicUsize::new(0));
        let dequeued = Arc::new(AtomicUsize::new(0));
        let dequeued_drain = dequeued.clone();
        tokio::spawn(async move {
            let dequeued = dequeued_drain;
            while let Some((app_id, event)) = rx.recv().await {
                // Counted as pending while this loop holds it, including
                // while it waits for a delivery slot.
                dequeued.store(1, Ordering::Relaxed);
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
                    let retry = RetryContext {
                        semaphore: sem_drain.clone(),
                        pending: pending_retries.clone(),
                    };
                    tokio::spawn(async move {
                        metrics::gauge!("zatat_webhooks_in_flight")
                            .set(in_flight_tx.load(Ordering::Relaxed) as f64);
                        deliver(client, t, event_for_task, permit, retry).await;
                        in_flight_tx.fetch_sub(1, Ordering::Relaxed);
                        metrics::gauge!("zatat_webhooks_in_flight")
                            .set(in_flight_tx.load(Ordering::Relaxed) as f64);
                    });
                }
                dequeued.store(0, Ordering::Relaxed);
            }
        });

        // In `Block` mode, a single dedicated forwarder task drains the
        // byte-budgeted staging queue into the bounded `tx`, one item at a
        // time, preserving order without spawning a task per message.
        let block_tx = if overflow == WebhookOverflow::Block {
            let bounded_tx = tx.clone();
            let (staging_tx, mut staging_rx) = mpsc::unbounded_channel::<(String, WebhookEvent)>();
            let bytes = Arc::new(AtomicUsize::new(0));
            let staged = bytes.clone();
            tokio::spawn(async move {
                while let Some(item) = staging_rx.recv().await {
                    let size = item.1.approx_size();
                    let sent = bounded_tx.send(item).await;
                    staged.fetch_sub(size, Ordering::Relaxed);
                    if sent.is_err() {
                        break;
                    }
                }
            });
            Some(BlockStaging {
                tx: staging_tx,
                bytes,
                limit: BLOCK_STAGING_MAX_BYTES,
            })
        } else {
            None
        };

        Self {
            tx,
            overflow,
            block_tx,
            drops_total: Arc::new(AtomicU64::new(0)),
            last_drop_warn: Arc::new(Mutex::new(None)),
            in_flight,
            delivery_semaphore: sem,
            dequeued,
        }
    }

    pub fn enqueue(&self, app_id: &str, event: WebhookEvent) {
        let staged = self
            .block_tx
            .as_ref()
            .map_or(0, |s| s.bytes.load(Ordering::Relaxed));
        metrics::gauge!("zatat_webhooks_queue_depth")
            .set((WEBHOOK_QUEUE_CAPACITY - self.tx.capacity()) as f64);
        metrics::gauge!("zatat_webhooks_staging_bytes").set(staged as f64);
        match self.overflow {
            WebhookOverflow::BestEffort => {
                if self.tx.try_send((app_id.to_string(), event)).is_err() {
                    self.record_drop(app_id);
                }
            }
            WebhookOverflow::Block => {
                let Some(staging) = &self.block_tx else {
                    return;
                };
                let size = event.approx_size();
                let reserved =
                    staging
                        .bytes
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                            (used + size <= staging.limit).then_some(used + size)
                        });
                if reserved.is_err() {
                    self.record_drop(app_id);
                    return;
                }
                if staging.tx.send((app_id.to_string(), event)).is_err() {
                    staging.bytes.fetch_sub(size, Ordering::Relaxed);
                }
            }
        }
    }

    fn record_drop(&self, app_id: &str) {
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

    /// Test / metrics hook: how many events have been dropped since startup.
    pub fn drops_total(&self) -> u64 {
        self.drops_total.load(Ordering::Relaxed)
    }

    /// Current count of in-flight webhook deliveries. Capped at
    /// MAX_IN_FLIGHT_DELIVERIES by the internal semaphore.
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Work not yet finished: queued events, staged `Block` events and
    /// deliveries in flight or awaiting a retry. Zero once fully drained.
    pub fn pending(&self) -> usize {
        let queued = WEBHOOK_QUEUE_CAPACITY - self.tx.capacity();
        let staged = self
            .block_tx
            .as_ref()
            .map_or(0, |s| usize::from(s.bytes.load(Ordering::Relaxed) > 0));
        queued
            + staged
            + self.dequeued.load(Ordering::Relaxed)
            + self.in_flight.load(Ordering::Relaxed) as usize
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

/// Shared state that lets a delivery retry without holding a delivery slot
/// while it waits.
struct RetryContext {
    semaphore: Arc<Semaphore>,
    pending: Arc<AtomicUsize>,
}

/// Delivers one event to one target. Like Pusher, any non-2xx response or
/// transport error is retried with exponential backoff until
/// `RETRY_WINDOW` has elapsed. The delivery slot (`permit`) is held only
/// while a request is in flight.
async fn deliver(
    client: reqwest::Client,
    target: CompiledTarget,
    event: WebhookEvent,
    permit: OwnedSemaphorePermit,
    retry: RetryContext,
) {
    let envelope = json!({
        "time_ms": now_millis(),
        "events": [event.to_event_json()],
    });
    let body = serde_json::to_vec(&envelope).unwrap_or_default();
    let signature = sign_body(&target.app_secret, &body);
    let started = Instant::now();
    let mut backoff = INITIAL_RETRY_BACKOFF;
    let mut permit = Some(permit);
    let mut counted_as_pending = false;
    let mut attempts = 0u32;
    let delivered = loop {
        let slot = match permit.take() {
            Some(p) => p,
            None => match retry.semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break false,
            },
        };
        attempts += 1;
        let res = client
            .post(&target.url)
            .header("Content-Type", "application/json")
            .header("X-Pusher-Key", &target.app_key)
            .header("X-Pusher-Signature", &signature)
            .body(body.clone())
            .send()
            .await;
        drop(slot);
        match res {
            Ok(r) if r.status().is_success() => break true,
            Ok(r) => {
                warn!(app = %target.app_id, url = %target.url, status = %r.status(), attempts, "webhook non-2xx");
            }
            Err(err) => {
                warn!(app = %target.app_id, url = %target.url, %err, attempts, "webhook transport error");
            }
        }
        if started.elapsed() + backoff > RETRY_WINDOW {
            break false;
        }
        if !counted_as_pending {
            if retry.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_RETRIES {
                retry.pending.fetch_sub(1, Ordering::Relaxed);
                warn!(app = %target.app_id, url = %target.url, "too many webhooks awaiting retry; abandoning this one");
                break false;
            }
            counted_as_pending = true;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_RETRY_BACKOFF);
    };
    if counted_as_pending {
        retry.pending.fetch_sub(1, Ordering::Relaxed);
    }
    if delivered {
        debug!(app = %target.app_id, url = %target.url, attempts, "webhook delivered");
    } else {
        metrics::counter!("zatat_webhooks_failed_total").increment(1);
        warn!(app = %target.app_id, url = %target.url, attempts, "webhook delivery abandoned");
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

    /// Opt-in `WebhookOverflow::Block` must not drop events that fit its
    /// staging budget, even past the bounded queue's capacity.
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
            "block mode must not drop within its staging budget; got {} drops",
            d.drops_total()
        );
    }

    /// Pusher retries any non-2xx (not only 5xx/408/429) with backoff. A
    /// receiver answering 404 then 200 must see exactly two requests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_2xx_is_retried_until_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = server_hits.fetch_add(1, Ordering::SeqCst);
                let mut buf = vec![0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let status = if n == 0 { "404 Not Found" } else { "200 OK" };
                let _ = sock
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        let url = format!("http://{addr}/hook");
        let d = WebhookDispatcher::spawn(move |_| {
            vec![CompiledTarget {
                app_id: "app-1".into(),
                app_key: "key".into(),
                app_secret: "secret".into(),
                url: url.clone(),
                event_filter: Vec::new(),
                channel_prefix: None,
            }]
        });
        d.enqueue(
            "app-1",
            WebhookEvent::ChannelOccupied {
                channel: "ch".into(),
            },
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.in_flight() == 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        while d.in_flight() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2, "one 404, then one retry");
        assert_eq!(d.in_flight(), 0, "delivery finished after the 200");
        assert_eq!(d.available_permits(), MAX_IN_FLIGHT_DELIVERIES);
    }

    /// Block mode's staging buffer is byte-bounded; past the budget events
    /// are dropped and counted rather than growing memory without limit.
    #[tokio::test(flavor = "current_thread")]
    async fn block_mode_staging_is_byte_bounded() {
        let mut d = WebhookDispatcher::spawn_with_overflow(|_| Vec::new(), WebhookOverflow::Block);
        d.block_tx.as_mut().unwrap().limit = 4096;
        // current_thread: the forwarder cannot run until we yield, so every
        // event below stays staged.
        for i in 0..1000 {
            d.enqueue(
                "app-1",
                WebhookEvent::ChannelOccupied {
                    channel: format!("ch-{i}"),
                },
            );
        }
        let staged = d.block_tx.as_ref().unwrap().bytes.load(Ordering::Relaxed);
        assert!(staged <= 4096, "staged {staged} bytes past the budget");
        assert!(d.drops_total() > 0);
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
