-- Tool Output Sanitization (Enterprise, opt-in per org) — see PORT_PROGRESS.md's
-- feature plan / inherited-forging-sutton.md Phase 5. Off by default: a row
-- only exists once an org has actually toggled it, same "no row = default"
-- shape as org_limit_overrides (migration 015) rather than a column with a
-- default on an existing per-org table.
CREATE TABLE IF NOT EXISTS org_output_sanitization (
  org_id     VARCHAR(255) PRIMARY KEY,
  enabled    BOOLEAN NOT NULL DEFAULT false,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
