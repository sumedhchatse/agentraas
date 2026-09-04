-- Per-field dedup rules: lets an org dedupe on a chosen subset of payload
-- fields (e.g. "email") instead of the default whole-payload hash, without
-- requiring every caller to supply their own idempotency key. Mirrors
-- custom_validation_rules (026) in shape and resolution pattern — one row
-- per (org_id, service, action), dashboard-managed, always overrides the
-- default full-payload-hash dedup for that action when present.
CREATE TABLE IF NOT EXISTS custom_dedup_rules (
  id SERIAL PRIMARY KEY,
  org_id VARCHAR(255) NOT NULL,
  service VARCHAR(100) NOT NULL,
  action VARCHAR(100) NOT NULL,
  fields JSONB NOT NULL,
  created_by INTEGER REFERENCES users(id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE(org_id, service, action)
);

CREATE INDEX IF NOT EXISTS idx_custom_dedup_rules_org ON custom_dedup_rules(org_id);
