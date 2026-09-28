# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.2.5] - 2026-09-28

### Security

- WebSocket messages and frames are capped at the transport
  (`max(4 × max_message_size, 64 KiB)`) instead of the 64 MiB library
  default, and every inbound frame — pings and binary included — counts
  against the rate limiter.
- Each connection may hold at most `max_channels_per_connection`
  subscriptions (default 100).
- Every socket write has a 10 s deadline, so a client that stops reading
  can no longer pin its connection task and slot forever.
- On an apps reload, removing an app or changing its key closes its
  connections (4001), and changing its allowed origins closes connections
  from origins no longer allowed (4009). Other changes, such as a rotated
  secret, apply to existing connections from their next subscribe or
  sign-in instead of leaving them on the old settings.
- A socket can no longer sign in as a second user; the old user index
  kept routing that user's events to it.
- The HTTP API rejects a mismatching `auth_key`, an `auth_version` other
  than `1.0`, and a declared `body_md5` that does not match the body.
  Parameter names are lowercased before signing, as Pusher specifies.
- An extreme `auth_timestamp` could wrap the age check in release builds
  and never expire; the comparison no longer overflows.
- `rediss://` URLs now actually use TLS; previously TLS support was not
  compiled in and the connection silently fell back to plaintext.

### Fixed

- A cache-channel subscriber with a cached event never received
  `pusher_internal:subscription_succeeded`; it now gets the confirmation
  first, then the cached event.
- `member_added` was sent from a deferred task, so a join immediately
  followed by a disconnect could reach peers as removed-then-added and
  leave a ghost member. Presence transitions and their events are now
  applied under one per-channel lock, locally and for peer updates.
- Presence member caps are enforced atomically per node and count the
  users other nodes have reported (not a strict fleet-wide quota while
  Redis updates are in flight).
- Membership changes and everything they trigger — local frames,
  cross-node messages, subscription counts and webhooks — are issued in
  one per-channel critical section, so a user's leave racing their
  reconnect can no longer reach peers as added-then-removed.
- Cross-node messages carry a per-node sequence number. A periodic
  snapshot captured before a live join/leave (but published after it) no
  longer undoes that update on peers, and a snapshot that does change the
  fleet roster — e.g. after a lost live message — now sends local clients
  the correcting `member_added` / `member_removed` and watchlist
  online/offline events (diffed against what clients were told, so a
  peer entry past its TTL but not yet reaped is still announced as
  removed; subscribers of a channel a snapshot omits get a corrected
  `subscription_count`). Channel-count and user-session snapshots are
  always sent, even when empty, so peers drop state the sender no longer
  has.
- A user's sign-in and last-socket disconnect — and the online/offline
  events they publish — are serialized per user, so a quick reconnect can
  no longer reach peers as online-then-offline.
- Peer presence and count caches drop channels no peer reports any more,
  instead of keeping an entry for every channel ever seen.
- Nodes always share per-channel subscriber counts, so
  `GET /channels/:channel` and `info` on `POST /events` are fleet-wide with
  default settings; `subscription_count` events remain opt-in.
- Presence client events carry the sender's `user_id`, and client events
  no longer overwrite a cache channel's stored event.
- An oversized watchlist no longer closes the socket: the first 100 users
  are watched, sign-in succeeds and `pusher:error` 4302 is reported.
- `zatat restart` disconnected every client but never exited; it now runs
  the full graceful shutdown.
- `ZATAT_APPS__<n>__<FIELD>` overrides failed startup; they now override
  one field of the indexed app and survive apps reloads. Text settings
  (ids, keys, secrets, hosts, passwords) from the environment are used
  verbatim instead of being parsed — a numeric app id loads, and a secret
  like `0042` is no longer read as `42`.
- A publish to a `private-encrypted-*` channel that cannot be encrypted
  returned 200 and was dropped; it is now rejected with 400 before any
  event of the request is delivered.
- `GET /channels` lists only occupied channels and returns `{}` rather
  than `[]` when empty; `info=cache` returns `{"data", "ttl"}`; a batch
  without `info` returns `{}`; `info` on `POST /events` is fleet-wide;
  `user_count` on a non-presence channel is rejected with 400.
- `private-encrypted-cache-*` channels cache their last event.
- `channel_occupied`, `channel_vacated`, `member_added` and
  `member_removed` webhooks describe fleet-wide transitions instead of
  firing once per node. When two nodes lose their last member at the same
  moment, one of them reports the transition; when a peer crashes, a
  surviving node does. `cache_miss` is sent once per empty period.
