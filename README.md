# Zatat

A Pusher-compatible WebSocket server, written in Rust.

zatat implements the [Pusher Channels protocol v7][pusher-protocol] and
the Pusher HTTP API for self-hosted WebSocket messaging, and is a drop-in
replacement for Laravel Reverb. Point pusher-js / Laravel Echo at
`ws://host:8080/app/YOUR_KEY` and your backend's Pusher SDK at the same
host and port. Where zatat deliberately differs from hosted Pusher, the
difference is listed under [Known limitations](#known-limitations).

Client library versions and protocol versions are different. zatat accepts
protocol **5, 6, and 7** and does not inspect the library version string,
so pusher-js 8.x (protocol 7) connects like every other current SDK.

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
  rosters, and fleet-wide `GET /channels` / `/channels/:channel` /
  `/users` aggregation.
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
| Private-encrypted-cache (`private-encrypted-cache-*`) | ✅ |
| Client events (`client-*`) with member-gating + rate limits; `user_id` on presence channels; never cached | ✅ |
| User authentication (`pusher:signin`) | ✅ |
| Server-to-user events (`POST /apps/:id/users/:uid/events`) | ✅ |
| Watchlist events (`pusher_internal:watchlist_events`) | ✅ |
| Subscription count (`pusher_internal:subscription_count`), opt-in | ✅ |
| Cache-miss frame (`pusher:cache_miss`) | ✅ |
| HTTP API: `events`, `batch_events`, `channels`, `channel`, `channel_users`, `users/:id/events`, `terminate_connections`, `connections` | ✅ |
| `info` echo on both `POST /events` and `POST /batch_events` (fleet-wide) | ✅ |
| `info=cache` (`{"data": …, "ttl": …}`) on channel stats | ✅ |
| Channel name validated against Pusher charset, cap 164 bytes; event name cap 200 bytes | ✅ |
| Presence channel caps — max members (fleet-wide, atomic), max `channel_data` size (4301 over limit) | ✅ |
| Per-connection channel cap, transport frame limit, bounded socket writes | ✅ |
| Webhooks — 7 event types, HMAC-signed, fleet-aware occupied/vacated/member events, Pusher-style 5-minute retry | ✅ |
| Periodic ping-inactive + prune-stale with 4201 close (15 s sweep) | ✅ |
| Graceful shutdown (`SIGTERM`/`SIGINT` or restart-signal file): `/health` → 503, upgrades refused, bounded drain | ✅ |
| Origin allow-list with glob patterns | ✅ |
| Per-app rate limiting (sliding window, optional connection terminate) | ✅ |
| `server.path` URL prefix (routes + signature strip) | ✅ |
| Protocol version check at upgrade (4007 on unsupported) | ✅ |
| Watchlist size cap 100 users (first 100 kept, 4302 reported) | ✅ |
| Redis pub/sub horizontal scaling | ✅ |
| Cross-node presence with snapshot heartbeat + orphan GC | ✅ |
| Cross-node aggregation for `GET /channels`, `/channels/:channel`, `/users` | ✅ |
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
`http://127.0.0.1:8080/health` — it returns `ok` (and `503 draining`
once the server is shutting down). `/up` is an alias, as in Reverb.

### Docker

```sh
docker build -t zatat .
docker run --ulimit nofile=65536:65536 -p 8080:8080 \
  -v "$(pwd)/zatat.toml:/etc/zatat/zatat.toml:ro" zatat
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
ZATAT_APPS__0__SECRET=hunter2          # overrides one field of the first [[apps]] entry
ZATAT_APPS__1__ID=app-2                # indexes past the file's apps add new apps
```

`ZATAT_APPS__<index>__<FIELD>` overrides apply on startup *and* on every
apps reload, so a secret supplied through the environment is never
replaced by the file's value when `zatat.toml` changes. Values are read
as TOML scalars (numbers, booleans, `[...]` arrays), except text settings
such as ids, keys, secrets, hosts and passwords, which are used verbatim —
`ZATAT_APPS__0__ID=123456` or an all-digit secret works as written.

Full schema with every option in `zatat.toml.example`.

### Per-app options

```toml
[[apps]]
id     = "app-1"
key    = "app-key-1"
secret = "app-secret-1"

# Keep-alive
ping_interval      = 30       # idle seconds before the server sends pusher:ping;
                              # no pong within 30s more and the socket is closed (4201)
activity_timeout   = 30       # advertised to clients in connection_established —
                              # doesn't drive server behaviour itself

# Size limits
max_message_size   = 10_000   # emit pusher:error 4200 on an oversized frame (connection stays open);
                              # frames over max(4 × this, 64 KiB) close the socket at the transport
max_connections    = 10_000   # reject with 4004 when reached
max_channels_per_connection = 100   # further subscribes are refused (pusher:error 4301)

# Origin allow-list — globs are supported. IPv6 origins are matched with
# their brackets stripped, so a pattern of "::1" matches a browser Origin
# of "http://[::1]:8080".
allowed_origins    = ["app.example.com", "*.example.com", "localhost"]

# Who can send client-* events: "all" | "members" | "none"
# "all" is accepted but behaves like "members" — Pusher requires channel
# membership for client events regardless, and a WARN is logged at
# config load if you set "all".
accept_client_events_from = "members"

# Opt-in Pusher-parity features
emit_subscription_count = true                             # fires pusher_internal:subscription_count on sub/unsub
encryption_master_key   = "base64-32-bytes"                # enables server-side encryption for private-encrypted-*
cache_ttl_seconds       = 1800                             # 0 = never expire

# Presence channel caps
max_presence_members_per_channel = 100    # new user_id rejected once the channel has this many users
                                          # across the fleet, as known to this node (pusher:error 4301)
max_presence_member_size_bytes   = 2048   # channel_data over this size is rejected (pusher:error 4301)

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

Channel names are validated against Pusher's allowed charset —
`A-Za-z0-9_-=@,.;` — and capped at 164 bytes; event names cap at 200
bytes. A client `pusher:subscribe` for a name that fails either check is
rejected with `pusher:error 4200` before any auth runs. That includes the
internal `#server-to-user-<user_id>` namespace used for server-to-user
events — clients can't subscribe to it directly. The HTTP events API is
the one place `#server-to-user-<user_id>` is accepted as a channel name,
since that's how the server publishes user-targeted events.

---

## Client events (whispers)

Any client on a private or presence channel can send a frame with an event
name starting with `client-`. zatat re-broadcasts it to every other
subscriber of that channel (excluding the sender).

Gated by `accept_client_events_from` per app:

- `"members"` — only subscribers of the target channel may send (default)
- `"all"` — accepted for compatibility, but behaves exactly like
  `"members"`: Pusher requires channel membership for client events no
  matter what, so this setting can't actually widen who can send. A
  `WARN` is logged at config load when it's set.
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

On signin, a client can include a `watchlist: [user_ids]` array. As in
Pusher, only the first 100 entries are watched: signin still succeeds and
a `pusher:error` with code 4302 reports the truncation. zatat then sends the client a
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
| `POST` | `/apps/:id/batch_events` | Publish multiple events in one request, no per-batch cap (per-event `info` echo; `{}` without `info`) |
| `GET` | `/apps/:id/channels` | List occupied channels as an object (fleet-wide when scaling; `user_count` needs `filter_by_prefix=presence-`) |
| `GET` | `/apps/:id/channels/:channel` | Inspect one channel (`info=occupied,subscription_count,user_count,cache`; fleet-wide when scaling) |
| `GET` | `/apps/:id/channels/:channel/users` | Members of a presence channel (fleet-wide when scaling) |
| `POST` | `/apps/:id/users/:user_id/events` | Fan out to every socket signed in as this user |
| `POST` | `/apps/:id/users/:user_id/terminate_connections` | Kick a user (also available as `DELETE /apps/:id/users/:user_id`) |
| `GET` | `/apps/:id/connections` | Live connection count (fleet-wide when scaling) |
| `GET` | `/health`, `/up` | Readiness probe — `ok`, or `503` while draining |

Each event may target at most 100 channels. An event for a
`private-encrypted-*` channel whose data is plaintext is rejected with
400 unless the app has an `encryption_master_key` to encrypt it; a
rejected request publishes none of its events.

Signature format: `HMAC-SHA256("{METHOD}\n{PATH}\n{sorted_params}", secret)`
with `body_md5 = md5(body)` added when the body is non-empty, excluding
`auth_signature`, `body_md5`, `appId`, `appKey`, and `channelName` from
the sorted set, parameter names lowercased. `server.path` is stripped
from `PATH` before signing. A request whose `auth_key` names a different
app, whose `auth_version` is not `1.0`, or whose declared `body_md5` does
not match the body is rejected with 401.

`auth_timestamp` must be present and within 600 seconds of the server's
clock in either direction — missing or stale, and the request is
rejected with 401 before the signature is even checked. Every official
`pusher-http-*` SDK sets this on every request, so it only bites
hand-rolled signing code.

---

## Webhooks

Per-app webhook targets. Each delivery carries `X-Pusher-Key` and
`X-Pusher-Signature` (HMAC-SHA256 of the body) — verify exactly the same
way as Pusher's hosted webhooks.

Event types emitted:

| Event | When |
|---|---|
| `channel_occupied` | First subscriber joins, fleet-wide |
| `channel_vacated` | Last subscriber leaves, fleet-wide |
| `member_added` | New user joins a presence channel, fleet-wide |
| `member_removed` | A user's last socket leaves a presence channel, fleet-wide |
| `client_event` | A `client-*` event was relayed |
| `cache_miss` | A subscriber hit an empty cache channel (once per channel per empty period) |
| `subscription_count` | Subscription count on a channel changed |

Delivery: async worker with a 10 s timeout per request; filter per
target with `event_types` + optional `filter_by_prefix`. As with Pusher,
any non-2xx response or transport error is retried with exponential
backoff (1 s, doubling, capped at 60 s) for up to 5 minutes, then counted
in `zatat_webhooks_failed_total`. At most 256 requests are in flight; a
delivery waiting for its next retry does not hold one of those slots.

**Delivery guarantees.** The in-process enqueue queue is bounded at 64k
events to keep a stalled consumer from OOM'ing the server. Behavior when
it fills is controlled by `server.webhook_overflow_mode`:

- `"best_effort"` (default) — new events are dropped and counted via
  `zatat_webhooks_dropped_total`. Alert on the counter. Producer hot paths
  never block.
- `"block"` — events overflow into an ordered staging buffer of up to
  64 MiB before being dropped (and counted). Producers still never wait.
  This rides out longer receiver outages; it is not durable.

For durable-across-restart delivery, feed events from zatat into a real
queue (Redis Streams, Kafka) and fan out from there.

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

Fails closed, but only where server-side encryption is actually in play: a
payload that already looks encrypted (`{nonce, ciphertext}`, i.e.
passthrough mode) is forwarded regardless of whether a master key is
configured. A plaintext payload that needs server-side encryption is a
different story — if the app has no valid `encryption_master_key`
configured, or encryption fails for any reason, that event is dropped and
a `WARN` is logged. It is never sent to subscribers as plaintext.

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
incoming payloads to its local subscribers. All six cross-node behaviours
work:

- **Event fan-out** — `POST /events` on node A reaches subscribers on
  node B in ~1–2 ms.
- **Cross-node presence roster** — presence-snapshot heartbeat (5 s) +
  peer cache (15 s TTL). When a node SIGKILLs, its members are GC'd
  from peers and `member_removed` fires within ~5–15 s.
- **Cross-node `GET /channels`** — originator publishes a `MetricsRequest`
  on the bus, peers respond with their local channel lists, originator
  merges. Up to a 750 ms window, returning as soon as all known peers
  respond; originator's own echo filtered out.
- **Cross-node `GET /channels/:channel` and `GET .../users`** — these
  don't do a live roundtrip. They read the same continuously-updated
  peer caches used for presence (subscription counts and presence
  rosters, refreshed on the same heartbeat/TTL as above) and merge them
  with local state.
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
polls a sentinel file and, when it sees a newer mtime, runs the same
drain as `SIGTERM` and exits. Have systemd / a container runtime bring it
back up.

Shutdown sequence: `/health` starts answering `503` and new WebSocket
upgrades are refused, every connection is closed with 1001, in-flight
HTTP requests get up to 30 s to finish, and queued cross-node publishes
and webhooks get up to 5 s to leave the process.

---

## Docker

```sh
docker build -t zatat .
docker run --rm --ulimit nofile=65536:65536 -p 8080:8080 \
  -v "$(pwd)/zatat.toml:/etc/zatat/zatat.toml:ro" \
  zatat
```

Each WebSocket holds a file descriptor, and HTTP requests, Redis, logs
and listeners need more. Docker commonly starts containers with a soft
limit of 1,024, which fails HTTP accepts near 1,000 WebSockets. zatat
raises its soft limit to the hard limit at startup and logs the result
(`open-file limit`, with a warning below 16,384), so Docker's default hard
limit of 524,288 is used automatically. Still set the limit explicitly
where you control it: `--ulimit` for `docker run`, `ulimits` for Compose,
`LimitNOFILE` for a service that runs zatat directly (a systemd unit that
runs `docker run` does not pass its own `LimitNOFILE` to the container).
Keep the connection limits across all apps below the effective limit.

Docker Compose for a two-node Redis-scaled setup:

```yaml
services:
  redis:
    image: redis:7-alpine
    restart: unless-stopped
  zatat-a:
    build: .
    ulimits:
      nofile: {soft: 65536, hard: 65536}
    ports: ["8080:8080"]
    environment:
      ZATAT_SERVER__SCALING__ENABLED: "true"
      ZATAT_SERVER__SCALING__REDIS__HOST: "redis"
    volumes: [./zatat.toml:/etc/zatat/zatat.toml]
  zatat-b:
    build: .
    ulimits:
      nofile: {soft: 65536, hard: 65536}
    ports: ["8081:8080"]
    environment:
      ZATAT_SERVER__SCALING__ENABLED: "true"
      ZATAT_SERVER__SCALING__REDIS__HOST: "redis"
    volumes: [./zatat.toml:/etc/zatat/zatat.toml]
```

---

## Backends

The official Pusher server libraries work unchanged — point them at
zatat's host and port.

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
- Channels protocol versions **5, 6, and 7** are accepted at the WS
  handshake; anything else is closed with 4007. The client *library*
  version (the `version=` query param) is not inspected, so `pusher-js`
  8.x — 8.5.0 included — connects on protocol 7 like every other current
  SDK.
- Error codes emitted: **4001** (app doesn't exist), **4004** (over
  quota), **4009** (unauthorized / bad origin, including an origin
  removed by a reload), **4200** (invalid message format), **4201**
  (stale prune), **4301** (rate limit /
  client-events disabled / not a member / channel caps), **4302**
  (watchlist truncated to 100 — reported, not fatal).
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

Resource limits: WebSocket frames are capped at the transport, every
socket write has a 10 s deadline, every HTTP API request has a 10 s
deadline, each connection may hold at most
`max_channels_per_connection` subscriptions, cross-node and webhook
queues are bounded in items and bytes, and presence caps are enforced
per node against the fleet membership that node knows of (see Known
limitations). Set `max_connections` and `rate_limiting` per app for
public deployments.

Operations: `/health` doubles as readiness (503 while draining). Alert on
`zatat_scaling_publish_drops_total`, `zatat_scaling_publish_timeouts_total`,
`zatat_redis_publish_failures_total`, `zatat_redis_connected == 0`,
`zatat_webhooks_dropped_total`, `zatat_webhooks_failed_total` and
`zatat_ws_write_timeouts_total`.

App reloads: removing an app or changing its key closes that app's
connections (4001); changing its allowed origins closes only connections
from origins no longer allowed (4009). Every other change — including a
rotated secret or encryption key — applies to existing connections from
their next subscribe or sign-in, without disconnecting them.

### Known limitations

- Cross-node delivery uses Redis pub/sub: an event published while a
  peer is disconnected from Redis is not replayed to that peer. There is
  no durable outbox. Presence rosters, subscriber counts and online users
  do heal: every node re-announces its state every 5 s, and clients are
  sent the corrections.
- Presence caps are enforced against this node's members plus the peer
  state received over Redis. Joins on different nodes within Redis's
  propagation delay (milliseconds normally; the whole outage if Redis is
  down) can each take the last slot, so a channel can exceed
  `max_presence_members_per_channel` — at worst by one cap's worth per
  node. Members admitted that way stay until they leave; the cap only
  stops further joins. It bounds resource use and is not a strict
  fleet-wide quota.
- Likewise two nodes acting in the same moment can both send
  `channel_occupied` / `member_added`. `channel_vacated` and
  `member_removed` are reconciled between the nodes involved (and reported
  by a surviving node when a peer crashes), but can be lost if the
  cross-node message itself is dropped. Webhook receivers should order
  events by `time_ms`.
- No HTTP/SockJS fallback transports — WebSocket only (the Echo/Reverb
  default).
- Pusher's per-event 10 KB data limit and 10-event batch limit are not
  enforced; `server.max_request_size` bounds the request body instead.
- zatat does not rate-limit connection attempts per IP address or time out
  slow request headers; run it behind a load balancer or reverse proxy
  that does (API requests themselves have a 10 s deadline, body included).

## Development

```sh
cargo build --workspace
cargo test --workspace                                      # unit + proptest, ~140 tests
cargo clippy --workspace --all-targets -- -D warnings       # lint
cargo run --bin zatat -- start --config zatat.toml.example --debug
```

The hardening E2E suite (47 scenarios, hundreds of individual checks)
lives under `tests/hardening/`:

```sh
redis-server --port 16379 --save "" --daemonize yes --dir /tmp/chaos-redis
cd tests/hardening
ulimit -n 10000
node run-all.mjs                  # runs every scenario except 10-soak
SOAK=1 node run-all.mjs           # also runs 10-soak, the long soak scenario
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
