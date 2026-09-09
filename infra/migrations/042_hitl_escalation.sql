-- HITL SLA Tracking + Auto-Escalation — a stuck approval pages a backup
-- channel instead of sitting in `pending` forever with nobody watching.
-- See crates/api/src/ee/hitl.rs::spawn_hitl_escalation_loop.

ALTER TABLE hitl_requests ADD COLUMN IF NOT EXISTS escalated_at TIMESTAMPTZ;

-- "No row = feature off" — same shape as org_hitl_limit_overrides
-- (migration 036): opt-in per org, no new tier-gating logic needed since
-- hitl.rs is already Pro+/Enterprise end to end.
CREATE TABLE IF NOT EXISTS hitl_escalation_config (
  org_id             VARCHAR(255) PRIMARY KEY,
  sla_minutes        INTEGER NOT NULL DEFAULT 60,
  escalation_channel VARCHAR(255) NOT NULL,
  updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
