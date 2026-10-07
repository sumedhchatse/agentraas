-- Undo log (crates/api/src/undo.rs): actions that ran and have a known
-- reverse, with the payload that reverses them. Safe to re-run.
CREATE TABLE IF NOT EXISTS undo_log (
  id            BIGSERIAL PRIMARY KEY,
  org_id        TEXT NOT NULL,
  agent_id      TEXT NOT NULL,
  req_id        TEXT NOT NULL,          -- the original call
  service       TEXT NOT NULL,
  action        TEXT NOT NULL,
  undo_service  TEXT NOT NULL,
  undo_action   TEXT NOT NULL,
  undo_payload  JSONB NOT NULL,         -- identifiers the reverse needs, e.g. a charge id
  status        TEXT NOT NULL DEFAULT 'available', -- available | undoing | undone | unknown
  error         TEXT,                   -- last failed attempt, if any
  undo_req_id   TEXT,
  undone_by     INTEGER,                -- users.id
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  expires_at    TIMESTAMPTZ NOT NULL,
  undone_at     TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_undo_log_org ON undo_log (org_id, created_at DESC);

-- A custom action can name another custom action as its reverse.
ALTER TABLE custom_actions ADD COLUMN IF NOT EXISTS undo_action TEXT;
ALTER TABLE custom_actions ADD COLUMN IF NOT EXISTS undo_with JSONB;
