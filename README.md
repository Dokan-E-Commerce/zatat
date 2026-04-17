# Zatat

A Pusher-compatible WebSocket server, written in Rust.

zatat speaks the [Pusher Channels protocol v7][pusher-protocol]. Any client
or backend library that talks to Pusher or Laravel Reverb — pusher-js,
Laravel Echo, `pusher-http-node`, `pusher-http-php`, `pusher-http-python`,
`pusher-http-go`, `pusher-http-ruby`, the iOS / Android / Flutter SDKs, or a
plain HTTP client — connects to zatat without code changes. Point it at
`ws://host:8080/app/YOUR_KEY` and it just works.

The name means "in a hurry" (زتات) in Bahraini Arabic. Seemed fitting.

[pusher-protocol]: https://pusher.com/docs/channels/library_auth_reference/pusher-websockets-protocol/

---

## Contents

- [Why zatat](#why-zatat)
- [Feature matrix](#feature-matrix)
- [Install](#install)
- [Configuration](#configuration)
- [Channels](#channels)
- [Client events (whispers)](#client-events-whispers)
- [User authentication & server-to-user events](#user-authentication--server-to-user-events)
- [Watchlist events](#watchlist-events)
- [HTTP API](#http-api)
- [Webhooks](#webhooks)
- [Private-encrypted channels](#private-encrypted-channels)
- [Scaling with Redis](#scaling-with-redis)
- [TLS](#tls)
- [Metrics and logs](#metrics-and-logs)
- [CLI](#cli)
- [Docker](#docker)
- [Backends](#backends) — Node.js · PHP · Python · Ruby · Go · .NET · raw HTTP · Laravel
- [Frontends](#frontends) — pusher-js · Laravel Echo · iOS · Android · Flutter
- [Migrating from Pusher or Reverb](#migrating-from-pusher-or-reverb)
- [Performance](#performance)
- [Protocol compliance notes](#protocol-compliance-notes)
- [Production-readiness](#production-readiness)
- [Development](#development)
- [License](#license)

---

## Why Zatat

- **Drop-in.** Speaks the Pusher protocol byte-for-byte — every frame
  (`connection_established`, `subscription_succeeded`, `member_added`,
  `pusher:error`, `cache_miss`, …) matches Reverb's output on the same
  inputs. Verified by a live diff.
- **10× the capacity on the same hardware.** On a €3.99/mo Hetzner CX23
  (2 vCPU / 4 GB / shared-AMD), zatat serves **10,000 concurrent
  WebSocket connections** with zero failures. Reverb on the same box
  caps out at ~1,000 and the `php artisan reverb:start` process crashes
  once it's pushed hard.
- **17× the HTTP ingest.** On that same $5 VM, zatat handles ~46k
  `POST /events`/sec vs Reverb's ~2.7k.
- **Self-hosted.** One static binary, one config file, no runtime
  dependencies. Run under systemd, in a container, or behind a reverse
  proxy.
- **Multi-tenant.** Any number of apps in one config, each with their own
  key / secret / rate limit / webhook targets / encryption key / origin
  allow-list.
- **Scales out.** Enable Redis pub/sub for fan-out, cross-node presence
  rosters, and fleet-wide `GET /channels` aggregation.
- **Observable.** Prometheus `/metrics` with bearer-token auth for
  non-loopback binds; structured `tracing` logs (JSON or pretty).

---

## Feature matrix

| Feature | Supported |
|---|---|
| Public channels | ✅ |
| Private channels (HMAC-SHA256 channel auth) | ✅ |
| Presence channels + roster + member_added/removed | ✅ |
| Cache channels (`cache-*`) with configurable TTL | ✅ |
| Private-cache (`private-cache-*`) | ✅ |
| Presence-cache (`presence-cache-*`) | ✅ |
| Private-encrypted (`private-encrypted-*`) via NaCl Secretbox | ✅ |
| Client events (`client-*`) with member-gating + rate limits | ✅ |
| User authentication (`pusher:signin`) | ✅ |
| Server-to-user events (`POST /apps/:id/users/:uid/events`) | ✅ |
| Watchlist events (`pusher_internal:watchlist_events`) | ✅ |
| Subscription count (`pusher_internal:subscription_count`), opt-in | ✅ |
| Cache-miss frame (`pusher:cache_miss`) | ✅ |
| HTTP API: `events`, `batch_events`, `channels`, `channel`, `channel_users`, `users/:id/events`, `terminate_connections` | ✅ |
| `info` echo on both `POST /events` and `POST /batch_events` | ✅ |
| `info=cache` field on single-channel stats | ✅ |
| Channel name cap 164 bytes, event name cap 200 bytes (Pusher spec) | ✅ |
| Webhooks — 7 event types, HMAC-signed, backoff retry | ✅ |
| Periodic ping-inactive + prune-stale with 4201 close | ✅ |
| Graceful shutdown (`SIGTERM`/`SIGINT`) + restart-signal file | ✅ |
| Origin allow-list with glob patterns | ✅ |
| Per-app rate limiting (sliding window, optional connection terminate) | ✅ |
| `server.path` URL prefix (routes + signature strip) | ✅ |
| Protocol version check at upgrade (4007 on unsupported) | ✅ |
| Watchlist size cap 100 users (4302 over limit) | ✅ |
| Redis pub/sub horizontal scaling | ✅ |
| Cross-node presence with snapshot heartbeat + orphan GC | ✅ |
| Cross-node `GET /channels` aggregation (merges stats from every node) | ✅ |
| Constant-time HMAC comparison everywhere | ✅ |
| TLS (rustls) | ✅ |
| Prometheus metrics with bearer-token auth | ✅ |

---

## Install

### From source

```sh
git clone https://github.com/Dokan-E-Commerce/zatat.git zatat
cd zatat
cargo build --release
cp zatat.toml.example zatat.toml
./target/release/zatat start --config zatat.toml
```

zatat listens on `0.0.0.0:8080` by default. Check
`http://127.0.0.1:8080/health` — it returns `ok`.

### Docker

```sh
docker build -t zatat .
docker run -p 8080:8080 -v $(pwd)/zatat.toml:/etc/zatat/zatat.toml zatat
```

Image is based on `gcr.io/distroless/static-debian12:nonroot` — static
binary, no shell, runs as UID 65532.

### Static musl binary

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

---

## Configuration

One TOML file. Minimal:

```toml
[server]
host = "0.0.0.0"
port = 8080

[[apps]]
id     = "app-1"
key    = "app-key-1"
secret = "app-secret-1"
allowed_origins = ["*"]
```

Every field can be overridden at runtime with environment variables using
the `ZATAT_` prefix and `__` as the nesting separator:

```sh
ZATAT_SERVER__PORT=9090
ZATAT_SERVER__SCALING__ENABLED=true
ZATAT_APPS__0__SECRET=hunter2
```

Full schema with every option in `zatat.toml.example`.

### Per-app options

```toml
[[apps]]
id     = "app-1"
key    = "app-key-1"
secret = "app-secret-1"

# Keep-alive
ping_interval      = 30       # seconds between pings
activity_timeout   = 30       # idle before we ping / prune

# Size limits
max_message_size   = 10_000   # reject with 4200 if a WS frame exceeds this
max_connections    = 10_000   # reject with 4004 when reached

# Origin allow-list — globs are supported
allowed_origins    = ["app.example.com", "*.example.com", "localhost"]

# Who can send client-* events: "all" | "members" | "none"
accept_client_events_from = "members"

# Opt-in Pusher-parity features
emit_subscription_count = true                             # fires pusher_internal:subscription_count on sub/unsub
encryption_master_key   = "base64-32-bytes"                # enables server-side encryption for private-encrypted-*
cache_ttl_seconds       = 1800                             # 0 = never expire

# Rate limiter: sliding window per connection
[apps.rate_limiting]
enabled            = true
max_attempts       = 60       # frames per window
decay_seconds      = 60       # window length
terminate_on_limit = false    # drop the socket on overflow vs. just throttle

# Per-app webhook targets
[[apps.webhooks]]
url         = "https://backend.example.com/hooks"
event_types = ["channel_occupied", "member_added", "client_event"]
filter_by_prefix = "presence-"
```

### Server-level options

```toml
[server]
host = "0.0.0.0"
port = 8080
path = "/realtime"          # nest all routes under this prefix (optional)
hostname = "realtime.example.com"
max_request_size = 10_000
restart_signal_file = "/tmp/zatat.restart"
restart_poll_interval_seconds = 5

[server.tls]                # optional, native rustls
cert = "/etc/zatat/cert.pem"
key  = "/etc/zatat/key.pem"

[server.scaling]            # optional, Redis pub/sub
enabled = true
channel = "zatat"

[server.scaling.redis]
host = "127.0.0.1"
port = 6379
db = 0
# url = "redis://user:pass@host:6379/0"    # alternative to host/port
# username = "..."
# password = "..."
timeout_seconds = 60

[server.prometheus]         # optional, dedicated listener
listen       = "127.0.0.1:9090"
# bearer_token = "..."      # required if `listen` is non-loopback
```

---

## Channels

| Prefix | Type | Auth | Behaviour |
|---|---|---|---|
| `my-channel` | Public | none | Anyone who knows the name can subscribe |
| `private-*` | Private | HMAC | Backend signs an auth token per socket+channel |
| `presence-*` | Presence | HMAC | Private + a live roster of `{id, info}` members |
| `cache-*` | Cache | none | Late subscribers replay the last payload |
| `private-cache-*` | Private cache | HMAC | Private + cache |
| `presence-cache-*` | Presence cache | HMAC | Presence + cache |
| `private-encrypted-*` | Private + E2E crypto | HMAC | NaCl Secretbox, compatible with the Pusher SDKs |

Auth for private / presence / private-encrypted channels goes through your
backend's standard `/broadcasting/auth` (Laravel) or `/pusher/auth` (other
frameworks) endpoint. zatat verifies the HMAC on the WS before completing
the subscription.

Channel-name cap: 164 bytes. Event-name cap: 200 bytes. Oversized get
`pusher:error 4200`.

---

## Client events (whispers)

Any client on a private or presence channel can send a frame with an event
name starting with `client-`. zatat re-broadcasts it to every other
subscriber of that channel (excluding the sender).

Gated by `accept_client_events_from` per app:

- `"all"` — any signed-in socket may send
- `"members"` — only subscribers of the target channel may send (default)
- `"none"` — no whispers allowed; rejected with `pusher:error 4301`

Additional guard: the rate limiter tracks total inbound frames per
connection, so a flood of client-events will hit 4301 before it can DoS
the server.

---

## User authentication & server-to-user events

Clients authenticate themselves to zatat by sending a `pusher:signin`
frame with an HMAC signed over `"{socket_id}::user::{user_data}"`. After
zatat replies with `pusher:signin_success`, the backend can push events
directly to that user across every tab they have open:

```
POST /apps/:id/users/:user_id/events
body: { "name": "system-notice", "data": "..." }
```

The frame lands on the pseudo-channel `#server-to-user-<user_id>` on each
signed-in socket.

`POST /apps/:id/users/:user_id/terminate_connections` (or
`DELETE /apps/:id/users/:user_id`) closes every WS for that user with a
4009.

---

## Watchlist events

On signin, a client can include a `watchlist: [user_ids]` array (max 100
— over the cap yields a 4302 close). zatat then sends the client a
`pusher_internal:watchlist_events` frame whenever any of those users
comes online or goes offline, following the standard Pusher shape:

```json
{
  "event": "pusher_internal:watchlist_events",
  "data": { "events": [{ "name": "online", "user_ids": ["alice"] }] }
}
```

Initial snapshot on signin, plus live deltas. Works across nodes when
scaling is on.

---

## HTTP API

All endpoints use HMAC-SHA256 request signing, verified in constant time.

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/apps/:id/events` | Publish one event (supports `info` echo) |
| `POST` | `/apps/:id/batch_events` | Publish up to 10 at once (per-event `info` echo) |
| `GET` | `/apps/:id/channels` | List active channels (fleet-wide when scaling) |
| `GET` | `/apps/:id/channels/:channel` | Inspect one channel (`info=occupied,subscription_count,user_count,cache`) |
| `GET` | `/apps/:id/channels/:channel/users` | Members of a presence channel |
| `POST` | `/apps/:id/users/:user_id/events` | Fan out to every socket signed in as this user |
| `POST` | `/apps/:id/users/:user_id/terminate_connections` | Kick a user (also available as `DELETE /apps/:id/users/:user_id`) |
| `GET` | `/health` | Liveness probe — returns `ok` |

Signature format: `HMAC-SHA256("{METHOD}\n{PATH}\n{sorted_params}", secret)`
with `body_md5 = md5(body)` added when the body is non-empty, excluding
`auth_signature`, `body_md5`, `appId`, `appKey`, and `channelName` from
the sorted set. `server.path` is stripped from `PATH` before signing.

---

## Webhooks

Per-app webhook targets. Each delivery carries `X-Pusher-Key` and
`X-Pusher-Signature` (HMAC-SHA256 of the body) — verify exactly the same
way as Pusher's hosted webhooks.

Event types emitted:

| Event | When |
|---|---|
| `channel_occupied` | First subscriber joins |
| `channel_vacated` | Last subscriber leaves |
| `member_added` | New user joins a presence channel |
| `member_removed` | Last socket for a user leaves a presence channel |
| `client_event` | A `client-*` event was relayed |
| `cache_miss` | A subscriber hit an empty cache channel |
| `subscription_count` | Subscription count on a channel changed |

Delivery: async worker with 4-attempt exponential backoff, 10 s per
request; filter per target with `event_types` + optional
`filter_by_prefix`.

---

## Private-encrypted channels

Two modes:

1. **Passthrough** (default) — the Pusher client library encrypts before
   publishing; zatat just forwards the already-`{nonce, ciphertext}`
   payload.
2. **Server-side encryption** — set `encryption_master_key` per app (32
   random bytes, base64 encoded) and zatat encrypts outbound payloads
   with a per-channel key derived as
   `SHA256(channel_name || master_key_bytes)`. Matches the
   `pusher-http-node` derivation so `tweetnacl.secretbox.open` on the
   client decrypts the wire value directly.

Double-encryption is prevented — zatat checks `looks_encrypted(data)`
before wrapping, so if the backend already sent a `{nonce, ciphertext}`
object it's passed through untouched.

---

## Scaling with Redis

```toml
[server.scaling]
enabled = true
channel = "zatat"

[server.scaling.redis]
host = "127.0.0.1"
port = 6379
db   = 0
```

Every node subscribes to the same Redis channel and rebroadcasts
incoming payloads to its local subscribers. All five cross-node behaviours
work:

- **Event fan-out** — `POST /events` on node A reaches subscribers on
  node B in ~1–2 ms.
- **Cross-node presence roster** — presence-snapshot heartbeat (5 s) +
  peer cache (15 s TTL). When a node SIGKILLs, its members are GC'd
  from peers and `member_removed` fires within ~5–15 s.
- **Cross-node `GET /channels`** — originator publishes a `MetricsRequest`
  on the bus, peers respond with their local channel lists, originator
  merges. 750 ms collection window, originator's own echo filtered out.
- **Cross-node user events** — `POST /users/:id/events` reaches every
  socket of that user regardless of which node they're on.
- **Auto-resubscribe** — fred's `manage_subscriptions` re-issues
  SUBSCRIBE after any reconnect, so a Redis restart doesn't silently
  break cross-node delivery.

Wire format: JSON with `"v": 2`. **Do not mix zatat and Reverb on the
same Redis channel** — Reverb serializes Application via PHP
`serialize()`, which isn't portable.

---

## TLS

zatat can terminate TLS itself with rustls, or sit behind a reverse proxy.
Termination at the proxy is usually simpler.

```toml
[server.tls]
cert = "/etc/zatat/cert.pem"
key  = "/etc/zatat/key.pem"
```

Client-side, flip `forceTLS: true` / `useTLS: true` / `scheme: "https"`
according to your SDK.

---

## Metrics and logs

### Prometheus

```toml
[server.prometheus]
listen       = "127.0.0.1:9090"
# bearer_token = "long-random-hex"  # required if listen is non-loopback
```

When `listen` is non-loopback and no bearer token is set, zatat logs a
`WARN` at startup and returns 401 to every scrape.

Series exported:

- `zatat_connections_total` (counter)
- `zatat_connections_closed_total` (counter)
- `zatat_connections` (gauge, current)
- `zatat_messages_sent_total` (counter)
- `zatat_messages_received_total` (counter)
- `zatat_channels_total` (gauge)
- `zatat_rate_limited_total` (counter)
- `zatat_redis_reconnects_total` (counter)

All labeled with `app` so you can break down by tenant.

### Logs

Structured via `tracing`. JSON by default; pretty with `--debug`.
`RUST_LOG=zatat=debug,tower_http=info` controls verbosity.

---

## CLI

```
zatat start   [--config PATH] [--debug]
zatat restart [--config PATH]     # touches server.restart_signal_file
zatat ping    [--config PATH]     # hits /health on the configured host:port
```

`zatat restart` is the graceful-shutdown mechanism: the running server
polls a sentinel file and drops cleanly when it sees a newer mtime. Have
systemd / a container runtime bring it back up.

---

## Docker

```sh
docker build -t zatat .
docker run --rm -p 8080:8080 \
  -v $(pwd)/zatat.toml:/etc/zatat/zatat.toml \
  zatat
```

Docker Compose for a two-node Redis-scaled setup:

```yaml
services:
  redis:
    image: redis:7-alpine
    restart: unless-stopped
  zatat-a:
    build: .
    ports: ["8080:8080"]
    environment:
      ZATAT_SERVER__SCALING__ENABLED: "true"
      ZATAT_SERVER__SCALING__REDIS__HOST: "redis"
    volumes: [./zatat.toml:/etc/zatat/zatat.toml]
  zatat-b:
    build: .
    ports: ["8081:8080"]
    environment:
      ZATAT_SERVER__SCALING__ENABLED: "true"
      ZATAT_SERVER__SCALING__REDIS__HOST: "redis"
    volumes: [./zatat.toml:/etc/zatat/zatat.toml]
```

---

## Backends

zatat is wire-compatible with the official Pusher server libraries — no
drop-in adapter required, just change the host / port.

### Node.js

```js
const Pusher = require("pusher");
const pusher = new Pusher({
  appId: "app-1", key: "app-key-1", secret: "app-secret-1",
  host: "127.0.0.1", port: "8080", useTLS: false,
});
await pusher.trigger("chat-room", "message", { text: "hello" });
```

### PHP

```php
$pusher = new Pusher\Pusher(
  'app-key-1', 'app-secret-1', 'app-1',
  ['host' => '127.0.0.1', 'port' => 8080, 'scheme' => 'http'],
);
$pusher->trigger('chat-room', 'message', ['text' => 'hello']);
```

### Python

```python
import pusher
client = pusher.Pusher(
    app_id="app-1", key="app-key-1", secret="app-secret-1",
    host="127.0.0.1", port=8080, ssl=False,
)
client.trigger("chat-room", "message", {"text": "hello"})
```

### Ruby

```ruby
Pusher.app_id = "app-1"
Pusher.key    = "app-key-1"
Pusher.secret = "app-secret-1"
Pusher.host   = "127.0.0.1"
Pusher.port   = 8080
Pusher.encrypted = false
Pusher.trigger("chat-room", "message", { text: "hello" })
```

### Go

```go
client := pusher.Client{
    AppID: "app-1", Key: "app-key-1", Secret: "app-secret-1",
    Host: "127.0.0.1:8080", Secure: false,
}
client.Trigger("chat-room", "message", map[string]string{"text": "hello"})
```

### .NET / C#

Use `PusherServer` from NuGet with the `Host` and `Encrypted` fields on
the options struct.

### Laravel

Use the stock `pusher` broadcasting driver — no package needed:

```env
BROADCAST_CONNECTION=pusher
PUSHER_APP_ID=app-1
PUSHER_APP_KEY=app-key-1
PUSHER_APP_SECRET=app-secret-1
PUSHER_HOST=127.0.0.1
PUSHER_PORT=8080
PUSHER_SCHEME=http
PUSHER_APP_CLUSTER=mt1

VITE_PUSHER_APP_KEY="${PUSHER_APP_KEY}"
VITE_PUSHER_HOST="${PUSHER_HOST}"
VITE_PUSHER_PORT="${PUSHER_PORT}"
VITE_PUSHER_SCHEME="${PUSHER_SCHEME}"
VITE_PUSHER_APP_CLUSTER="${PUSHER_APP_CLUSTER}"
```

Use `broadcast(new MyEvent(...))` and `Echo.channel(...)` exactly as you
would with Pusher or Reverb.

### Raw HTTP

```sh
curl -X POST "http://127.0.0.1:8080/apps/app-1/events?auth_key=...&auth_timestamp=...&auth_version=1.0&body_md5=...&auth_signature=..." \
     -H 'Content-Type: application/json' \
     -d '{"name":"message","channel":"chat-room","data":"{\"text\":\"hello\"}"}'
```

---

## Frontends

### Browser (pusher-js)

```js
import Pusher from "pusher-js";

const pusher = new Pusher("app-key-1", {
  wsHost: "127.0.0.1", wsPort: 8080,
  forceTLS: false, enabledTransports: ["ws"],
  disableStats: true, cluster: "mt1",
});
pusher.subscribe("chat-room").bind("message", console.log);
```

For `private-encrypted-*` channels pass a NaCl implementation:

```js
import nacl from "tweetnacl";
const pusher = new Pusher(KEY, { /* …, */ nacl });
```

### Laravel Echo

```js
import Echo from "laravel-echo";
import Pusher from "pusher-js";
window.Pusher = Pusher;
window.Echo = new Echo({
  broadcaster: "pusher",
  key:         import.meta.env.VITE_PUSHER_APP_KEY,
  wsHost:      import.meta.env.VITE_PUSHER_HOST,
  wsPort:      import.meta.env.VITE_PUSHER_PORT,
  forceTLS:    false, disableStats: true,
  cluster:     import.meta.env.VITE_PUSHER_APP_CLUSTER,
});
```

### iOS / Android / Flutter

- [`pusher-websocket-swift`](https://github.com/pusher/pusher-websocket-swift)
- [`pusher-websocket-java`](https://github.com/pusher/pusher-websocket-java)
- [`pusher_channels_flutter`](https://pub.dev/packages/pusher_channels_flutter)

Each accepts `host` / `wsPort` (or equivalent) options to point at zatat.

---

## Migrating from Pusher or Reverb

### From hosted Pusher

1. Pick your `app_id`, `key`, `secret`; put them in `zatat.toml`.
2. Change `host`, `port`, and `scheme` (or `useTLS` / `forceTLS`) on your
   client and server libraries.
3. Leave the rest of your code alone.

### From Laravel Reverb

1. Stop the Reverb daemon.
2. Point Laravel at zatat with `BROADCAST_CONNECTION=pusher` and the
   `PUSHER_*` env vars shown above. Laravel's own `reverb` driver
   delegates to the `pusher` driver anyway.
3. If you were running Reverb multi-node on Redis, drain and restart —
   zatat uses a different Redis wire format, so don't mix the two on the
   same Redis channel.

---

## Performance

Measured on a **Hetzner CX23** — shared 2 vCPU (Intel/AMD) / 4 GB RAM /
Ubuntu 24.04.3 LTS with `ulimit -n 200000` and default sysctl tuning.
Same box, same config, back-to-back runs, matching bench credentials.

| Scenario | zatat | Reverb | ratio |
|---|---:|---:|---:|
| `POST /events` throughput (wrk, 2t × 64c, 20 s) | **46,228 req/s** | 2,710 req/s | **~17×** |
| Avg latency, same | 1.35 ms | 23.5 ms | ~17× |
| WebSocket idle ramp, target 10,000 | **10,000 / 10,000** | 1,016 / 10,000 | **~10× capacity** |
| Connections/second during ramp | 1,720 | 173 | ~10× |
| Broadcast fan-out p50 / p95 / p99 (1,000 subs, single publish) | 71 / 83 / 84 ms | 68 / 83 / 84 ms | within noise |

Three observations worth calling out:

- **Fan-out latency is network-bound, so both servers perform equally
  well at 1,000 subscribers.** The difference is how many subscribers
  you can actually *keep* connected at once on this hardware before
  something gives.
- **Reverb can't survive the benchmark.** After the WS ramp scenario —
  during which Reverb rejected 9 of every 10 connection attempts — the
  `php artisan reverb:start` process exited on its own. We had to
  restart it to re-run the fan-out scenario. zatat kept serving HTTP
  and WS throughout all three scenarios with no restart.
- **Reverb's per-process ceiling on a 2 vCPU box is ~1,000 concurrent
  WebSocket connections.** zatat handles 10,000 with zero failures on
  the same hardware — a 10× effective capacity multiplier.

On tuned Linux with higher file-descriptor limits and sysctl adjustments
(`fs.file-max`, `net.core.somaxconn`, `net.ipv4.ip_local_port_range`,
`LimitNOFILE`) the zatat design targets 250k+ concurrent connections per
node. Verified end-to-end on CX23 up to 10k; the higher numbers are
design-grounded but awaiting a bigger-box benchmark. Reproduction steps
live in `bench/README.md`.

---

## Protocol compliance notes

- Cross-referenced against the official [Pusher protocol spec][pusher-protocol]
  and live-diffed against Laravel Reverb. Every frame type matches
  byte-for-byte when the two servers run with matching config.
- Error codes emitted: **4001** (app doesn't exist), **4004** (over
  quota), **4009** (unauthorized / bad origin), **4200** (invalid
  message format), **4201** (stale prune), **4301** (rate limit /
  client-events disabled / not a member), **4302** (watchlist > 100).
- Deliberately **not** emitted: **4003** (app disabled — no disabled
  state in self-hosted), **4100** (over capacity — zatat uses per-app
  `max_connections` → 4004 instead), **4202** (24-hour forced close —
  the stale pruner handles dead connections continuously).
- Guarded by explicit regression tests: the whisper bypass on private
  channels (Reverb #272), signature with custom `server.path` (Reverb
  #356), timing-safe signature comparison (Reverb PR #376), cross-node
  presence with orphan GC (Reverb #273 / PR #378), the MetricsHandler
  memory-leak pattern (Reverb #357).
- Constant-time HMAC everywhere — `subtle::ConstantTimeEq` on channel
  auth, user auth, HTTP signing, and webhook signatures.

---

## Production-readiness

Honest take, because "production ready" depends on your scale:

**Internal tools, < 10k concurrent, one or two nodes**: yes. Coverage is
stronger than most comparable OSS and every known Reverb bug class is
guarded by an explicit test.

**Customer-facing, 10k–100k concurrent, multi-node, revenue-adjacent**:
most of the way there. Before shipping, run the hardening churn scripts
in a loop for 24 h on a real Linux box watching RSS, actually build and
run the Docker image in staging, canary behind a small LB weight first.

**SLA-bound / payment-critical**: not yet. The remaining gaps are
**external security audit**, **fuzz testing** of protocol parsers
(`cargo-fuzz` on `parse_inbound`, `verify_http`, `decrypt_payload`), a
**multi-week soak in a staging environment under replayed production
traffic**, **verified 250 k-connection test on tuned Linux** (the design
target, currently unrun), and a **CI pipeline** that runs the whole test
matrix on every push.

---

## Development

```sh
cargo build --workspace
cargo test --workspace                                      # unit + proptest, ~55 tests
cargo clippy --workspace --all-targets -- -D warnings       # lint
cargo run --bin zatat -- start --config zatat.toml.example --debug
```

The hardening E2E suite (23 scenarios, ~100 individual checks) lives
under `tests/hardening/`:

```sh
redis-server --port 16379 --save "" --daemonize yes --dir /tmp/chaos-redis
cd tests/hardening
ulimit -n 10000
node run-all.mjs                  # runs every scenario
SKIP_LONG=0 node run-all.mjs      # also runs the ~130 s idle-prune cycle
```

The repository is a Cargo workspace, one crate per concern:

| Crate | What it owns |
|---|---|
| `zatat-core` | `Application`, `SocketId`, `ChannelName`, `PusherError`, length caps |
| `zatat-config` | TOML + env loader |
| `zatat-protocol` | Wire envelope, HMAC signing (channel + user + HTTP), presence data, private-encrypted crypto |
| `zatat-connection` | Per-connection state machine, rate limiter |
| `zatat-channels` | Channel manager, presence refcounts, cache replay, user index, watchers |
| `zatat-webhooks` | Outbound webhook dispatcher with backoff |
| `zatat-scaling` | `PubSubProvider` trait + Redis impl, `EventDispatcher`, `PresenceCache`, cross-node metrics |
| `zatat-metrics` | Prometheus registry + bearer auth |
| `zatat-http` | REST API + signature verification |
| `zatat-ws` | WebSocket server (axum), periodic tasks (ping/prune, presence heartbeat, presence GC, restart watcher) |
| `zatat-cli` | The `zatat` binary |

Pull requests and issues welcome.

---

## License

MIT. See [LICENSE](LICENSE).
