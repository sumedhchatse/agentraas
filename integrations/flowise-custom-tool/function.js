// Flowise Custom Tool function body. Runs in Flowise's sandboxed Node.js
// context — `service`, `action`, `payload` are in scope as named variables
// matching schema.json's `property` fields; `$vars` reads Flowise's
// configured Variables (Settings → Variables). Set these three there:
//   agentraasBaseUrl  - your AgentRaaS deployment (e.g. http://localhost:13000)
//   agentraasKey      - your agent's API key from AgentRaaS's Connect Agent panel
//   agentraasOrgId / agentraasAgentId - recommended; see the AgentRaaS docs
//     on why omitting them means an unenforced shared identity.
const baseUrl = ($vars.agentraasBaseUrl || 'http://localhost:13000').replace(/\/$/, '');

const headers = {
  'Content-Type': 'application/json',
  'X-AgentRaaS-Key': $vars.agentraasKey,
};
if ($vars.agentraasOrgId) headers['X-AgentRaaS-Org'] = $vars.agentraasOrgId;
if ($vars.agentraasAgentId) headers['X-AgentRaaS-Agent'] = $vars.agentraasAgentId;

let body;
try {
  body = typeof payload === 'string' ? JSON.parse(payload) : payload;
} catch (err) {
  throw new Error(`payload must be valid JSON: ${err.message}`);
}

const response = await fetch(`${baseUrl}/v1/sdk/${encodeURIComponent(service)}/${encodeURIComponent(action)}`, {
  method: 'POST',
  headers,
  body: JSON.stringify(body),
});

const data = await response.json();
if (!response.ok) {
  throw new Error(data.error || `AgentRaaS request failed (${response.status})`);
}

return JSON.stringify(data);
