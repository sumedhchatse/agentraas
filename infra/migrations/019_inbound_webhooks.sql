-- 019_inbound_webhooks.sql
-- Inbound webhook signature verification (HMAC) — lets a user register a
-- provider webhook (Stripe, GitHub, Shopify, WhatsApp, Twilio), receive it
-- at a unique AgentRaaS URL, have the signature verified against the
-- provider's real scheme, and only then forward the (now-proven-authentic)
-- payload to their real destination (e.g. an n8n webhook URL).
--
-- This is the inverse direction of the existing outbound proxy — outbound
-- protects calls AgentRaaS makes TO a service; this protects calls a
-- service makes TO the user's downstream system, verified at the edge.

CREATE TABLE IF NOT EXISTS inbound_webhooks (
  id                    SERIAL PRIMARY KEY,
  user_id               INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
  org_id                VARCHAR(255) NOT NULL,
  provider              VARCHAR(50) NOT NULL,
  -- 'stripe' | 'github' | 'shopify' | 'whatsapp' | 'twilio'
  webhook_secret        TEXT NOT NULL,
  -- Encrypted at rest via the same AES-256-GCM scheme as service_credentials
  -- (encryptCredential/decryptCredential) — this is the provider's signing
  -- secret, exactly as sensitive as any other saved credential.
  destination_url       TEXT NOT NULL,
  -- Where the verified payload gets forwarded (e.g. an n8n webhook URL).
  -- Checked against validateTargetUrl (the same SSRF guard Custom Actions
  -- uses) at registration time, so this can't become a fresh SSRF vector.
  inbound_token         VARCHAR(64) UNIQUE NOT NULL,
  -- Random, unguessable — part of the public URL
  -- (/v1/inbound/<token>) the provider is configured to call.
  whatsapp_verify_token TEXT,
  -- Only used when provider = 'whatsapp'. Meta's webhook setup requires a
  -- GET handshake (hub.mode/hub.verify_token/hub.challenge, no signature —
  -- there's no body to sign yet) before it will register a callback URL at
  -- all. Nullable for every other provider, which has no equivalent step.
  created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_used_at          TIMESTAMPTZ,
  revoked_at             TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_inbound_webhooks_user ON inbound_webhooks (user_id);
CREATE INDEX IF NOT EXISTS idx_inbound_webhooks_token ON inbound_webhooks (inbound_token);