- A presence joiner's initial roster could list a peer's member who had
  just left; the roster is now read under the same lock as peer updates.
- Races fixed: channel removal vs. a new subscriber, user-index cleanup vs.
  sign-in, cache expiry vs. a fresh publish, watchlist cleanup vs. a new
  watch, and a registration leak when the welcome frame failed to send.
- The last unsubscribe no longer discards a cache channel's live payload.
- `zatat_channels_total` and `zatat_redis_reconnects_total` were never
  updated; per-app series no longer have a permanently-zero unlabeled twin.

### Changed

- At startup zatat raises its open-file soft limit to the hard limit and
  logs it (warning below 16,384). Containers often start at a soft limit
  of 1,024, which failed HTTP accepts near 1,000 WebSockets.

- `/health` (and the new `/up` alias) answers 503 while draining; new
  WebSocket upgrades are refused from the moment shutdown starts. The
  plain-HTTP drain is bounded at 30 s, and queued cross-node publishes and
  webhooks get up to 5 s to flush before exit.
- Webhooks are retried like Pusher: any non-2xx or transport error, with
  exponential backoff for up to 5 minutes. Waiting retries don't hold
  delivery slots.
- `"block"` overflow modes stage overflow in an ordered, byte-bounded
  buffer (256 MiB publisher, 64 MiB webhooks) and then drop and count;
  they no longer grow without limit.
- A metrics listener that cannot bind now fails startup.
- Events may target at most 100 channels.
- HTTP API requests (body read included) have a 10 s deadline, and a
  request repeating a query parameter is rejected.

### Added

- `GET /apps/:id/connections` (fleet-wide connection count).
- Metrics: `zatat_redis_publish_failures_total`,
  `zatat_redis_connection_errors_total`, `zatat_redis_connected`,
  `zatat_scaling_publish_timeouts_total`,
  `zatat_scaling_publish_staging_bytes`, `zatat_webhooks_failed_total`,
  `zatat_webhooks_staging_bytes`, `zatat_ws_write_timeouts_total`,
  `zatat_http_request_duration_seconds`.
- A watchdog pings the Redis subscriber every 5 s and forces a reconnect
  after two failures, so a silently dead pub/sub connection cannot cut a
  node off from the fleet.

## [0.2.4] - 2026-07-19

### Security

- The HTTP API now rejects requests whose `auth_timestamp` is more than 600
  seconds old or in the future, instead of accepting any signed request
  regardless of age.
- Publishes to private-encrypted channels fail closed (the event is dropped)
  when the master key or decryption is unavailable, rather than silently
  falling back to a plaintext broadcast.

### Fixed

- Cross-node protocol frames (subscription counts, presence updates) no
  longer overwrite the cached payload of cache channels.
- A duplicate `member_removed` event could fire when two unsubscribes for
  the same presence member raced each other; only one is now reported.
- A race in connection registration could let a node briefly exceed
  `max_connections` under concurrent connects.
- Cache channels that go empty are now garbage-collected instead of
  lingering indefinitely.
- The rate limiter no longer panics when configured with a very large decay
  window.
- The webhook queue's `Block` overflow mode now preserves delivery order
  instead of reordering under load.
- Unsubscribing from a channel the client wasn't actually subscribed to no
  longer emits a spurious `channel_vacated` webhook.
- IPv6 origins (bracketed host, with or without a port) now match the
  origin allow-list correctly.
- The CI Docker smoke test now fails the job when the container never
  becomes healthy, instead of passing silently.

### Changed

- Channel names are validated against Pusher's channel-name charset and
  length limit.
- Presence channels are capped via `max_presence_members_per_channel`
  (default 100) and `max_presence_member_size_bytes` (default 2048), both
  configurable.
- `ping_interval` now drives server-initiated pings, with a 15s maintenance
  sweep and a 30s pong grace period before a connection is considered
  stale.
- Webhook retries are limited to transient failures (429/408 and 5xx); 4xx
  client errors are no longer retried.
- `GET /channels/:channel` and `GET /users` aggregate counts and members
  fleet-wide when Redis-backed scaling is enabled, instead of only
  reflecting the local node.
- The release profile allows unwinding (no `panic = "abort"`), so
  supervised tasks can catch a panic and respawn instead of taking the
  process down.
- Removed the unused `server.hostname` config key.

### Added

- SECURITY.md with a vulnerability reporting process.
- `cargo-deny` license/advisory/source checks in CI, and Dependabot for
  `cargo` and GitHub Actions updates.
- Multi-arch (amd64/arm64) Docker images, built natively on GitHub's arm64
  runners instead of amd64 + QEMU.
