-- Stateful Human-in-the-Loop (HITL) Gateway (Enterprise, opt-in per org) —
-- freezes a webhook call matching a configured rule, sends an interactive
-- Slack approval card, and resumes (forwards for real) only on approval.
-- See crates/api/src/ee/hitl.rs.

-- A request matching ANY row here for its org+service+action is frozen.
-- field/operator/threshold all NULL means "always require approval for
-- this service.action"; otherwise the named payload field is compared
-- numerically against threshold using operator.
CREATE TABLE IF NOT EXISTS hitl_rules (
  id         SERIAL PRIMARY KEY,
  org_id     VARCHAR(255) NOT NULL,
  service    VARCHAR(255) NOT NULL,
  action     VARCHAR(255) NOT NULL,
  field      VARCHAR(255),
  operator   VARCHAR(10),
  threshold  DOUBLE PRECISION,
  created_by INTEGER NOT NULL REFERENCES users(id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_hitl_rules_lookup ON hitl_rules (org_id, service, action);

-- One Slack destination per org: a bot token (chat.postMessage) plus the
-- signing secret used to verify inbound button-click callbacks.
CREATE TABLE IF NOT EXISTS hitl_slack_config (
  org_id                     VARCHAR(255) PRIMARY KEY,
  encrypted_bot_token        TEXT NOT NULL,
  encrypted_signing_secret   TEXT NOT NULL,
  default_channel            VARCHAR(255) NOT NULL,
  updated_at                 TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One row per frozen request. `result` is filled in once an approval has
-- actually been forwarded; `dedup_hash`/`dedup_ttl_seconds` let approval
-- complete the same dedup slot the original request already claimed.
CREATE TABLE IF NOT EXISTS hitl_requests (
  id               SERIAL PRIMARY KEY,
  req_id           VARCHAR(64) NOT NULL UNIQUE,
  org_id           VARCHAR(255) NOT NULL,
  agent_id         VARCHAR(255) NOT NULL,
  api_key          VARCHAR(255) NOT NULL,
  service          VARCHAR(255) NOT NULL,
  action           VARCHAR(255) NOT NULL,
  payload          JSONB NOT NULL,
  dedup_hash       VARCHAR(64) NOT NULL,
  dedup_ttl_seconds INTEGER,
  status           VARCHAR(20) NOT NULL DEFAULT 'pending',
  result           JSONB,
  error_message    TEXT,
  resolved_by      VARCHAR(255),
  slack_channel    VARCHAR(255),
  slack_message_ts VARCHAR(64),
  created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  resolved_at      TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS idx_hitl_requests_org ON hitl_requests (org_id, created_at);

-- Monthly approval quota override — no row means the default free-tier
-- limit (10/month, enforced in code) applies. Set a higher/very large
-- value here for a paid org, same "no row = default" shape as
-- org_limit_overrides (migration 015).
CREATE TABLE IF NOT EXISTS org_hitl_limit_overrides (
  org_id        VARCHAR(255) PRIMARY KEY,
  monthly_limit INTEGER NOT NULL
);
