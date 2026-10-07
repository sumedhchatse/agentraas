// Undo log (crates/api/src/undo.rs): an action with a known reverse is
// recorded once it ran and can be undone once, by a member of its org.
// mockpay payment.create's reverse is payment.refund (config/services.json).
// Needs migration 054. Run with the server already up.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const RUN_ID = Date.now();

async function registerAndVerify(email, orgId) {
  const reg = await client.post('/api/v1/auth/register', { email, password: 'validpassword123', org_id: orgId });
  assert.equal(reg.status, 200, JSON.stringify(reg.data));
  const token = new URL(reg.data.dev_verify_url).searchParams.get('verify_token');
  const res = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  return res.headers['set-cookie'][0].split(';')[0];
}

let pg, orgId, agentId, cookie, key;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
  orgId = `org_undo_${RUN_ID}`;
  agentId = `agent_undo_${RUN_ID}`;
  cookie = await registerAndVerify(`undo-${RUN_ID}@internal.test`, orgId);
  key = (await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'undo test' }, { headers: { Cookie: cookie } })).data.api_key;
});
test.after(async () => { await pg.end(); });

const pay = async (amount) => {
  const res = await client.post(`/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } },
    { headers: { Authorization: `Bearer ${key}` } });
  assert.equal(res.status, 200, JSON.stringify(res.data));
  return res.data;
};
const entries = async (who = cookie, org = orgId) => client.get(`/api/v1/undo-log?org_id=${org}`, { headers: { Cookie: who } });
async function entryFor(reqId) {
  for (let i = 0; i < 50; i++) {
    const e = (await entries()).data.entries.find((x) => x.req_id === reqId);
    if (e) return e;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert.fail(`no undo entry for ${reqId}`);
}
const undo = (id, who = cookie) => client.post(`/api/v1/undo-log/${id}/undo`, {}, { headers: { Cookie: who } });

test('a payment that ran is undoable, undoing refunds it once, and is logged', async () => {
  const charge = await pay(11);
  const entry = await entryFor(charge.reqId);
  assert.equal(entry.status, 'available');
  assert.equal(entry.undo_action, 'payment.refund');
  assert.deepEqual(entry.undo_payload, { payment: charge.upstream_response.id });

  const res = await undo(entry.id);
  assert.equal(res.status, 200, JSON.stringify(res.data));
  assert.equal(res.data.result.upstream_response.payment, charge.upstream_response.id);
  assert.equal(res.data.result.upstream_response.status, 'refunded');
  assert.equal((await entryFor(charge.reqId)).status, 'undone');

  const again = await undo(entry.id);
  assert.equal(again.status, 409, 'a second undo is refused');
  assert.match(again.data.error, /undone/);

  const { rows } = await pg.query("SELECT status FROM audit_log WHERE req_id = $1", [`undo_${entry.id}`]);
  assert.deepEqual(rows.map((r) => r.status), ['undo'], 'the undo is in the audit log, not billed as a success');
});

test('two simultaneous undos run the reverse once', async () => {
  const charge = await pay(12);
  const entry = await entryFor(charge.reqId);
  const results = await Promise.all([undo(entry.id), undo(entry.id), undo(entry.id)]);
  const codes = results.map((r) => r.status).sort();
  assert.deepEqual(codes, [200, 409, 409], JSON.stringify(results.map((r) => r.data)));
});

test('another org can neither see nor undo it', async () => {
  const charge = await pay(13);
  const entry = await entryFor(charge.reqId);
  const otherCookie = await registerAndVerify(`undo-other-${RUN_ID}@internal.test`, `org_undo_other_${RUN_ID}`);
  assert.equal((await entries(otherCookie)).status, 403);
  assert.equal((await undo(entry.id, otherCookie)).status, 404);
  assert.equal((await entryFor(charge.reqId)).status, 'available');
});

test('an MCP call is recorded the same way (one pipeline)', async () => {
  const res = await client.post('/mcp',
    { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'mockpay_payment_create', arguments: { org_id: orgId, agent_id: agentId, payload: { amount: 14, fail: false } } } },
    { headers: { 'x-agentraas-key': key } });
  const body = JSON.parse(res.data.result.content[0].text);
  assert.equal(res.data.result.isError, false, JSON.stringify(body));
  const entry = await entryFor(body.reqId);
  assert.equal(entry.undo_payload.payment, body.upstream_response.id);
});

test('a duplicate answered from cache, or a failed call, leaves nothing to undo', async () => {
  const first = await pay(16);
  const dup = await pay(16);
  assert.equal(dup.cached, true);
  await entryFor(first.reqId);
  // Last: a failure can open mockpay's breaker for the calls after it.
  const failed = await client.post(`/v1/webhook/${orgId}/${agentId}`,
    { service: 'mockpay', action: 'payment.create', payload: { amount: 15, fail: true } },
    { headers: { Authorization: `Bearer ${key}` } });
  assert.notEqual(failed.status, 200);
  await new Promise((r) => setTimeout(r, 500));
  const all = (await entries()).data.entries;
  assert.ok(!all.some((e) => e.req_id === dup.reqId), 'no entry for a duplicate answered from cache');
  assert.ok(!all.some((e) => e.req_id === failed.data.reqId), 'no entry for a failed call');
});
