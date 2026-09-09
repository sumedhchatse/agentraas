-- 040_custom_mcp_servers.sql
-- MCP Custom Actions: lets a dashboard user register a third-party MCP
-- server (any JSON-RPC tools/call endpoint, not just AgentRaaS's own
-- curated services or plain-HTTP Custom Actions) so every tool it exposes
-- gets AgentRaaS's dedup/circuit-breaker/audit pipeline automatically.
-- Deliberately its own table, not a row in custom_actions: one row here
-- represents a whole server exposing many tools (name.remote_tool), not
-- a single name->URL mapping.

CREATE TABLE IF NOT EXISTS custom_mcp_servers (
  id                SERIAL PRIMARY KEY,
  user_id           INTEGER REFERENCES users(id) ON DELETE CASCADE,
  org_id            VARCHAR(255) NOT NULL,
  name              VARCHAR(100) NOT NULL,  -- becomes the tool-name prefix: name.remote_tool
  target_url        TEXT NOT NULL,
  auth_type         VARCHAR(20) NOT NULL DEFAULT 'none', -- none | bearer | basic | header
  auth_header_name  VARCHAR(100),                        -- used only when auth_type='header'
  created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  revoked_at        TIMESTAMPTZ
);

-- One active MCP server per (org, name) — re-registering the same name retires the old one.
CREATE UNIQUE INDEX IF NOT EXISTS idx_custom_mcp_servers_org_name_active
  ON custom_mcp_servers (org_id, name) WHERE revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_custom_mcp_servers_user ON custom_mcp_servers (user_id);
