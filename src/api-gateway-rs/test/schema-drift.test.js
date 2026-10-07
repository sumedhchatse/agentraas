// Integration tests for upstream schema drift detection
// (schema_drift.rs). mockpay's response shape is
// fixed/deterministic (fail:false), so there's no natural way to make the
// real upstream shape change for a test — instead, this establishes a
// real baseline via a real call, then injects a fake extra field directly
// into schema_baselines (same "go straight through Postgres" shortcut
// every other tier-dependent test in this suite already uses) to
// simulate "the API used to have this field," and verifies the next real
// call detects and records it as removed.
//
// Run with the server already up: node --test test/schema-drift.test.js

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });

const RUN_ID = Date.now();

// The drift check runs in the background after the response, so wait for
// its effect instead of a fixed sleep (CI machines are slower).
async function waitFor(check, what) {
  for (let i = 0; i < 50; i++) {
    const v = await check();
    if (v) return v;
    await new Promise((r) => setTimeout(r, 200));
  }
  assert.fail(`timed out waiting for ${what}`);
}

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, `Expected registration to succeed: ${JSON.stringify(registerRes.data)}`);
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});

test('a removed field is detected and recorded against the org whose request found it', async () => {
  const email = `schemadrift-${RUN_ID}@internal.test`;
  const orgId = `org_schemadrift_${RUN_ID}`;
  const agentId = `agent_schemadrift_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'schema drift test' }, { headers: { Cookie: sessionCookie } });
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const apiKey = connectRes.data.api_key;

  // A different amount each time: an identical payload would be answered by
  // dedup without reaching mockpay, and so without a schema check.
  const call = (amount) =>
    client.post(
      `/v1/webhook/${orgId}/${agentId}`,
      { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } },
      { headers: { Authorization: `Bearer ${apiKey}` } }
    );

  // Establish (or confirm) a real baseline first.
  const first = await call(1);
  assert.equal(first.status, 200, JSON.stringify(first.data));

  await waitFor(async () => (await pg.query("SELECT 1 FROM schema_baselines WHERE service = 'mockpay' AND action = 'payment.create'")).rowCount > 0, 'the baseline');

  // Inject a fake field into the shared baseline, simulating an upstream
  // field that used to exist.
  await pg.query(
    `UPDATE schema_baselines
     SET field_paths = field_paths || '["legacy_fee_field:number"]'::jsonb
     WHERE service = 'mockpay' AND action = 'payment.create'`
  );

  const second = await call(2);
  assert.equal(second.status, 200, JSON.stringify(second.data));

  const rows = await waitFor(async () => {
    const r = await pg.query(
      `SELECT removed_fields FROM schema_drift_events
       WHERE service = 'mockpay' AND action = 'payment.create' AND org_id = $1
       ORDER BY detected_at DESC LIMIT 1`,
      [orgId]
    );
    return r.rows.length ? r.rows : null;
  }, 'the drift event');
  assert.equal(rows.length, 1, 'expected exactly one drift event recorded for this org');
  const removed = rows[0].removed_fields;
  assert.ok(removed.some((f) => f.startsWith('legacy_fee_field:')), `expected legacy_fee_field to be flagged as removed, got: ${JSON.stringify(removed)}`);

  // Dashboard-facing listing endpoint reflects it too.
  const eventsRes = await client.get('/api/v1/schema-drift-events', { headers: { Cookie: sessionCookie } });
  assert.equal(eventsRes.status, 200, JSON.stringify(eventsRes.data));
  assert.ok(eventsRes.data.events.some((e) => e.service === 'mockpay' && e.action === 'payment.create'));
});
