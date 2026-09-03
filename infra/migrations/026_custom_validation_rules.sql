-- 026_custom_validation_rules.sql
-- Backs the dashboard's "Validation Rules" builder (README roadmap item:
-- "Custom validation rule builder (UI)"). One row per org+service+action,
-- `fields` mirrors the same shape config/services.json's static
-- `validation` blocks already use (see validator.js) - so a custom rule
-- is checked with the exact same logic as a curated service's built-in
-- one, just resolved from this table first instead of the static config.
--
-- Unlike curated services, Custom Actions have no built-in validation at
-- all (see the comment at each proxy/mcp call site) - this table is the
-- ONLY way to get payload validation on a Custom Action.
CREATE TABLE IF NOT EXISTS custom_validation_rules (
  id           SERIAL PRIMARY KEY,
  org_id       VARCHAR(255) NOT NULL,
  service      VARCHAR(100) NOT NULL,
  action       VARCHAR(100) NOT NULL,
  fields       JSONB NOT NULL,
  created_by   INTEGER REFERENCES users(id),
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE(org_id, service, action)
);

CREATE INDEX IF NOT EXISTS idx_custom_validation_rules_org ON custom_validation_rules(org_id);
