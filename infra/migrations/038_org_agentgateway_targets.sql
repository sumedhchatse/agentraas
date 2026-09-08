-- Agentgateway plugin (beta): lets an org list other MCP/tool backends they
-- want unified behind one gateway alongside AgentRaaS's own MCP endpoint.
-- Storage/CRUD only for now — provisioning the actual per-org gateway
-- process is a separate follow-up (needs a container-orchestration decision).
CREATE TABLE IF NOT EXISTS org_agentgateway_targets (
  id         SERIAL PRIMARY KEY,
  org_id     VARCHAR(255) NOT NULL,
  name       VARCHAR(255) NOT NULL,
  target_url TEXT NOT NULL,
  created_by INTEGER REFERENCES users(id) ON DELETE SET NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_org_agentgateway_targets_org ON org_agentgateway_targets (org_id);
