-- 003_api_keys.sql
-- Lets a dashboard user generate a real API key for a specific org_id/agent_id
-- pair via "Connect Agent". Idempotent: safe to run against an existing database.

CREATE TABLE IF NOT EXISTS api_keys (
  id            SERIAL PRIMARY KEY,
  user_id       INTEGER REFERENCES users(id) ON DELETE CASCADE,
  org_id        VARCHAR(255) NOT NULL,
  agent_id      VARCHAR(255) NOT NULL,
  label         VARCHAR(255),
  key_hash      TEXT NOT NULL,
  key_prefix    VARCHAR(16) NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_used_at  TIMESTAMPTZ,
  revoked_at    TIMESTAMPTZ
);

-- Fast lookup path when verifying an incoming request's key.
CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys (key_prefix);
CREATE INDEX IF NOT EXISTS idx_api_keys_org_agent ON api_keys (org_id, agent_id);
CREATE INDEX IF NOT EXISTS idx_api_keys_user ON api_keys (user_id);
