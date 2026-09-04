-- 028_notification_webhooks.sql
-- Instant outage notifications: lets an org configure a Slack/Discord
-- incoming webhook, or a Telegram bot, to be pinged when the circuit
-- breaker trips open for a service that org actually uses — not a
-- global broadcast (the circuit itself is shared across every org using
-- that service), scoped to orgs actually affected (see notifyCircuitOpen
-- in server.js, called from the "blocked: circuit_open" path).
--
-- encrypted_target is the same AES-256-GCM scheme as service_credentials
-- (crypto-helper.js) — a Slack/Discord webhook URL or a Telegram bot
-- token is exactly as sensitive as any other stored credential.
CREATE TABLE IF NOT EXISTS notification_webhooks (
  id                SERIAL PRIMARY KEY,
  org_id            VARCHAR(255) NOT NULL,
  type              VARCHAR(20) NOT NULL CHECK (type IN ('slack', 'discord', 'telegram')),
  encrypted_target  TEXT NOT NULL,
  -- Slack/Discord: the incoming webhook URL, encrypted.
  -- Telegram: the bot token, encrypted (see api.telegram.org/bot<token>/sendMessage).
  extra             VARCHAR(255),
  -- Telegram only: the chat_id to send to. NULL for slack/discord.
  created_by        INTEGER REFERENCES users(id),
  created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  UNIQUE(org_id, type)
);

CREATE INDEX IF NOT EXISTS idx_notification_webhooks_org ON notification_webhooks(org_id);
