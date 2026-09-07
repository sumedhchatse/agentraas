-- Multi-step run/step tracking was previously ephemeral (Redis only,
-- 24h TTL, via core::checkpoint) — this persists it onto the audit log
-- so a run's step history survives past the checkpoint TTL and is
-- queryable/visible in the dashboard.
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS run_id TEXT;
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS step_id TEXT;

CREATE INDEX IF NOT EXISTS idx_audit_log_run_id ON audit_log (run_id, created_at) WHERE run_id IS NOT NULL;

-- No hitl_requests ALTER here — that table only exists in the private
-- agentraas-enterprise repo's Enterprise-gated schema (migration
-- 036_hitl_gateway.sql was never mirrored here either), since HITL
-- lives entirely under src/ee/, absent from this Community-only repo.
