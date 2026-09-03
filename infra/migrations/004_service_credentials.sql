-- 004_service_credentials.sql
-- Lets a dashboard user store their own service API keys (Stripe, WhatsApp, etc.)
-- per org, encrypted at rest. This is what makes credential setup self-serve —
-- previously only an operator with server access could set these via env vars.
-- Idempotent: safe to run against an existing database.

CREATE TABLE IF NOT EXISTS service_credentials (
  id                 SERIAL PRIMARY KEY,
  user_id            INTEGER REFERENCES users(id) ON DELETE CASCADE,
  org_id             VARCHAR(255) NOT NULL,
  service            VARCHAR(100) NOT NULL,
  encrypted_payload  TEXT NOT NULL,
  masked_preview     VARCHAR(50),
  created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  revoked_at         TIMESTAMPTZ
);

-- Fast lookup: "give me the active credential for this org+service" is the hot path,
-- called on every forwarded request.
CREATE INDEX IF NOT EXISTS idx_service_credentials_lookup
  ON service_credentials (org_id, service, created_at DESC)
  WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_service_credentials_user ON service_credentials (user_id);
