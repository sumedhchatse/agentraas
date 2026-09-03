# src/core — MIT Core Engine

Phase 3 of `RESTRUCTURE_PLAN.md`: proxy/mcp/dashboard logic has been
migrated out of `src/api-gateway/server.js` into the three modules below,
now that all four `ee/` modules are built and proven against it. Each
module is a dependency-injected factory (`createProxy(deps)`,
`createMcp(deps)`, `registerDashboardRoutes(fastify, deps)`) — logic moved
verbatim from server.js, only how free variables are bound changed
(closure over server.js's module scope -> explicit `deps`). `server.js`
still owns shared cross-cutting helpers (auth, credentials, usage limits,
audit logging) and constructs/wires these three modules together.

## The split

- **`proxy/`** — payload hashing, Redis dedup, circuit breaker, the
  unified forwarder (`handleRequest`, `forwardAction`,
  `checkAgentRateLimit`). `getCircuitStatesBatch` is also consumed
  directly by `dashboard/`'s `/api/v1/services` endpoint.
- **`mcp/`** — MCP JSON-RPC handling (`handleMCP`, the `/mcp` route),
  reusing `proxy/`'s dedup/circuit-breaker/forwarder rather than
  duplicating them.
- **`dashboard/`** — the dashboard's stats/usage/admin API endpoints
  (`/api/v1/stats`, `/dashboard/*`, `/usage`, `/admin/*`, `/services`,
  `/export/csv`). The static frontend (`public/index.html`) and
  everything else — auth, credentials, custom actions, billing, inbound
  webhooks — stays in `server.js` for now; not part of this split.

## Verifying a change here

Restart `ar-api` (`podman-compose up -d --force-recreate ar-api` after
editing `compose.yaml`'s mounts, or just let nodemon pick up a file
change) and run `podman exec -it ar-api npm test` — `test/dedup.test.js`
exercises `proxy/` directly (exactly-once semantics), and the other
suites cover auth/credentials/SSO end to end through the same routes.
