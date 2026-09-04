-- 031_health_check_monitoring.sql
-- Proactive, opt-in active health checks — separate from the passive
-- circuit breaker (which only reacts to real agent traffic). An org
-- explicitly enables this per service they've already configured
-- credentials for; a background job (server.js runHealthChecks) then pings
-- that service directly with the org's own stored credentials every 5
-- minutes and records the result here.
--
-- Deliberately per-org, not fed into the shared circuit:<service> state:
-- a failing check here is usually THIS org's credential going bad (revoked
-- key, expired token), not a service-wide outage — tripping the shared
-- breaker (and blocking every other org's real traffic) off one org's
-- stale credential would be a serious blast-radius bug. See runHealthChecks
-- for the notification path, which reuses notification_webhooks.
CREATE TABLE IF NOT EXISTS health_check_settings (
  id          SERIAL PRIMARY KEY,
  org_id      VARCHAR(255) NOT NULL,
  service     VARCHAR(100) NOT NULL,
  enabled_by  INTEGER REFERENCES users(id),
  enabled_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE(org_id, service)
);

CREATE TABLE IF NOT EXISTS health_check_results (
  id           SERIAL PRIMARY KEY,
  org_id       VARCHAR(255) NOT NULL,
  service      VARCHAR(100) NOT NULL,
  ok           BOOLEAN NOT NULL,
  latency_ms   INTEGER,
  error        TEXT,
  checked_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_health_check_results_lookup ON health_check_results(org_id, service, checked_at DESC);
