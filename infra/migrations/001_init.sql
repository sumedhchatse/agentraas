CREATE TABLE IF NOT EXISTS audit_log (
    id BIGSERIAL PRIMARY KEY,
    req_id TEXT NOT NULL,
    api_key TEXT,
    org_id TEXT,
    agent_id TEXT,
    service TEXT NOT NULL,
    action TEXT NOT NULL,
    status TEXT NOT NULL,
    error_type TEXT,
    duration_ms INTEGER,
    payload_hash TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_audit_created ON audit_log(created_at);
CREATE INDEX idx_audit_org ON audit_log(org_id, created_at);
CREATE INDEX idx_audit_apikey ON audit_log(api_key, created_at);
