// Integration tests for the upstream Idempotency-Key and the outcome-unknown
// path (agent/forward.rs `outcome_unknown`, core dedup `mark_dedup_slot_unknown`).
//
// Needs the server running with a short forward timeout, so a mock call can
// outlast it: PROXY_TIMEOUT_SECONDS=2 in .env (the mock's delay_ms caps at 10s).
// Run: node --test test/outcome-unknown.test.js

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const Redis = require('ioredis');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });
const RUN_ID = Date.now();
const redis = new Redis(process.env.REDIS_URL || 'redis://localhost:6379');
const { Pool } = require('pg');
const pg = new Pool({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
const pgQuery = (sql, params) => pg.query(sql, params);

// mockpay's circuit breaker is shared by every org; the failure tests below
// would otherwise open it for the next test (same reset as dedup.test.js).
test.beforeEach(() => redis.del('circuit:mockpay'));
test.after(async () => {
  await redis.del('circuit:mockpay');
  redis.disconnect();
  await pg.end();
});

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, `Expected registration to succeed: ${JSON.stringify(registerRes.data)}`);
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

async function agent(name) {
  const orgId = `org_ou_${name}_${RUN_ID}`;
  const agentId = `agent_ou_${name}_${RUN_ID}`;
  const cookie = await registerAndVerify(`ou-${name}-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const res = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'outcome test' }, { headers: { Cookie: cookie } });
  assert.equal(res.status, 200, JSON.stringify(res.data));
  const call = (payload) =>
    client.post(`/v1/webhook/${orgId}/${agentId}`, { service: 'mockpay', action: 'payment.create', payload }, { headers: { Authorization: `Bearer ${res.data.api_key}` } });
  return { call, cookie };
}

test('every forward carries one stable Idempotency-Key per logical action', async () => {
  const { call } = await agent('key');
  const first = await call({ amount: 5, fail: false });
  assert.equal(first.status, 200, JSON.stringify(first.data));
  assert.match(first.data.upstream_response.idempotency_key, /^agentraas-.+/);

  const other = await call({ amount: 6, fail: false });
  assert.equal(other.status, 200);
  assert.notEqual(other.data.upstream_response.idempotency_key, first.data.upstream_response.idempotency_key, 'a different action gets a different key');
});

test('a timeout is not retried, keeps the slot as unknown, and lands in the DLQ', async () => {
  const { call, cookie } = await agent('timeout');
  const payload = { amount: 7, fail: false, delay_ms: 6000 };

  const started = Date.now();
  const res = await call(payload);
  const elapsed = Date.now() - started;
  assert.equal(res.status, 504, JSON.stringify(res.data));
  assert.equal(res.data.outcome, 'unknown');
  // one attempt at a 2s timeout; three attempts would take well over 6s
  assert.ok(elapsed < 5000, `took ${elapsed}ms: looks like it was retried`);

  const again = await call(payload);
  assert.equal(again.status, 409, JSON.stringify(again.data));
  assert.match(again.data.error, /outcome is unknown/);

  const dlq = await client.get('/api/v1/dead-letter-queue', { headers: { Cookie: cookie } });
  assert.equal(dlq.status, 200);
  assert.ok(dlq.data.some((e) => e.req_id === res.data.reqId), 'the unknown call is in the dead-letter queue');
});

test('a definite upstream failure still frees the slot for a real retry', async () => {
  const { call } = await agent('definite');
  const payload = { amount: 8, fail: true };
  const first = await call(payload);
  assert.equal(first.status, 500, JSON.stringify(first.data));
  const second = await call(payload);
  assert.equal(second.status, 500, 'retried for real, not blocked as a duplicate');
});

test('a call left in flight by a dead process becomes outcome-unknown once, on the next copy', async () => {
  const { call, cookie } = await agent('stale');
  const payload = { amount: 11, fail: true };
  // a definite failure records the call's dedup hash in the DLQ and frees the slot
  const failed = await call(payload);
  assert.equal(failed.status, 500, JSON.stringify(failed.data));
  const dlq = (await client.get('/api/v1/dead-letter-queue', { headers: { Cookie: cookie } })).data;
  const entry = dlq.find((e) => e.req_id === failed.data.reqId);
  assert.ok(entry, 'failure is in the DLQ');
  const { rows } = await pgQuery('SELECT dedup_hash FROM dead_letter_queue WHERE id = $1', [entry.id]);
  const key = `dedup:${rows[0].dedup_hash}`;

  // what a process that died mid-call leaves behind: pending, lease long gone
  await redis.set(key, JSON.stringify({ pending: true, reqId: 'req_dead_worker', leaseUntil: 1 }), 'EX', 3600);

  const first = await call(payload);
  assert.equal(first.status, 409, JSON.stringify(first.data));
  assert.match(first.data.error, /outcome is unknown/);
  assert.equal(JSON.parse(await redis.get(key)).outcome_unknown, true);

  const second = await call(payload);
  assert.equal(second.status, 409);
  const { rows: dead } = await pgQuery("SELECT error_message FROM dead_letter_queue WHERE req_id = 'req_dead_worker' AND dedup_hash = $1", [rows[0].dedup_hash]);
  assert.equal(dead.length, 1, 'recorded once, not once per copy');
  assert.match(dead[0].error_message, /^outcome unknown: /);
});

test('over MCP too: a timed-out call is outcome unknown and an identical retry is not run again', async () => {
  const orgId = `org_ou_mcp_${RUN_ID}`;
  const agentId = `agent_ou_mcp_${RUN_ID}`;
  const cookie = await registerAndVerify(`ou-mcp-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const key = (await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'outcome mcp' }, { headers: { Cookie: cookie } })).data.api_key;
  const mcpCall = async () => {
    const res = await client.post('/mcp',
      { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'mockpay_payment_create', arguments: { org_id: orgId, agent_id: agentId, payload: { amount: 77, fail: false, delay_ms: 4000 } } } },
      { headers: { 'x-agentraas-key': key } });
    return { isError: res.data.result.isError, body: JSON.parse(res.data.result.content[0].text) };
  };
  const first = await mcpCall();
  assert.equal(first.isError, true);
  assert.equal(first.body.outcome, 'unknown', JSON.stringify(first.body));
  const retry = await mcpCall();
  assert.equal(retry.isError, true);
  assert.match(retry.body.error, /outcome is unknown, so it is not being run again/);
  const { rows } = await pgQuery('SELECT error_message FROM dead_letter_queue WHERE org_id = $1', [orgId]);
  assert.ok(rows.some((r) => r.error_message.startsWith('outcome unknown: ')), JSON.stringify(rows));
});
