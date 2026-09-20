// Integration tests for per-agent spend caps (SPEC-SPEND-CAPS.md,
// spend_caps.rs). Kept intentionally small, same shape as hitl.test.js —
// covers the two things most likely to be wrong: the block-then-reset
// counting itself, and the Community-tier gate on the "hitl" enforcement
// option (block-only caps need no tier gate at all, by design).
//
// Run with the server already up: node --test test/spend-caps.test.js

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

test('Community-tier org cannot create an on_exceed="hitl" spend-cap rule (block-only needs no tier gate)', async () => {
  const email = `spendcap-community-${RUN_ID}@internal.test`;
  const orgId = `org_spendcap_community_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const hitlRes = await client.post(
    '/api/v1/spend-cap-rules',
    { org_id: orgId, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls: 5, on_exceed: 'hitl' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(hitlRes.status, 403, JSON.stringify(hitlRes.data));

  const blockRes = await client.post(
    '/api/v1/spend-cap-rules',
    { org_id: orgId, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls: 5, on_exceed: 'block' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(blockRes.status, 200, JSON.stringify(blockRes.data));
});

test('a block rule lets exactly max_calls through, then rejects with 429', async () => {
  const email = `spendcap-block-${RUN_ID}@internal.test`;
  const orgId = `org_spendcap_block_${RUN_ID}`;
  const agentId = `agent_spendcap_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const createRes = await client.post(
    '/api/v1/spend-cap-rules',
    { org_id: orgId, agent_id: agentId, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls: 2, on_exceed: 'block' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(createRes.status, 200, JSON.stringify(createRes.data));

  const connectRes = await client.post(
    '/api/v1/agents/connect',
    { org_id: orgId, agent_id: agentId, label: 'spend cap test' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const apiKey = connectRes.data.api_key;

  const call = (amount) =>
    client.post(
      `/v1/webhook/${orgId}/${agentId}`,
      { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } },
      { headers: { Authorization: `Bearer ${apiKey}` } }
    );

  const first = await call(1);
  assert.equal(first.status, 200, JSON.stringify(first.data));
  const second = await call(2);
  assert.equal(second.status, 200, JSON.stringify(second.data));

  const third = await call(3);
  assert.equal(third.status, 429, JSON.stringify(third.data));
  assert.match(third.data.error, /Spend cap exceeded/);
});
