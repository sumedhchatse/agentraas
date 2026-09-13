// Integration tests for the Stateful HITL Gateway's tier gating
// (ee/hitl.rs) — the first automated coverage this feature has had; up to
// now it was only manually curl-verified. Covers exactly what Task 4 of
// tasks/todo.md changed: create_rule requiring Pro+, and the freeze-point
// gate re-checking tier live (not just at rule-creation time).
//
// Run with the server already up AND ENTERPRISE_MODE=true (ar-api-rs
// always runs with this set — see compose.yaml): podman exec -it ar-api
// npm test, or node --test test/hitl.test.js directly.
//
// Plan changes go straight through Postgres (`pg`, already a dependency
// of this app) rather than the Paddle checkout flow — there's no
// "set my own plan" API by design, so this is the same shortcut this
// session's own manual verification used.

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

test('Community-tier org cannot create a HITL rule', async () => {
  const email = `hitl-community-${RUN_ID}@internal.test`;
  const orgId = `org_hitl_community_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const res = await client.post(
    '/api/v1/hitl-rules',
    { org_id: orgId, service: 'mockpay', action: 'payment.create' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(res.status, 403, JSON.stringify(res.data));
  assert.match(res.data.error, /Pro plan/);
});

test('Pro-tier org can create a rule and have it actually freeze a matching call; downgrading stops it without deleting the rule', async () => {
  const email = `hitl-pro-${RUN_ID}@internal.test`;
  const orgId = `org_hitl_pro_${RUN_ID}`;
  const agentId = `agent_hitl_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await setPlan(pg, orgId, 'pro');

  const createRes = await client.post(
    '/api/v1/hitl-rules',
    { org_id: orgId, service: 'mockpay', action: 'payment.create' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(createRes.status, 200, JSON.stringify(createRes.data));

  const connectRes = await client.post(
    '/api/v1/agents/connect',
    { org_id: orgId, agent_id: agentId, label: 'hitl test' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const apiKey = connectRes.data.api_key;

  // Still Pro: the matching call must freeze, not forward.
  const frozenRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 100, fail: false } },
    { headers: { Authorization: `Bearer ${apiKey}` } }
  );
  assert.equal(frozenRes.status, 202, JSON.stringify(frozenRes.data));
  assert.equal(frozenRes.data.pending_approval, true);

  // Downgrade — same rule, same org, no cleanup — and the next matching
  // call must forward normally instead of freezing again.
  await setPlan(pg, orgId, 'free');
  const forwardedRes = await client.post(
    `/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 101, fail: false } },
    { headers: { Authorization: `Bearer ${apiKey}` } }
  );
  assert.equal(forwardedRes.status, 200, JSON.stringify(forwardedRes.data));
  assert.equal(forwardedRes.data.forwarded, true);
});
