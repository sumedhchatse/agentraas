-- 015_org_limit_overrides.sql
-- Lets a specific org's monthly call limit be raised (or lowered) above the
-- global default, without changing the limit for everyone. Looked up by
-- checkUsageLimit() before falling back to the global CLOUD_MONTHLY_LIMIT /
-- SELF_HOST_MONTHLY_LIMIT env vars. Set via infra/scripts/set-org-limit.sh.

CREATE TABLE IF NOT EXISTS org_limit_overrides (
  org_id        VARCHAR(255) PRIMARY KEY,
  monthly_limit INTEGER NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
