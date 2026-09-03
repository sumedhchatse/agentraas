-- 013_single_admin_and_forced_password_change.sql
--
-- Two changes:
--
-- 1. Enforces "at most one admin exists" at the DATABASE level, not just in
--    application code — a partial unique index on is_admin=true means
--    Postgres itself rejects a second admin, even if some future code path
--    forgets to check. Demotes everyone except admin@agentraas.local first
--    (the index creation would fail otherwise, against inconsistent data).
--
-- 2. Adds must_change_password — set true for the admin account here, since
--    it likely still holds a shown-once generated password from an earlier
--    bootstrap run. Cleared automatically the first time they successfully
--    change their password (see server.js's change-password endpoint).

ALTER TABLE users ADD COLUMN IF NOT EXISTS must_change_password BOOLEAN NOT NULL DEFAULT false;

-- Demote every admin except the one that stays.
UPDATE users SET is_admin = false WHERE is_admin = true AND email != 'admin@agentraas.local';

-- Ensure admin@agentraas.local is admin and must change its password once.
UPDATE users SET is_admin = true, must_change_password = true WHERE email = 'admin@agentraas.local';

-- Now safe to add — at most one row has is_admin=true at this point.
CREATE UNIQUE INDEX IF NOT EXISTS idx_only_one_admin ON users ((is_admin)) WHERE is_admin = true;
