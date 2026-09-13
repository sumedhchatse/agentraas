-- Multi-step run/step tracking was previously ephemeral (Redis only,
-- 24h TTL, via core::checkpoint) — this persists it onto the audit log
-- so a run's step history survives past the checkpoint TTL and is
-- queryable/visible in the dashboard.
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS run_id TEXT;
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS step_id TEXT;

CREATE INDEX IF NOT EXISTS idx_audit_log_run_id ON audit_log (run_id, created_at) WHERE run_id IS NOT NULL;

-- Same reasoning for HITL: a frozen request that's part of a multi-step
-- run should show that context (prior steps already done) to whoever's
-- approving it in Slack, and the eventual log_audit call on resolution
-- should carry the same run_id/step_id the original request had.
ALTER TABLE hitl_requests ADD COLUMN IF NOT EXISTS run_id TEXT;
ALTER TABLE hitl_requests ADD COLUMN IF NOT EXISTS step_id TEXT;
