-- Tool Result & Context Pruner — opt-in per org (Community + Enterprise
-- both get this feature, unlike org_output_sanitization). Off by default:
-- a row only exists once an org has actually toggled it.
CREATE TABLE IF NOT EXISTS org_output_pruning (
  org_id     VARCHAR(255) PRIMARY KEY,
  enabled    BOOLEAN NOT NULL DEFAULT false,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
