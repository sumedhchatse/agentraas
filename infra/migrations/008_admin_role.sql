-- 008_admin_role.sql
-- Adds a real admin role: an admin can see all users/orgs system-wide,
-- not just their own. First admin is bootstrapped by install.sh (registers
-- an account, then flips this flag directly via SQL — there's no other way
-- to create the very first admin, since promoting requires an existing admin).

ALTER TABLE users ADD COLUMN IF NOT EXISTS is_admin BOOLEAN NOT NULL DEFAULT false;

CREATE INDEX IF NOT EXISTS idx_users_is_admin ON users (is_admin) WHERE is_admin = true;
