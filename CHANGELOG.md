# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

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
