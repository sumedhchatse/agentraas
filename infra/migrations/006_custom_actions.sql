-- 006_custom_actions.sql
-- Lets a dashboard user register their own arbitrary endpoint (any URL, not just
-- the curated services in config/services.json), so agents aren't limited to a
-- fixed whitelist of integrations. Registration happens via the authenticated
-- human user, not the agent itself — that's the SSRF safeguard: the agent can
-- only invoke endpoints a trusted human has already explicitly approved by name,
-- never supply a live target URL of its own choosing at request time.

CREATE TABLE IF NOT EXISTS custom_actions (
  id                SERIAL PRIMARY KEY,
  user_id           INTEGER REFERENCES users(id) ON DELETE CASCADE,
  org_id            VARCHAR(255) NOT NULL,
  name              VARCHAR(100) NOT NULL,
  method            VARCHAR(10) NOT NULL DEFAULT 'POST',
  target_url        TEXT NOT NULL,
  auth_type         VARCHAR(20) NOT NULL DEFAULT 'none', -- none | bearer | basic | header
  auth_header_name  VARCHAR(100),                        -- used only when auth_type='header'
  content_type      VARCHAR(100) NOT NULL DEFAULT 'application/json',
  created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  revoked_at        TIMESTAMPTZ
);

-- One active action per (org, name) — re-registering the same name retires the old one.
CREATE UNIQUE INDEX IF NOT EXISTS idx_custom_actions_org_name_active
  ON custom_actions (org_id, name) WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_custom_actions_user ON custom_actions (user_id);
