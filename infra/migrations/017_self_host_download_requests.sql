-- 017_self_host_download_requests.sql
-- Tracks the short form collected before a self-host download unlocks (see
-- POST /api/v1/download/self-host/request). Gives a record of who
-- requested it and why — useful for following up about a commercial
-- license — and the GET download endpoint checks a row exists here before
-- serving the file, as a real gate rather than just a hidden UI button.

CREATE TABLE IF NOT EXISTS self_host_download_requests (
  id         SERIAL PRIMARY KEY,
  user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE ON UPDATE CASCADE,
  reason     TEXT NOT NULL,
  company    VARCHAR(255),
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_self_host_download_requests_user ON self_host_download_requests (user_id);
