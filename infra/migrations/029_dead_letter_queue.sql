-- 029_dead_letter_queue.sql
-- One-Click Payload Replay: when a genuinely upstream call fails (the
-- target API itself returned an error — not a client-side rejection like
-- a validation or usage-limit block, see the scoping note in
-- src/core/proxy's catch block), the request's payload is stored here,
-- encrypted at rest (same AES-256-GCM scheme as service_credentials —
-- a failed request's payload can carry exactly the same sensitive data a
-- successful one would), so a human can review it in the dashboard and
-- either edit-and-replay it or dismiss it once the underlying issue is
-- understood/fixed.
--
-- Replay is dashboard-authenticated (the logged-in org member replaying
-- it), not a replay of the original agent's raw API key — cleaner than
-- storing a live, reusable credential long-term just for this.
CREATE TABLE IF NOT EXISTS dead_letter_queue (
  id                SERIAL PRIMARY KEY,
  req_id            VARCHAR(64) NOT NULL,
  org_id            VARCHAR(255) NOT NULL,
  agent_id          VARCHAR(255) NOT NULL,
  service           VARCHAR(100) NOT NULL,
  action            VARCHAR(100) NOT NULL,
  encrypted_payload TEXT NOT NULL,
  error_message     TEXT,
  created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  replayed_at       TIMESTAMPTZ,
  dismissed_at      TIMESTAMPTZ
);

-- The dashboard list only ever shows still-open entries (not replayed,
-- not dismissed) — a partial index keeps that the cheap, common case.
CREATE INDEX IF NOT EXISTS idx_dlq_org_open ON dead_letter_queue(org_id, created_at DESC)
  WHERE replayed_at IS NULL AND dismissed_at IS NULL;
