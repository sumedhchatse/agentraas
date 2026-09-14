-- 045_agent_identity_tokens.sql
-- Agent Identity: short-lived, scope-restricted tokens for an already
-- Connect Agent'd (org_id, agent_id) pair, as an alternative to the
-- long-lived, unscoped api_keys credential. A caller presenting one of
-- these can only call the (service, action) pairs listed in `scopes`,
-- and the token stops working on its own once `expires_at` passes —
-- no manual revocation needed for the common case. Purely additive:
-- existing api_keys auth is completely unaffected.

CREATE TABLE IF NOT EXISTS agent_identity_tokens (
  id            SERIAL PRIMARY KEY,
  api_key_id    INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  token_hash    TEXT NOT NULL,
  token_prefix  VARCHAR(16) NOT NULL,
  scopes        JSONB NOT NULL,
  expires_at    TIMESTAMPTZ NOT NULL,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_used_at  TIMESTAMPTZ,
  revoked_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_agent_identity_tokens_prefix ON agent_identity_tokens (token_prefix);
CREATE INDEX IF NOT EXISTS idx_agent_identity_tokens_api_key ON agent_identity_tokens (api_key_id);
