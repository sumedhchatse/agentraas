-- Pay-as-you-go Cloud billing (billing.rs). Safe to re-run.

-- Monthly spend cap per org, in whole dollars. No row = the default cap
-- (billing::DEFAULT_CAP_USD). Past the cap, calls get 402 until next month.
CREATE TABLE IF NOT EXISTS billing_caps (
  org_id          TEXT PRIMARY KEY,
  monthly_cap_usd INTEGER NOT NULL CHECK (monthly_cap_usd >= 1),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- One row per org per billed month. The primary key is the claim: the
-- monthly job inserts the row before calling Paddle, so a month can never
-- be charged twice. status: pending (claimed, Paddle not called yet or call
-- in flight), charged, skipped (under one block, carried over), failed
-- (Paddle said no), unknown (no answer: may or may not have charged, check
-- Paddle before touching it).
CREATE TABLE IF NOT EXISTS billing_charges (
  org_id       TEXT NOT NULL,
  month        TEXT NOT NULL,            -- "YYYY-MM", UTC
  actions      BIGINT NOT NULL,          -- actions that ran that month
  carried_in   BIGINT NOT NULL,          -- unbilled remainder from the month before
  blocks       BIGINT NOT NULL,          -- 1,000-action blocks charged
  carried_out  BIGINT NOT NULL,          -- remainder rolled into next month
  amount_cents BIGINT NOT NULL,
  status       TEXT NOT NULL,
  error        TEXT,
  created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  PRIMARY KEY (org_id, month)
);

-- Every proxied call on Cloud asks "is this org on payg?" (billing::is_payg_org).
CREATE INDEX IF NOT EXISTS idx_users_payg_org ON users (org_id) WHERE plan = 'payg';
