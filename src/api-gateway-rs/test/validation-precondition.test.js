// Integration test for the maxField/minField validation-rule precondition
// (agentraas_core::validator) — a field can be checked against another
// field's value in the SAME payload (e.g. "refund_amount must not exceed
// balance"), not just a fixed literal threshold.
//
// Run with the server already up: podman exec -it ar-api-rs npm test,
// or node --test test/validation-precondition.test.js directly.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });

const RUN_ID = Date.now();

test('maxField rejects a payload where the field exceeds the referenced field, allows it when within range', async () => {
  const email = `precond-${RUN_ID}@internal.test`;
  const orgId = `org_precond_${RUN_ID}`;
  const agentId = 'agent1';

  const registerRes = await client.post('/api/v1/auth/register', { email, password: 'validpassword123', org_id: orgId });
  assert.equal(registerRes.status, 200, JSON.stringify(registerRes.data));
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  const sessionCookie = verifyRes.headers['set-cookie'][0].split(';')[0];

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId }, { headers: { Cookie: sessionCookie } });
  const apiKey = connectRes.data.api_key;

  const ruleRes = await client.post('/api/v1/validation-rules', {
    org_id: orgId, service: 'mockpay', action: 'payment.create',
    fields: { amount: { type: 'number', maxField: 'balance' } },
  }, { headers: { Cookie: sessionCookie } });
  assert.equal(ruleRes.status, 200, JSON.stringify(ruleRes.data));

  const overBalance = await client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 500, balance: 100, fail: false } }, { headers: { Authorization: `Bearer ${apiKey}` } });
  assert.equal(overBalance.status, 422, JSON.stringify(overBalance.data));
  assert.match(overBalance.data.error, /amount.*balance/);

  const withinBalance = await client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount: 50, balance: 100, fail: false } }, { headers: { Authorization: `Bearer ${apiKey}` } });
  assert.equal(withinBalance.status, 200, JSON.stringify(withinBalance.data));
});
