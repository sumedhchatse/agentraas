-- 023_org_invites.sql
-- Enterprise SSO/RBAC (src/ee/auth): manual invite-by-email, for orgs that
-- want to add members without going through their IdP (or don't have SSO
-- configured at all - membership was previously SSO-derived only).
--
-- Mirrors the existing password_reset_tokens/email_verification_tokens
-- shape (hashed single-use token, expiry, used_at-style acceptance
-- marker) rather than inventing a new token pattern.

CREATE TABLE IF NOT EXISTS org_invites (
  id                SERIAL PRIMARY KEY,
  org_id            VARCHAR(255) NOT NULL REFERENCES orgs(org_id) ON DELETE CASCADE ON UPDATE CASCADE,
  email             VARCHAR(255) NOT NULL,
  role              VARCHAR(50) NOT NULL DEFAULT 'developer'
                    CHECK (role IN ('admin', 'developer', 'auditor')),
  token_hash        TEXT UNIQUE NOT NULL,
  invited_by_user_id INTEGER REFERENCES users(id) ON DELETE SET NULL ON UPDATE CASCADE,
  expires_at        TIMESTAMPTZ NOT NULL,
  accepted_at       TIMESTAMPTZ,
  created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_org_invites_org ON org_invites (org_id);
CREATE INDEX IF NOT EXISTS idx_org_invites_email ON org_invites (email);
