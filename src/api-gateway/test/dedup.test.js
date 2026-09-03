// Integration tests for AgentRaaS's core value proposition: exactly-once execution.
//
// These fire real HTTP requests at a RUNNING server (not mocks) — real Redis,
// real Postgres, real race conditions — because the dedup logic's whole job is
// to survive genuine concurrency, and a mocked unit test wouldn't prove that.
//
// Run with the server already up:
//   podman exec -it ar-api npm test
// (defaults to http://localhost:3000 — correct when run *inside* the container,
// which is what the command above does. Override with TEST_BASE_URL if needed.)
//
// Uses the built-in `mockpay` internal service, which exists specifically for
// safe testing — it never calls a real third party, and supports an explicit
// `fail: true` flag for deterministically testing the failure path.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const Redis = require('ioredis');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const TEST_EMAIL = 'dedup-test@internal.test';
const TEST_PASSWORD = 'dedup-test-password-123';
const TEST_ORG = 'org_dedup_test';
const TEST_AGENT = 'agent_dedup_test';

const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });
const redis = new Redis(process.env.REDIS_URL || 'redis://localhost:6379');

let sessionCookie;
let apiKey;

// Registration no longer logs in directly — it requires email verification
// first. With no SMTP configured (the state these tests run in), the verify
// link comes back directly in the response instead of only being emailed —
// same fallback a self-hoster without email set up would get. This walks
// the real flow end-to-end rather than bypassing it, and is robust to
// re-running the suite against an account left in any state by a prior run.
async function registerAndVerify(email, password) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password });
  if (registerRes.status === 200 && registerRes.data.dev_verify_url) {
    const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
    const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
    const setCookie = verifyRes.headers['set-cookie'];
    if (verifyRes.status === 200 && setCookie?.length > 0) return setCookie[0].split(';')[0];
  }

  // Account already existed from a previous run — try logging in.
  const loginRes = await client.post('/api/v1/auth/login', { email, password });
  if (loginRes.status === 200) {
    const setCookie = loginRes.headers['set-cookie'];
    if (setCookie?.length > 0) return setCookie[0].split(';')[0];
  }

  // Existing but never-verified account (e.g. a prior run crashed mid-test) —
  // resend and verify.
  if (loginRes.status === 403 && loginRes.data.code === 'EMAIL_NOT_VERIFIED') {
    const resendRes = await client.post('/api/v1/auth/resend-verification', { email });
    if (resendRes.data.dev_verify_url) {
      const token = new URL(resendRes.data.dev_verify_url).searchParams.get('verify_token');
      const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
      const setCookie = verifyRes.headers['set-cookie'];
      if (verifyRes.status === 200 && setCookie?.length > 0) return setCookie[0].split(';')[0];
    }
  }

  throw new Error(`Could not register/verify/log in as ${email}`);
}

