-- Record and replay (crates/api/src/recordings.rs): calls made with
-- `X-AgentRaaS-Record: <name>`, kept so they can be played back with
-- `X-AgentRaaS-Replay: <name>` without calling the provider. Payload and
-- response are encrypted like the dead-letter queue. Safe to re-run.
CREATE TABLE IF NOT EXISTS recordings (
  id                 BIGSERIAL PRIMARY KEY,
  org_id             TEXT NOT NULL,
  name               TEXT NOT NULL,
  agent_id           TEXT NOT NULL,
  req_id             TEXT NOT NULL,
  service            TEXT NOT NULL,
  action             TEXT NOT NULL,
  encrypted_payload  TEXT NOT NULL,
  encrypted_response TEXT NOT NULL,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  expires_at         TIMESTAMPTZ NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recordings_name ON recordings (org_id, name, service, action, id);
CREATE INDEX IF NOT EXISTS idx_recordings_expires ON recordings (expires_at);
