-- Server-side session revocation (auth/mod.rs). A session cookie whose
-- issued-at is before this moment is rejected: set on password change,
-- password reset and "log out everywhere". NULL = every unexpired session
-- is valid. Safe to re-run.
ALTER TABLE users ADD COLUMN IF NOT EXISTS sessions_valid_after TIMESTAMPTZ;
