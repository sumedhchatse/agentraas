-- Self-serve demo account: a designated non-admin account that can seed
-- and reset its own demo data from the dashboard, without needing the
-- single super-admin to log in and do it on their behalf every time.
-- Deliberately its own flag, not `is_admin` — that stays a one-account,
-- system-wide-visibility concept by design (see docs/kb/06-reference.md).
ALTER TABLE users ADD COLUMN IF NOT EXISTS is_demo BOOLEAN NOT NULL DEFAULT false;
