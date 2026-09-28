// GET /v1/sdk/whoami: the side-effect-free agent-key check integrations
// (the n8n node's credential test) call before saving a key.
// Run with the server up: node --test test/whoami.test.js

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });
const RUN_ID = Date.now();

test('whoami: 200 with the org for a live key, 401 for a missing, bogus or revoked one', async () => {
  const orgId = `org_whoami_${RUN_ID}`;
  const registerRes = await client.post('/api/v1/auth/register', { email: `whoami-${RUN_ID}@internal.test`, password: 'validpassword123', org_id: orgId });
  assert.equal(registerRes.status, 200, JSON.stringify(registerRes.data));
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  const cookie = verifyRes.headers['set-cookie'][0].split(';')[0];
  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: 'agent1' }, { headers: { Cookie: cookie } });
  const apiKey = connectRes.data.api_key;
  const whoami = (key) => client.get('/v1/sdk/whoami', { headers: key ? { 'X-AgentRaaS-Key': key } : {} });

  const ok = await whoami(apiKey);
  assert.equal(ok.status, 200, JSON.stringify(ok.data));
  assert.equal(ok.data.org_id, orgId);
  assert.equal((await whoami()).status, 401);
  assert.equal((await whoami('ar_live_notarealkey000000000000')).status, 401);

  const keys = await client.get('/api/v1/agents/keys', { headers: { Cookie: cookie } });
  const revokeRes = await client.delete(`/api/v1/agents/keys/${keys.data[0].id}`, { headers: { Cookie: cookie } });
  assert.equal(revokeRes.status, 200, JSON.stringify(revokeRes.data));
  assert.equal((await whoami(apiKey)).status, 401, 'revoked key is rejected');
});
