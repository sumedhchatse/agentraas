-- 002_users_and_indexes.sql
-- Adds dashboard user auth + indexes needed for ranged stats queries (24h/7d/30d/90d).
-- Idempotent: safe to run against an existing database.

CREATE TABLE IF NOT EXISTS users (
  id            SERIAL PRIMARY KEY,
  email         VARCHAR(255) UNIQUE NOT NULL,
  password_hash TEXT NOT NULL,
  org_id        VARCHAR(255),
  plan          VARCHAR(50) NOT NULL DEFAULT 'free',
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_login_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_users_email ON users (email);

-- audit_log is created by an earlier migration (001_*), so these only add indexes.
-- created_at DESC index: every dashboard query filters/sorts on this column.
CREATE INDEX IF NOT EXISTS idx_audit_log_created_at ON audit_log (created_at DESC);

-- Speeds up the per-service breakdown on /api/v1/services (WHERE service = $1 AND created_at >= ...).
CREATE INDEX IF NOT EXISTS idx_audit_log_service_created_at ON audit_log (service, created_at DESC);

-- Speeds up the per-agent breakdown on /api/v1/agents (GROUP BY org_id, agent_id).
CREATE INDEX IF NOT EXISTS idx_audit_log_org_agent ON audit_log (org_id, agent_id, created_at DESC);
