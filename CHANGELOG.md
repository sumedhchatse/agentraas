# Changelog

All notable changes to the AgentRaaS server (`src/api-gateway-rs`) are
recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
and versions follow [Semantic Versioning](https://semver.org/). Until 1.0.0,
a minor version may contain breaking changes; each one is listed under
**Breaking** with the steps to migrate.

The SDKs are versioned separately: `agentraas` on PyPI (`src/sdk`) and
`agentraas` on npm (`src/sdk-js`).

Every release that adds a file under `infra/migrations/` says so. Apply
migrations in order before starting the new version; they are written to be
safe to re-run.

## [Unreleased]

## [0.9.1] - 2026-10-01

### Security
- **MCP tool calls skipped action policies and spend caps.** A call made
  through `/mcp` was forwarded even when a deny policy, the `no_secrets` /
  `new_destination` guards or a spend cap should have stopped it. MCP now
  runs the same checks as the webhook path; a rule that would ask for human
  approval blocks the call over MCP. Upgrade if you use action policies or
  spend caps with MCP clients.
- Calls held for approval stored the agent's raw API key in
  `hitl_requests`. They now store a prefix and SHA-256 only.
  **Migration:** run `051_hash_held_call_keys.sql`.
- SSO discovery and the Slack approval callback now go through the SSRF
  guard: an org admin could otherwise point the server at an internal
  address.
- Security headers on every response (`X-Frame-Options: DENY`, `nosniff`,
  `Referrer-Policy`, HSTS in production).
- rustls 0.23.45 (RUSTSEC-2026-0285).

### Fixed
- MCP audit rows recorded every agent as `mcp-agent` instead of the
  verified agent id.
- An internal error in the circuit-breaker, usage or spend-cap check left
  the call's dedup slot claimed, so an identical retry got 409 until the
  lease expired.

## [0.9.0] - 2026-10-01

First versioned release. It covers everything shipped up to this date; the
server's `/health` response now includes its version.

### Added
- `/metrics` in Prometheus text format: `agentraas_calls_total` and the
  `agentraas_call_duration_ms` histogram, labeled by service and outcome.
  Off unless `METRICS_TOKEN` is set; scrape with it as a bearer token.
- OpenTelemetry export of every handled call as an OTLP span
  (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`).
- Action policies (allow, deny, require approval per service and action)
  and the prompt-injection guard (`no_secrets`, `new_destination`).
- Timeout-safe forwarding: every forward sends
  `Idempotency-Key: agentraas-<dedup hash>`; a call that times out after
  sending is recorded as "outcome unknown", kept in its dedup slot and never
  retried automatically. `PROXY_TIMEOUT_SECONDS` (default 30).
- Crash recovery for in-flight calls: dedup claims carry a lease.
- Spend and call caps per agent, schema drift detection, a local dev tunnel,
  dead-letter queue with replay, circuit breakers, human approval (HITL).

### Security
- Client IP for rate limiting is read only from `CLIENT_IP_HEADER`;
  `X-Forwarded-For` is no longer trusted.

### Changed
- `compose.yaml` pulls MinIO from `cgr.dev/chainguard/minio` (runs as root
  so existing data stays writable). MinIO's own registries now refuse
  anonymous pulls.

### Migrations
- Up to `050_known_destinations.sql`. On an existing install, apply any of
  `046` to `050` you have not run yet.

[Unreleased]: https://github.com/sumedhchatse/agentraas/compare/v0.9.1...HEAD
[0.9.1]: https://github.com/sumedhchatse/agentraas/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/sumedhchatse/agentraas/releases/tag/v0.9.0
