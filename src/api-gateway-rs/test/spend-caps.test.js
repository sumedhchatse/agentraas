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

test('a free org can create both block and on_exceed="hitl" spend-cap rules', async () => {
  const email = `spendcap-free-${RUN_ID}@internal.test`;
  const orgId = `org_spendcap_free_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  for (const on_exceed of ['hitl', 'block']) {
    const res = await client.post(
      '/api/v1/spend-cap-rules',
      { org_id: orgId, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls: 5, on_exceed },
      { headers: { Cookie: sessionCookie } }
    );
    assert.equal(res.status, 200, `${on_exceed}: ${JSON.stringify(res.data)}`);
  }
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

test('a per-agent rule overrides the org-wide default; other agents still get the default', async () => {
  const email = `spendcap-precedence-${RUN_ID}@internal.test`;
  const orgId = `org_spendcap_prec_${RUN_ID}`;
  const specialAgent = `agent_special_${RUN_ID}`;
  const otherAgent = `agent_other_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const rule = (agent_id, max_calls) =>
    client.post(
      '/api/v1/spend-cap-rules',
      { org_id: orgId, agent_id, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls, on_exceed: 'block' },
      { headers: { Cookie: sessionCookie } }
    );
  // Org-wide rule created first so insertion order can't be what picks the winner.
  assert.equal((await rule(null, 1)).status, 200);
  assert.equal((await rule(specialAgent, 3)).status, 200);

  const keyFor = async (agentId) => {
    const res = await client.post(
      '/api/v1/agents/connect',
      { org_id: orgId, agent_id: agentId, label: 'spend cap precedence test' },
      { headers: { Cookie: sessionCookie } }
    );
    assert.equal(res.status, 200, JSON.stringify(res.data));
    return res.data.api_key;
  };
  const call = (agentId, apiKey, amount) =>
    client.post(
      `/v1/webhook/${orgId}/${agentId}`,
      { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } },
      { headers: { Authorization: `Bearer ${apiKey}` } }
    );

  const specialKey = await keyFor(specialAgent);
  for (const amount of [1, 2, 3]) {
    const res = await call(specialAgent, specialKey, amount);
    assert.equal(res.status, 200, `special call ${amount}: ${JSON.stringify(res.data)}`);
  }
  assert.equal((await call(specialAgent, specialKey, 4)).status, 429);

  const otherKey = await keyFor(otherAgent);
  assert.equal((await call(otherAgent, otherKey, 1)).status, 200);
  assert.equal((await call(otherAgent, otherKey, 2)).status, 429);
});

// Needs a cloud-mode server (DEPLOYMENT_MODE=cloud), since self-host ignores
// users.plan, and the enterprise build. Slack posting is best-effort, so the
// freeze works with a fake bot token; the approve click is simulated with a
// correctly signed interaction, the same thing Slack would send.
test('an on_exceed="hitl" cap freezes the over-limit call, and a signed approve releases it', async (t) => {
  const crypto = require('node:crypto');
  const { Client } = require('pg');
  const email = `spendcap-hitl-${RUN_ID}@internal.test`;
  const orgId = `org_spendcap_hitl_${RUN_ID}`;
  const agentId = `agent_spendcap_hitl_${RUN_ID}`;
  const signingSecret = `test_signing_secret_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
  try {
    await pg.query('UPDATE users SET plan = $1 WHERE org_id = $2', ['team', orgId]);
  } finally {
    await pg.end();
  }
  const auth = { headers: { Cookie: sessionCookie } };

  const ruleRes = await client.post('/api/v1/spend-cap-rules',
    { org_id: orgId, agent_id: agentId, service: 'mockpay', action: 'payment.create', window: 'hour', max_calls: 1, on_exceed: 'hitl' }, auth);
  if (ruleRes.status === 501) return t.skip('Community build: hitl routing needs --features enterprise');
  assert.equal(ruleRes.status, 200, JSON.stringify(ruleRes.data));
  const slackRes = await client.post('/api/v1/hitl-slack-config',
    { org_id: orgId, bot_token: 'xoxb-fake-test-token', signing_secret: signingSecret, default_channel: '#test' }, auth);
  assert.equal(slackRes.status, 200, JSON.stringify(slackRes.data));

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'spend cap hitl test' }, auth);
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const bearer = { headers: { Authorization: `Bearer ${connectRes.data.api_key}` } };
  const call = (amount) =>
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } }, bearer);

  assert.equal((await call(1)).status, 200);
  const frozen = await call(2);
  assert.equal(frozen.status, 202, JSON.stringify(frozen.data));
  assert.equal(frozen.data.pending_approval, true);
  const reqId = frozen.data.reqId || frozen.data.req_id;
  assert.ok(reqId, JSON.stringify(frozen.data));

  const pending = await client.get(`/v1/hitl/${reqId}`, bearer);
  assert.equal(pending.data.status, 'pending', JSON.stringify(pending.data));

  const body = new URLSearchParams({
    payload: JSON.stringify({ user: { username: 'tester' }, actions: [{ action_id: 'hitl_approve', value: reqId }] }),
  }).toString();
  const ts = String(Math.floor(Date.now() / 1000));
  const sig = 'v0=' + crypto.createHmac('sha256', signingSecret).update(`v0:${ts}:${body}`).digest('hex');
  const approveRes = await client.post(`/v1/hitl/interactions/${orgId}`, body, {
    headers: { 'Content-Type': 'application/x-www-form-urlencoded', 'X-Slack-Signature': sig, 'X-Slack-Request-Timestamp': ts },
  });
  assert.equal(approveRes.status, 200, JSON.stringify(approveRes.data));

  // Approve runs in the background (Slack's 3s ack limit); poll briefly.
  let status;
  for (let i = 0; i < 20 && status !== 'approved'; i++) {
    await new Promise((r) => setTimeout(r, 250));
    status = (await client.get(`/v1/hitl/${reqId}`, bearer)).data.status;
  }
  assert.equal(status, 'approved');
});
