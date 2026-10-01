# Security Policy

AgentRaaS sits between AI agents and real-world services — it stores encrypted
credentials (Stripe, Twilio, and similar) on behalf of the people running it.
We take security reports seriously and would rather hear about a problem
privately, with time to fix it, than have it show up as a public GitHub issue.

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, email **support@agentraas.io** with:
- A description of the vulnerability and its potential impact
- Steps to reproduce it (a proof-of-concept, if you have one)
- Which version/commit you tested against

We'll acknowledge your report within 5 business days and aim to have a fix
or mitigation plan within 30 days for confirmed issues, sooner for anything
critical (auth bypass, credential exposure, remote code execution).

## Scope

In scope:
- The AgentRaaS API gateway and dashboard (this repository)
- Authentication, session handling, and the encrypted credential storage
- The dedup/exactly-once execution logic
- SSRF or injection issues in Custom Actions or curated service routing

Out of scope:
- Vulnerabilities in third-party services AgentRaaS forwards requests to
  (Stripe, Twilio, etc.) — report those to the respective provider
- Issues requiring physical access to a self-hosted deployment's server
- Social engineering

## How AgentRaaS protects what it holds

This section describes the mechanisms as they are in the code today,
including the known gaps. File references are to `src/api-gateway-rs/`.

### Outbound requests (SSRF)

Every URL AgentRaaS will call on someone's behalf (custom actions, MCP
servers, notification webhooks, inbound-webhook destinations) goes through
`validate_target_url` (`crates/api/src/util.rs`):

- only `http` and `https`;
- internal hostnames (`localhost`, `*.local`, the compose service names)
  rejected by name;
- the hostname is resolved and **every** returned address is checked against
  private, loopback, link-local, CGNAT, multicast and cloud-metadata ranges,
  including IPv4 addresses embedded in IPv6 (`::ffff:a.b.c.d` and the
  transition forms).

The check runs when a URL is registered **and again on every forward**
(`crates/api/src/agent/forward.rs`), so a DNS record changed after
registration is caught on the next call. The shared HTTP client never
follows redirects, so a destination cannot 302 the gateway to an internal
address.

Known gap: the forward-time check and the HTTP client resolve the name
separately, so a DNS server answering with a TTL of zero could return a
public address to the check and a private one to the client (DNS rebinding).
Closing it means pinning the checked address into the connection; until
then, run self-hosted deployments with egress rules that block private
ranges if the gateway shares a network with sensitive services.

### Secrets at rest

- Third-party credentials, SSO client secrets, webhook targets and secrets,
  custom-action secret headers and dead-letter payloads are encrypted with
  AES-256-GCM (`crates/core/src/crypto.rs`): a 32-byte key from
  `CREDENTIALS_ENCRYPTION_KEY`, a random 96-bit IV per value, stored as
  `iv:tag:ciphertext`. The authentication tag makes tampering detectable.
  The server refuses to start with a missing or malformed key.
- Agent API keys and one-time email/reset tokens are stored only as SHA-256
  hashes; a raw API key is shown once at creation. Passwords are bcrypt.
  Dashboard sessions are signed JWTs and are not stored server-side.
- Audit-log rows keep a masked API key and, outside Enterprise redaction
  mode, a size-limited payload preview. The OpenTelemetry export never
  includes the payload.
- Key rotation is manual today: decrypt with the old key and re-encrypt
  with the new one. There is no envelope encryption or KMS integration.

### Duplicate suppression under concurrency (Redis)

- A call claims its slot with a single `SET dedup:<hash> <value> EX <ttl> NX`
  (`crates/core/src/dedup.rs`). Redis executes it atomically, so of any
  number of simultaneous identical calls exactly one claims the slot; the
  rest get the stored result or a 409 while it is pending.
- The hash includes the API key, service, action and payload (or the
  caller's idempotency key, and the end-user id when given). One key's
  slots cannot be read or filled by another key, so one tenant cannot
  poison another tenant's cached responses.
- A pending slot carries a lease. If the process holding it crashes
  mid-call, the next copy after the lease expires marks it "outcome
  unknown" rather than running the call again.
- A forward that times out after sending keeps its slot ("outcome
  unknown"): it may have executed, so it is never retried automatically.
  Every forward carries `Idempotency-Key: agentraas-<hash>` so APIs that
  support it (Stripe and others) can deduplicate on their side too.
- Redis is the source of truth for dedup slots. Losing Redis data (flush,
  failover without persistence) reopens slots for calls inside their TTL.
  Run Redis with persistence (AOF) and never share it with untrusted
  clients: anyone who can write to it can mark calls as already done.

### Network exposure

The compose files bind Postgres, Redis and MinIO to `127.0.0.1` only. The
client IP for rate limiting comes only from the header named in
`CLIENT_IP_HEADER` (set it to the header your proxy writes, e.g.
`cf-connecting-ip`); `X-Forwarded-For` is never trusted. `/metrics` is off
unless `METRICS_TOKEN` is set and then requires it as a bearer token.

## Disclosure

We ask for a reasonable window to fix a confirmed vulnerability before any
public disclosure. We're happy to credit reporters (by name or handle) in
the fix's release notes, if you'd like that.

## Supported versions

AgentRaaS is under active development; only the latest commit on `main` is
supported with security fixes at this stage.
