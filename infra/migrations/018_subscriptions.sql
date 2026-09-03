-- 018_subscriptions.sql
-- Tracks Paddle subscription state per user. users.plan ('free' | 'pro') is
-- the fast-path field checked everywhere else in the app (usage limits,
-- rate limits, etc) — this table is the source of truth for *why* it's
-- set that way, and gives webhook handling somewhere to record events
-- idempotently (Paddle can and does redeliver webhooks).

CREATE TABLE IF NOT EXISTS subscriptions (
  id                    SERIAL PRIMARY KEY,
  user_id               INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
  paddle_subscription_id VARCHAR(255) UNIQUE NOT NULL,
  paddle_customer_id    VARCHAR(255) NOT NULL,
  status                VARCHAR(50) NOT NULL,
  -- active | trialing | past_due | paused | canceled — mirrors Paddle's own
  -- subscription status values, so webhook handling can just pass them through.
  current_period_end    TIMESTAMPTZ,
  created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_subscriptions_user ON subscriptions (user_id);

-- Every processed webhook event id, so a redelivered webhook (Paddle retries
-- on any non-2xx response, and can also send duplicates) is a safe no-op
-- instead of double-applying a plan change.
CREATE TABLE IF NOT EXISTS processed_webhook_events (
  event_id   VARCHAR(255) PRIMARY KEY,
  source     VARCHAR(50) NOT NULL,
  processed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
