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

// mockpay's circuit breaker is shared by every org; the failure tests below
// would otherwise open it for the next test (same reset as dedup.test.js).
test.beforeEach(() => redis.del('circuit:mockpay'));
test.after(async () => {
  await redis.del('circuit:mockpay');
  redis.disconnect();
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
