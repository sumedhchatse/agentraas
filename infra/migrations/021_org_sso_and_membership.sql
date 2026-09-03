-- 021_org_sso_and_membership.sql
-- Enterprise SSO (src/ee/auth) — lets an org authenticate its users via an
-- external OIDC identity provider instead of AgentRaaS's own email/password
-- flow, and assigns each user a role within that org automatically from
-- their IdP identity (domain match + best-effort group claim — see
-- SsoManager.matchOrgByEmailDomain / mapClaimsToRole). There is no manual
-- invite-by-email flow yet — membership is entirely SSO-derived for now.
--
-- IMPORTANT: this is a completely separate concept from the existing
-- single global admin flag (users.is_admin, migration 013's
-- idx_only_one_admin unique index). That flag represents one system-wide
-- cloud-operator account and is untouched by anything below.
-- org_members.role = 'admin' means "an admin of one specific org" (per-org,
-- many allowed across the system) — do not conflate the two. See
-- requireOrgAdmin in server.js, kept deliberately distinct from the
-- existing requireAdmin/is_admin.
--
-- Purely additive: doesn't touch users, api_keys, custom_actions,
-- service_credentials, or audit_log. Every existing org_id VARCHAR(255)
-- column elsewhere in the schema keeps working exactly as today — orgs
-- rows only start existing once an org admin actually configures SSO for
-- that org_id (see PUT /api/v1/auth/sso/:orgId/config in server.js).
-- There is deliberately NO foreign key from users.org_id to orgs.org_id:
-- most org_id values in this system will never have a corresponding orgs
-- row (SSO is opt-in), and users.org_id already has years of
-- unconstrained free-string data — same "org_id VARCHAR(255) is the real
-- key, no orgs table required" shape org_limit_overrides already has
-- (migration 015).

-- One row per org that has configured SSO. org_id is the same natural-key
-- string used everywhere else in the schema (users.org_id, api_keys.org_id,
-- etc.) — not a new surrogate id, so no existing org_id-typed column needs
-- to change to relate to this table.
CREATE TABLE IF NOT EXISTS orgs (
  org_id      VARCHAR(255) PRIMARY KEY,
  name        VARCHAR(255),
  -- Display name only, best-effort (e.g. taken from an IdP claim, or the
  -- domain). Never used for lookups — org_id is always the key.
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Real multi-user-per-org membership — something this schema has never had
-- before (previously "org" was just a tag rows happened to share, with no
-- concept of distinct users holding different permissions within one org).
-- One row per (user, org) pair. Written on every successful SSO login for
-- that org (see SsoManager.upsertMembership), always overwriting any
-- previous role rather than merging — role is meant to be re-derived from
-- the IdP's current claims each time, the same "never trust a stale value,
-- recheck the source of truth" spirit as users.is_admin being rechecked
-- fresh from the DB rather than trusted from an old JWT.
CREATE TABLE IF NOT EXISTS org_members (
  user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
  org_id      VARCHAR(255) NOT NULL REFERENCES orgs(org_id) ON DELETE CASCADE ON UPDATE CASCADE,
  role        VARCHAR(50) NOT NULL DEFAULT 'developer'
              CHECK (role IN ('admin', 'developer', 'auditor')),
  -- Only admin vs non-admin is actually permission-checked in this pass
  -- (see requireOrgAdmin, server.js). developer/auditor are stored and
  -- selectable now (surfaced on GET /api/v1/auth/me) so the distinction
  -- exists in data before it's enforced — avoids a second migration just
  -- to add the column later. Enforcing developer vs auditor differently
  -- is a future increment, same incremental spirit as hmac/dlp/rate_limiter.
  created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (user_id, org_id)
  -- A user has exactly one role in a given org — SSO always determines it
  -- deterministically from the current IdP claims, there's no concept of a
  -- user holding two different roles in the same org simultaneously.
);

CREATE INDEX IF NOT EXISTS idx_org_members_org ON org_members (org_id);

-- Per-org IdP configuration. One row per org (one IdP per org in this first
-- pass — no support yet for an org federating multiple IdPs).
CREATE TABLE IF NOT EXISTS sso_configs (
  org_id                  VARCHAR(255) PRIMARY KEY REFERENCES orgs(org_id) ON DELETE CASCADE ON UPDATE CASCADE,
  issuer_url              TEXT NOT NULL,
  -- The IdP's OIDC issuer, e.g. https://your-tenant.okta.com/oauth2/default —
  -- passed straight to openid-client's discovery() to fetch
  -- /.well-known/openid-configuration.
  client_id               VARCHAR(255) NOT NULL,
  encrypted_client_secret TEXT NOT NULL,
  -- "iv:authTag:ciphertext" hex — identical format to service_credentials'
  -- stored payload. client_secret is IdP-issued, exactly as sensitive as
  -- any other stored third-party credential, so it reuses the same
  -- AES-256-GCM scheme (encryptCredential/decryptCredential, keyed by
  -- CREDENTIALS_ENCRYPTION_KEY) rather than a new encryption mechanism.
  allowed_domains         TEXT NOT NULL,
  -- Comma-separated email domains (e.g. "acme.com,acme.io") allowed to
  -- auto-join this org via SSO. Plain TEXT + app-side split, not a Postgres
  -- array column — nothing else in this schema uses array columns, and a
  -- handful of domains per org doesn't need one.
  default_role            VARCHAR(50) NOT NULL DEFAULT 'developer'
                           CHECK (default_role IN ('admin', 'developer', 'auditor')),
  -- Role assigned when the ID token carries no recognized group/role claim
  -- (see SsoManager.mapClaimsToRole).
  enabled                 BOOLEAN NOT NULL DEFAULT true,
  -- Lets an org admin disable SSO login for their org without deleting the
  -- stored config (e.g. while rotating a compromised client secret).
  created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
