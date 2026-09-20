-- Per-agent action spend/call caps (SPEC-SPEND-CAPS.md). Call-count based,
-- not dollar-based (v1). A request matching a rule's org+service+action
-- (and agent_id, if set) is counted against a windowed Redis counter; the
-- rule's own on_exceed decides what happens once max_calls is hit within
-- the window. Block-only rules are available to every tier; on_exceed =
-- 'hitl' additionally requires Tier::Team, enforced at creation time in
-- crates/api/src/spend_caps.rs (same require_tier pattern as hitl_rules).
--
-- agent_id NULL = applies to every agent in the org for that service.action.
-- A more specific (org, agent_id, service, action) row wins over a
-- (org, NULL, service, action) org-wide default when both exist — same
-- "most specific match" convention resolve_route already uses.
CREATE TABLE IF NOT EXISTS spend_cap_rules (
  id          SERIAL PRIMARY KEY,
  org_id      VARCHAR(255) NOT NULL,
  agent_id    VARCHAR(255),
  service     VARCHAR(255) NOT NULL,
  action      VARCHAR(255) NOT NULL,
  time_window VARCHAR(10) NOT NULL CHECK (time_window IN ('hour', 'day')), -- not "window" - reserved SQL keyword
  max_calls   INTEGER NOT NULL CHECK (max_calls > 0),
  on_exceed   VARCHAR(10) NOT NULL CHECK (on_exceed IN ('block', 'hitl')),
  created_by  INTEGER NOT NULL REFERENCES users(id),
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_spend_cap_rules_lookup ON spend_cap_rules (org_id, agent_id, service, action);
