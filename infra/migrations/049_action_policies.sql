-- What each agent is allowed to do (SPEC-ACTION-POLICIES.md). Every
-- matching row applies (restrictions stack; no most-specific-wins):
--   effect 'deny'    = the agent may not call service.action at all
--   effect 'require' = the payload must pass `fields` (validation-rule
--                      syntax), otherwise on_violation
-- agent_id NULL = every agent in the org; action '*' = every action of the
-- service. on_violation 'hitl' needs Team + the enterprise build, checked
-- at creation time in crates/api/src/action_policies.rs.
CREATE TABLE IF NOT EXISTS action_policies (
  id           SERIAL PRIMARY KEY,
  org_id       VARCHAR(255) NOT NULL,
  agent_id     VARCHAR(255),
  service      VARCHAR(255) NOT NULL,
  action       VARCHAR(255) NOT NULL,
  effect       VARCHAR(10) NOT NULL CHECK (effect IN ('deny', 'require')),
  fields       JSONB,
  on_violation VARCHAR(10) NOT NULL DEFAULT 'block' CHECK (on_violation IN ('block', 'hitl')),
  created_by   INTEGER NOT NULL REFERENCES users(id),
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_action_policies_lookup ON action_policies (org_id, service);
