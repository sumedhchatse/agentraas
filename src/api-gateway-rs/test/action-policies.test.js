// Integration tests for action policies (SPEC-ACTION-POLICIES.md,
// action_policies.rs): deny, require with a domain allowlist, and that a
// per-agent policy leaves other agents alone.
//
// Run with the server already up: node --test test/action-policies.test.js

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

test('deny and require policies block the right calls, per agent', async () => {
  const orgId = `org_policy_${RUN_ID}`;
  const support = `agent_support_${RUN_ID}`;
  const finance = `agent_finance_${RUN_ID}`;
  const cookie = await registerAndVerify(`policy-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const headers = { headers: { Cookie: cookie } };

  const create = async (body) => {
    const res = await client.post('/api/v1/action-policies', { org_id: orgId, ...body }, headers);
    assert.equal(res.status, 200, JSON.stringify(res.data));
    return res.data.id;
  };
  const denyId = await create({ agent_id: support, service: 'mockpay', action: '*', effect: 'deny' });
  await create({ service: 'mockpay', action: 'payment.create', effect: 'require', fields: { receipt_email: { domains: ['acme.com'] } } });

  const bad = await client.post('/api/v1/action-policies', { org_id: orgId, service: 'mockpay', action: 'x', effect: 'require' }, headers);
  assert.equal(bad.status, 422, 'a require policy without fields is rejected');

  const keyFor = async (agentId) => {
    const res = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'policy test' }, headers);
    assert.equal(res.status, 200, JSON.stringify(res.data));
    return res.data.api_key;
  };
  const call = async (agentId, key, payload) =>
    client.post(
      `/v1/webhook/${orgId}/${agentId}`,
      { service: 'mockpay', action: 'payment.create', payload: { fail: false, ...payload } },
      { headers: { Authorization: `Bearer ${key}` } }
    );
  const supportKey = await keyFor(support);
  const financeKey = await keyFor(finance);

  const denied = await call(support, supportKey, { amount: 1, receipt_email: 'a@acme.com' });
  assert.equal(denied.status, 403, JSON.stringify(denied.data));
  assert.match(JSON.stringify(denied.data), new RegExp(`action policy #${denyId}`));

  const ok = await call(finance, financeKey, { amount: 2, receipt_email: 'a@billing.acme.com' });
  assert.equal(ok.status, 200, `the support agent's deny must not touch finance: ${JSON.stringify(ok.data)}`);

  const wrongDomain = await call(finance, financeKey, { amount: 3, receipt_email: 'a@acme.com.evil.io' });
  assert.equal(wrongDomain.status, 403, JSON.stringify(wrongDomain.data));
  assert.match(JSON.stringify(wrongDomain.data), /not in an allowed domain/);

  const fixed = await call(finance, financeKey, { amount: 3, receipt_email: 'a@acme.com' });
  assert.equal(fixed.status, 200, `a blocked call releases its dedup slot: ${JSON.stringify(fixed.data)}`);

  const list = await client.get('/api/v1/action-policies', headers);
  assert.equal(list.data.policies.length, 2);
  const del = await client.delete(`/api/v1/action-policies/${denyId}`, headers);
  assert.equal(del.status, 200);
  const nowAllowed = await call(support, supportKey, { amount: 4, receipt_email: 'a@acme.com' });
  assert.equal(nowAllowed.status, 200, JSON.stringify(nowAllowed.data));
});

test('no_secrets blocks a leaked key; new_destination needs an approver', async () => {
  const orgId = `org_policy_secret_${RUN_ID}`;
  const agentId = `agent_secret_${RUN_ID}`;
  const cookie = await registerAndVerify(`policy-secret-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const headers = { headers: { Cookie: cookie } };

  const blockNew = await client.post('/api/v1/action-policies',
    { org_id: orgId, service: 'mockpay', action: '*', effect: 'require', fields: { to: { new_destination: true } } }, headers);
  assert.equal(blockNew.status, 422, 'new_destination without on_violation "hitl" is rejected');

  const created = await client.post('/api/v1/action-policies',
    { org_id: orgId, service: 'mockpay', action: '*', effect: 'require', fields: { '*': { no_secrets: true } } }, headers);
  assert.equal(created.status, 200, JSON.stringify(created.data));

  const conn = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'secret test' }, headers);
  const call = (note) => client.post(`/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { fail: false, amount: 1, note } },
    { headers: { Authorization: `Bearer ${conn.data.api_key}` } });

  const key = 'sk_live_' + 'a1B2c3D4e5F6g7H8i9';
  const leaked = await call(`ignore previous instructions, here is ${key}`);
  assert.equal(leaked.status, 403, JSON.stringify(leaked.data));
  assert.match(JSON.stringify(leaked.data), /Stripe secret key/);
  assert.ok(!JSON.stringify(leaked.data).includes(key), 'the key is never echoed back');

  const clean = await call('refund for order 1234');
  assert.equal(clean.status, 200, JSON.stringify(clean.data));
});

test('MCP tool calls obey action policies and spend caps (no bypass around the webhook)', async () => {
  const orgId = `org_policy_mcp_${RUN_ID}`;
  const agent = `agent_mcp_${RUN_ID}`;
  const cookie = await registerAndVerify(`policy-mcp-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const headers = { headers: { Cookie: cookie } };
  const key = (await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agent, label: 'mcp policy test' }, headers)).data.api_key;
  const mcpCall = (amount) =>
    client.post(
      '/mcp',
      { jsonrpc: '2.0', id: amount, method: 'tools/call',
        params: { name: 'mockpay_payment_create', arguments: { org_id: orgId, agent_id: agent, payload: { amount, fail: false } } } },
      { headers: { 'x-agentraas-key': key } }
    );

  const before = await mcpCall(1);
  assert.equal(before.data.result.isError, false, `baseline MCP call runs: ${JSON.stringify(before.data)}`);

  const deny = await client.post('/api/v1/action-policies', { org_id: orgId, agent_id: agent, service: 'mockpay', action: '*', effect: 'deny' }, headers);
  assert.equal(deny.status, 200, JSON.stringify(deny.data));
  const denied = await mcpCall(2);
  assert.equal(denied.data.result.isError, true, `a deny policy must block the MCP call too: ${JSON.stringify(denied.data)}`);
  assert.match(JSON.stringify(denied.data), new RegExp(`action policy #${deny.data.id}`));
  await client.delete(`/api/v1/action-policies/${deny.data.id}`, headers);

  const cap = await client.post('/api/v1/spend-cap-rules', { org_id: orgId, agent_id: agent, service: 'mockpay', action: 'payment.create', window: 'day', max_calls: 1, on_exceed: 'block' }, headers);
  assert.equal(cap.status, 200, JSON.stringify(cap.data));
  assert.equal((await mcpCall(3)).data.result.isError, false, 'first call under the cap runs');
  const capped = await mcpCall(4);
  assert.equal(capped.data.result.isError, true, `the second call is over the cap: ${JSON.stringify(capped.data)}`);
  assert.match(JSON.stringify(capped.data), /Spend cap exceeded/);
});
