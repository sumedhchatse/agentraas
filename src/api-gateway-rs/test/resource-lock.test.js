// Integration tests for the cross-agent resource lock (agentraas_core::
// resource_lock) — a different concern from dedup: dedup catches a retry
// of the SAME request, this catches two DIFFERENT requests racing to act
// on the same declared resource_id at the same time.
//
// Run with the server already up: podman exec -it ar-api-rs npm test,
// or node --test test/resource-lock.test.js directly.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

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

test('two concurrent requests naming the same resource_id: one proceeds, one is blocked', async () => {
  const email = `reslock-${RUN_ID}@internal.test`;
  const orgId = `org_reslock_${RUN_ID}`;
  const agentId = 'agent1';
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId }, { headers: { Cookie: sessionCookie } });
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const apiKey = connectRes.data.api_key;

  // Different payloads so dedup (which hashes the whole payload) doesn't
  // catch these as the same request — resource_id is the only thing tying
  // them together, which is exactly the scenario this feature covers.
  const [r1, r2] = await Promise.all([
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 100, fail: false }, resource_id: 'cus_123' }, { headers: { Authorization: `Bearer ${apiKey}` } }),
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 200, fail: false }, resource_id: 'cus_123' }, { headers: { Authorization: `Bearer ${apiKey}` } }),
  ]);
  const statuses = [r1.status, r2.status].sort();
  assert.deepEqual(statuses, [200, 409], `Expected one 200 and one 409, got ${JSON.stringify(statuses)}`);
  const blocked = r1.status === 409 ? r1 : r2;
  assert.match(blocked.data.error, /resource_id/);
});

test('different resource_id values are not blocked by each other', async () => {
  const email = `reslock-diff-${RUN_ID}@internal.test`;
  const orgId = `org_reslock_diff_${RUN_ID}`;
  const agentId = 'agent1';
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId }, { headers: { Cookie: sessionCookie } });
  const apiKey = connectRes.data.api_key;

  const [r1, r2] = await Promise.all([
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 300, fail: false }, resource_id: 'cus_A' }, { headers: { Authorization: `Bearer ${apiKey}` } }),
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 400, fail: false }, resource_id: 'cus_B' }, { headers: { Authorization: `Bearer ${apiKey}` } }),
  ]);
  assert.equal(r1.status, 200, JSON.stringify(r1.data));
  assert.equal(r2.status, 200, JSON.stringify(r2.data));
});

test('omitting resource_id entirely is unaffected (purely additive)', async () => {
  const email = `reslock-none-${RUN_ID}@internal.test`;
  const orgId = `org_reslock_none_${RUN_ID}`;
  const agentId = 'agent1';
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId }, { headers: { Cookie: sessionCookie } });
  const apiKey = connectRes.data.api_key;

  const [r1, r2] = await Promise.all([
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 500, fail: false } }, { headers: { Authorization: `Bearer ${apiKey}` } }),
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 600, fail: false } }, { headers: { Authorization: `Bearer ${apiKey}` } }),
  ]);
  assert.equal(r1.status, 200, JSON.stringify(r1.data));
  assert.equal(r2.status, 200, JSON.stringify(r2.data));
});