// ─── Shared setup: get a logged-in session and a real API key, once ───
test('setup: register or log in, then connect a test agent', async () => {
  // Reset mockpay's circuit breaker before anything else runs — accumulated
  // failures from earlier test runs (or the ~10% random failures mockpay
  // itself injects) can leave it open, which would then interfere with
  // every other test in this file, not just the one that deliberately
  // triggers failures.
  await redis.del('circuit:mockpay');

  sessionCookie = await registerAndVerify(TEST_EMAIL, TEST_PASSWORD);

  const connectRes = await client.post(
    '/api/v1/agents/connect',
    { org_id: TEST_ORG, agent_id: TEST_AGENT, label: 'automated dedup test' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(connectRes.status, 200, `Expected agent connect to succeed, got ${connectRes.status}: ${JSON.stringify(connectRes.data)}`);
  apiKey = connectRes.data.api_key;
  assert.ok(apiKey && apiKey.startsWith('ar_live_'), 'Expected a real API key back');
});

function callWebhook(payload) {
  return client.post(`/v1/webhook/${TEST_ORG}/${TEST_AGENT}`, payload, {
    headers: { Authorization: `Bearer ${apiKey}` },
  });
}

test('concurrent identical requests execute exactly once', async () => {
  const payload = {
    service: 'mockpay',
    action: 'payment.create',
    // Unique per test run so this test doesn't collide with leftover dedup
    // state from a previous run within the same 24h TTL window.
    payload: { amount: 1000, fail: false, idempotency_probe: `concurrent-${Date.now()}` },
  };

  const CONCURRENCY = 8;
  const responses = await Promise.all(Array.from({ length: CONCURRENCY }, () => callWebhook(payload)));

  // Every response must be one of: a genuine fresh execution (200, forwarded,
  // not cached), a cached replay of that execution (200, cached:true), or a
  // "duplicate in progress" rejection (409) — never a second real execution.
  const freshExecutions = responses.filter((r) => r.status === 200 && r.data.forwarded === true && !r.data.cached);
  const cachedReplays = responses.filter((r) => r.status === 200 && r.data.cached === true);
  const inProgressRejections = responses.filter((r) => r.status === 409);
  const unexpected = responses.filter((r) =>
    !(r.status === 200 && (r.data.forwarded === true || r.data.cached === true)) && r.status !== 409
  );

  assert.equal(unexpected.length, 0, `Got unexpected responses: ${JSON.stringify(unexpected.map(r => ({status: r.status, data: r.data})))}`);
  assert.equal(freshExecutions.length, 1, `Expected exactly 1 fresh execution among ${CONCURRENCY} concurrent identical requests, got ${freshExecutions.length}`);
  assert.equal(freshExecutions.length + cachedReplays.length + inProgressRejections.length, CONCURRENCY, 'Every response should be accounted for as fresh, cached, or in-progress');
});

test('sequential duplicate (after completion) returns the cached result, not a fresh execution', async () => {
  const payload = {
    service: 'mockpay',
    action: 'payment.create',
    payload: { amount: 2500, fail: false, idempotency_probe: `sequential-${Date.now()}` },
  };

  const first = await callWebhook(payload);
  assert.equal(first.status, 200);
  assert.equal(first.data.forwarded, true);
  assert.ok(!first.data.cached, 'First request should be a fresh execution, not cached');

  const second = await callWebhook(payload);
  assert.equal(second.status, 200);
  assert.equal(second.data.cached, true, 'A repeat of a completed request should return the cached result');
  assert.equal(
    second.data.upstream_id,
    first.data.upstream_id,
    'Cached replay should return the exact same upstream result, not a new one'
  );
});

test('different payloads are NOT deduplicated against each other', async () => {
  const probe = Date.now();
  const a = await callWebhook({ service: 'mockpay', action: 'payment.create', payload: { amount: 100, fail: false, idempotency_probe: `distinct-a-${probe}` } });
  const b = await callWebhook({ service: 'mockpay', action: 'payment.create', payload: { amount: 200, fail: false, idempotency_probe: `distinct-b-${probe}` } });

  assert.equal(a.status, 200);
  assert.equal(b.status, 200);
  assert.ok(!a.data.cached, 'Distinct payload A should execute fresh');
  assert.ok(!b.data.cached, 'Distinct payload B should execute fresh');
  assert.notEqual(a.data.upstream_id, b.data.upstream_id, 'Distinct requests must not share a result');
});

test('a failed request releases its dedup slot, so an identical retry executes fresh (not blocked as in-progress)', async () => {
  const payload = {
    service: 'mockpay',
    action: 'payment.create',
    // fail:true makes mockpay's failure deterministic instead of relying on its ~10% random chance.
    payload: { amount: 500, fail: true, idempotency_probe: `retry-${Date.now()}` },
  };

  const first = await client.post(`/v1/webhook/${TEST_ORG}/${TEST_AGENT}`, payload, {
    headers: { Authorization: `Bearer ${apiKey}` },
  });
  assert.equal(first.status, 500, 'mockpay with fail:true should return an upstream error');
  assert.equal(first.data.error, 'MockPay temporarily unavailable');

  // Retry with the EXACT same payload. If the failed attempt's dedup slot wasn't
  // released, this would come back as 409 "duplicate in progress" instead of
  // executing fresh — that's precisely the bug the fix was for.
  const second = await client.post(`/v1/webhook/${TEST_ORG}/${TEST_AGENT}`, payload, {
    headers: { Authorization: `Bearer ${apiKey}` },
  });
  assert.equal(second.status, 500, 'Retry should execute fresh and hit the same deterministic failure again, not be blocked as in-progress');
  assert.equal(second.data.error, 'MockPay temporarily unavailable');
  assert.notEqual(second.data.reqId, first.data.reqId, 'Each retry should be its own genuine execution with a distinct request ID');

  // Clean up after ourselves so this test doesn't leave mockpay's circuit
  // primed to trip on the *next* run either.
  await redis.del('circuit:mockpay');
});

test('teardown: close the Redis connection', async () => {
  await redis.quit();
});
