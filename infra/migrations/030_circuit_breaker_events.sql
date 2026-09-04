-- 030_circuit_breaker_events.sql
-- Persists circuit breaker state transitions (closed->open, open->half-open,
-- half-open->open, half-open->closed) so uptime can be reported historically
-- instead of only reflecting whatever the current Redis key happens to say
-- right now. Redis circuit:<service> keys have a 3600s TTL and get
-- overwritten on every transition — there was previously no record of how
-- often or how long a service had actually been down. Powers
-- /api/v1/reliability-report (src/core/dashboard).
--
-- Circuit state is shared infrastructure (one circuit per service, across
-- every org calling it — see the scoping note on notifyCircuitOpen in
-- server.js), so these events are service-level, not org-scoped, same as
-- the circuit:<service> Redis keys they mirror.
CREATE TABLE IF NOT EXISTS circuit_breaker_events (
  id           SERIAL PRIMARY KEY,
  service      VARCHAR(100) NOT NULL,
  from_state   VARCHAR(20) NOT NULL,
  to_state     VARCHAR(20) NOT NULL,
  occurred_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_circuit_events_service_time ON circuit_breaker_events(service, occurred_at DESC);
