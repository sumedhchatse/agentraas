// MCP tool calls go through the same pipeline as webhook/SDK calls
// (agent::handle_request). These pin that every gate behaves the same over
// MCP: dedup, idempotency keys, validation, checkpoints, loop budget,
// upstream errors to the DLQ, and (since one shared pipeline) resource locks
// and the identical-response shape. Run with the server already up.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const RUN_ID = Date.now();

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, JSON.stringify(registerRes.data));
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

let orgId, agent, key, cookie;
test.before(async () => {
  orgId = `org_mcppipe_${RUN_ID}`;
  agent = `agent_mcppipe_${RUN_ID}`;
  cookie = await registerAndVerify(`mcppipe-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  key = (await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agent, label: 'mcp pipeline test' }, { headers: { Cookie: cookie } })).data.api_key;
});

let nextId = 1;
async function mcp(args, apiKey = key) {
  const res = await client.post(
    '/mcp',
    { jsonrpc: '2.0', id: nextId++, method: 'tools/call', params: { name: 'mockpay_payment_create', arguments: { org_id: orgId, agent_id: agent, ...args } } },
    { headers: { 'x-agentraas-key': apiKey } }
  );
  assert.equal(res.status, 200, JSON.stringify(res.data));
  const r = res.data.result;
  return { isError: r.isError, body: JSON.parse(r.content[0].text) };
}

test('a bad key is rejected', async () => {
  const r = await mcp({ payload: { amount: 5, fail: false } }, 'ar_live_not_a_real_key');
  assert.equal(r.isError, true);
  assert.match(r.body.error, /Invalid or missing API key/);
});

test('an identical call returns the first result instead of running again', async () => {
  const payload = { amount: 11, fail: false, n: RUN_ID };
  const first = await mcp({ payload });
  assert.equal(first.isError, false, JSON.stringify(first.body));
  const second = await mcp({ payload });
  assert.equal(second.isError, false);
  assert.equal(second.body.cached, true);
  assert.equal(second.body.id, first.body.id);
});

test('an idempotency key reused with a different payload is refused', async () => {
  const idempotency_key = `idem_${RUN_ID}`;
  assert.equal((await mcp({ idempotency_key, payload: { amount: 12, fail: false } })).isError, false);
  const reused = await mcp({ idempotency_key, payload: { amount: 13, fail: false } });
  assert.equal(reused.isError, true);
  assert.match(reused.body.error, /already used with a different payload/);
});

test('validation rules apply', async () => {
  const r = await mcp({ payload: { amount: 0, fail: false, n: `v${RUN_ID}` } });
  assert.equal(r.isError, true);
  assert.match(r.body.error, /amount/);
});

test('a completed step is served from its checkpoint', async () => {
  const run_id = `run_cp_${RUN_ID}`;
  const first = await mcp({ run_id, step_id: 'charge', payload: { amount: 21, fail: false } });
  assert.equal(first.isError, false);
  const replay = await mcp({ run_id, step_id: 'charge', payload: { amount: 22, fail: false } }); // payload differs: still the same step
  assert.equal(replay.body.checkpointed, true);
  assert.equal(replay.body.id, first.body.id);
});

test('the loop budget halts a run that repeats itself', async () => {
  const run_id = `run_loop_${RUN_ID}`;
  let last;
  for (let i = 0; i < 6; i++) last = await mcp({ run_id, payload: { amount: 30 + i, fail: false } });
  assert.equal(last.isError, true);
  assert.equal(last.body.error, 'agent_circuit_open');
});

test('a resource_id held by an in-flight call refuses a second call on it', async () => {
  const resource_id = `order_${RUN_ID}`;
  const slow = mcp({ resource_id, payload: { amount: 51, fail: false, delay_ms: 1500 } });
  await new Promise((r) => setTimeout(r, 300));
  const second = await mcp({ resource_id, payload: { amount: 52, fail: false } });
  assert.equal(second.isError, true, JSON.stringify(second.body));
  assert.match(second.body.error, /resource_id/);
  assert.equal((await slow).isError, false);
  const after = await mcp({ resource_id, payload: { amount: 53, fail: false } });
  assert.equal(after.isError, false, 'the lock is released when the first call finishes');
});

// Last: failures trip mockpay's circuit breaker (shared by every org).
test('an upstream failure is an error, goes to the DLQ, and frees the slot', async () => {
  const payload = { amount: 41, fail: true, n: RUN_ID };
  const failed = await mcp({ payload });
  assert.equal(failed.isError, true);
  assert.match(failed.body.error, /MockPay temporarily unavailable/);
  const dlq = await client.get(`/api/v1/dead-letter-queue?org_id=${orgId}`, { headers: { Cookie: cookie } });
  const entries = Array.isArray(dlq.data) ? dlq.data : dlq.data.entries || dlq.data.items || [];
  assert.ok(entries.some((e) => (e.req_id || e.reqId) === failed.body.reqId), `DLQ has ${failed.body.reqId}: ${JSON.stringify(dlq.data).slice(0, 300)}`);
  const retried = await mcp({ payload });
  assert.equal(retried.isError, true, 'a definite failure is retried for real, not served from cache');
  assert.notEqual(retried.body.cached, true);
});
