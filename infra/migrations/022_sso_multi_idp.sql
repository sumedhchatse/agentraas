-- 022_sso_multi_idp.sql
-- Enterprise SSO (src/ee/auth): allow more than one IdP per org (e.g. a
-- parent company where each acquired subsidiary keeps its own Okta
-- tenant, or separate IdPs per business unit). Migration 021 assumed
-- exactly one IdP per org and PK'd sso_configs on org_id alone - this
-- widens that to a real id-keyed table, org_id now just an indexed
-- column. Purely additive elsewhere: orgs/org_members untouched.
--
-- server.js's SSO login route now takes an optional config_id - when an
-- org has exactly one enabled config (the common case), it's still
-- picked automatically with no config_id needed; multi-IdP orgs must
-- specify which one.

ALTER TABLE sso_configs DROP CONSTRAINT sso_configs_pkey;
ALTER TABLE sso_configs ADD COLUMN IF NOT EXISTS id SERIAL;
ALTER TABLE sso_configs ADD PRIMARY KEY (id);
CREATE INDEX IF NOT EXISTS idx_sso_configs_org ON sso_configs (org_id);
