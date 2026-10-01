-- Held calls (hitl_requests) stored the agent's raw API key, so a database
-- dump or backup held usable keys. The server now stores
-- "<first 16 chars>#sha256:<hex>" (agent::db::stored_key_ref); this converts
-- the rows written before that. Safe to re-run: converted rows are skipped.
UPDATE hitl_requests
SET api_key = left(api_key, 16) || '#sha256:' || encode(sha256(convert_to(api_key, 'UTF8')), 'hex')
WHERE api_key NOT LIKE '%#sha256:%'
  AND api_key NOT IN ('', 'anonymous');
