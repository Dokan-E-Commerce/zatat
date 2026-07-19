# Security

## Supported versions

The latest tagged release and `main`. Older releases don't get backported
fixes — upgrade instead.

## Reporting a vulnerability

Do not open a public issue. Report privately through
[GitHub Security Advisories](https://github.com/Dokan-E-Commerce/zatat/security/advisories/new)
for this repository.

Include a reproduction (config, request/frame sequence, or a failing
test) where possible — it's the fastest way to get a fix out.

Expect an initial response within 7 days.

## Scope

This covers the zatat server itself: the WebSocket protocol
implementation, the HTTP API, signing/auth, config loading, and the
Redis scaling path. Vulnerabilities in Pusher client SDKs (pusher-js,
Laravel Echo, pusher-http-*, etc.) belong upstream in those projects.
