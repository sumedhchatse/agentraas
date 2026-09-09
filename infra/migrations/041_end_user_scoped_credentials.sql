-- 041_end_user_scoped_credentials.sql
-- On-Behalf-Of End-User Identity: lets an org store more than one credential
-- per (org, service) — one per end_user_id — so a multi-tenant agent
-- product built on AgentRaaS can safely isolate its own end-users' own
-- connected accounts instead of every call sharing one org-wide key.
-- NULL end_user_id (every credential today) keeps meaning exactly what it
-- means now: a shared org-wide credential. Purely additive.

ALTER TABLE service_credentials ADD COLUMN IF NOT EXISTS end_user_id VARCHAR(255);
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS end_user_id VARCHAR(255);

-- At most one active credential per (org, service, end_user) once an
-- end_user_id is actually used — the existing NULL-scoped uniqueness
-- behavior for shared org-wide credentials is untouched.
CREATE UNIQUE INDEX IF NOT EXISTS idx_service_credentials_end_user_active
  ON service_credentials (org_id, service, end_user_id) WHERE revoked_at IS NULL AND end_user_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_audit_log_end_user ON audit_log (org_id, end_user_id) WHERE end_user_id IS NOT NULL;
