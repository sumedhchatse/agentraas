// Integration tests for Agent Identity (ee/identity.rs) — short-lived,
// scope-restricted tokens as an alternative to the long-lived api_keys
// credential, enforced in handle_request (agent/mod.rs).
//
// Run with the server already up AND ENTERPRISE_MODE=true:
// podman exec -it ar-api-rs npm test, or node --test test/identity.test.js
// directly (needs DATABASE_URL / TEST_BASE_URL like the other *.test.js
// files in this directory).

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });

const RUN_ID = Date.now();

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, `Expected registration to succeed: ${JSON.stringify(registerRes.data)}`);
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

async function setPlan(pg, orgId, plan) {
  await pg.query('UPDATE users SET plan = $1 WHERE org_id = $2', [plan, orgId]);
}

let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});

test('Community-tier org cannot mint an agent identity token', async () => {
  const email = `identity-community-${RUN_ID}@internal.test`;
  const orgId = `org_identity_community_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const res = await client.post(
    '/api/v1/agent-identity-tokens',
    { org_id: orgId, agent_id: 'agent1', scopes: [{ service: 'mockpay', action: 'payment.create' }], ttl_seconds: 3600 },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(res.status, 403, JSON.stringify(res.data));
  assert.match(res.data.error, /Enterprise plan/);
});

test('scoped token: allows the scoped action, denies an unscoped one, and a normal api_key is unaffected', async () => {
  const email = `identity-ent-${RUN_ID}@internal.test`;
  const orgId = `org_identity_ent_${RUN_ID}`;
  const agentId = 'agent1';
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await setPlan(pg, orgId, 'enterprise');

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId }, { headers: { Cookie: sessionCookie } });
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const longLivedKey = connectRes.data.api_key;

  const issueRes = await client.post(
    '/api/v1/agent-identity-tokens',
    { org_id: orgId, agent_id: agentId, scopes: [{ service: 'mockpay', action: 'payment.create' }], ttl_seconds: 3600 },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(issueRes.status, 200, JSON.stringify(issueRes.data));
  const scopedToken = issueRes.data.token;
  assert.match(scopedToken, /^art_live_/);

  // "Agent Passport" framing: minting returns identity + the reliability
  // guarantees already in effect for these scopes, not just a bare token.
  assert.equal(issueRes.data.passport.identity.org_id, orgId);
  assert.equal(issueRes.data.passport.identity.agent_id, agentId);
  assert.equal(typeof issueRes.data.passport.reliability.rate_limit_per_minute, 'number');
  assert.equal(issueRes.data.passport.reliability.dedup[0].service, 'mockpay');
  assert.equal(issueRes.data.passport.reliability.dedup[0].mode, 'whole-payload');

  // In-scope call succeeds.
  const okRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 100, fail: false } },
    { headers: { Authorization: `Bearer ${scopedToken}` } }
  );
  assert.equal(okRes.status, 200, JSON.stringify(okRes.data));

  // Out-of-scope action (different service entirely) on the same token is
  // rejected, not silently forwarded.
  const forbiddenRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'stripe', action: 'customer.create', payload: {} },
    { headers: { Authorization: `Bearer ${scopedToken}` } }
  );
  assert.equal(forbiddenRes.status, 403, JSON.stringify(forbiddenRes.data));

  // A token minted for this agent cannot be replayed against a different agent_id.
  const wrongAgentRes = await client.post(
    `/v1/webhook/${orgId}/other-agent`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 1, fail: false } },
    { headers: { Authorization: `Bearer ${scopedToken}` } }
  );
  assert.equal(wrongAgentRes.status, 401, JSON.stringify(wrongAgentRes.data));

  // Revoking the token blocks further use immediately.
  const listRes = await client.get('/api/v1/agent-identity-tokens', { headers: { Cookie: sessionCookie } });
  assert.equal(listRes.status, 200);
  const tokenId = listRes.data.tokens.find((t) => t.org_id === orgId && t.agent_id === agentId).id;
  const revokeRes = await client.delete(`/api/v1/agent-identity-tokens/${tokenId}`, { headers: { Cookie: sessionCookie } });
  assert.equal(revokeRes.status, 200, JSON.stringify(revokeRes.data));

  const afterRevokeRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 1, fail: false } },
    { headers: { Authorization: `Bearer ${scopedToken}` } }
  );
  assert.equal(afterRevokeRes.status, 401, JSON.stringify(afterRevokeRes.data));

  // The original long-lived api_key still works unchanged throughout —
  // scoping is exclusive to art_live_ tokens.
  const legacyRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'stripe', action: 'customer.create', payload: {} },
    { headers: { Authorization: `Bearer ${longLivedKey}` } }
  );
  assert.notEqual(legacyRes.status, 401);
  assert.notEqual(legacyRes.status, 403);
});
