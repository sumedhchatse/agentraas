// Pay-as-you-go Cloud billing (billing.rs). Needs a cloud-mode server:
// DEPLOYMENT_MODE=cloud, and for the BILLING_ON cases also
// BILLING_PAYG_ENABLED=true with no PADDLE_API_KEY (so the monthly job can
// never reach Paddle: a due charge must end up "failed", not charged).
//   BILLING_ON=1 node --test test/payg-billing.test.js   (billing switched on)
//   node --test test/payg-billing.test.js                (billing off)
// The charge cases seed last month's audit rows, then need the job to run:
// it runs once at startup, so restart the container after the first pass
// and run again with CHARGE_CHECK=1.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const BILLING_ON = process.env.BILLING_ON === '1';
const CHARGE_CHECK = process.env.CHARGE_CHECK === '1';
const RUN_ID = Date.now();

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, JSON.stringify(registerRes.data));
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

function lastMonth() {
  const d = new Date();
  const first = new Date(Date.UTC(d.getUTCFullYear(), d.getUTCMonth() - 1, 1, 12));
  return { key: first.toISOString().slice(0, 7), at: first.toISOString() };
}

let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});

test('usage endpoint, cap validation and the payg limit', { skip: CHARGE_CHECK }, async () => {
  const orgId = `org_payg_${RUN_ID}`;
  const cookie = await registerAndVerify(`payg-${RUN_ID}@internal.test`, 'validpassword123', orgId);
  const headers = { Cookie: cookie };

  let u = await client.get(`/api/v1/billing/usage?org_id=${orgId}`, { headers });
  assert.equal(u.status, 200, JSON.stringify(u.data));
  assert.equal(u.data.enabled, BILLING_ON);
  assert.equal(u.data.payg, false);
  assert.equal(u.data.monthly_cap_usd, 10);

  const other = await client.get('/api/v1/billing/usage?org_id=org_someone_else', { headers });
  assert.equal(other.status, 403);

  assert.equal((await client.put('/api/v1/billing/usage', { org_id: orgId, monthly_cap_usd: 0 }, { headers })).status, 422);
  assert.equal((await client.put('/api/v1/billing/usage', { org_id: orgId, monthly_cap_usd: 3 }, { headers })).status, 200);

  const checkout = await client.get('/api/v1/billing/checkout-info?plan=payg', { headers });
  assert.equal(checkout.status, 503, 'billing off, or no Paddle keys locally: never a checkout');

  // Free account: the free allowance. Payg: free + what $3 buys.
  let usage = await client.get('/api/v1/usage', { headers });
  const free = usage.data.limit;
  await pg.query("UPDATE users SET plan = 'payg' WHERE org_id = $1", [orgId]);
  usage = await client.get('/api/v1/usage', { headers });
  assert.equal(usage.data.limit, free + 3000);

  u = await client.get(`/api/v1/billing/usage?org_id=${orgId}`, { headers });
  assert.equal(u.data.payg, true);
  assert.equal(u.data.estimated_cents, 0);

  const me = await client.get('/api/v1/auth/me', { headers });
  assert.equal(me.data.user.plan, 'payg');
});

// Seeds two payg orgs with last month's actions: one under the free
// allowance (skipped, nothing owed), one over it (a charge is due).
test('seed last month for the charge job', { skip: !BILLING_ON || CHARGE_CHECK }, async () => {
  const { at } = lastMonth();
  for (const [name, actions] of [['under', 300], ['over', 2600]]) {
    const orgId = `org_paygjob_${name}`;
    await pg.query('DELETE FROM billing_charges WHERE org_id = $1', [orgId]);
    await pg.query('DELETE FROM audit_log WHERE org_id = $1', [orgId]);
    const email = `paygjob-${name}@internal.test`;
    let user = await pg.query('SELECT id FROM users WHERE email = $1', [email]);
    if (!user.rows.length) {
      await registerAndVerify(email, 'validpassword123', orgId);
      user = await pg.query('SELECT id FROM users WHERE email = $1', [email]);
    }
    const userId = user.rows[0].id;
    await pg.query("UPDATE users SET plan = 'payg' WHERE id = $1", [userId]);
    await pg.query(
      `INSERT INTO subscriptions (user_id, paddle_subscription_id, paddle_customer_id, status)
       VALUES ($1, $2, 'ctm_test', 'active') ON CONFLICT (paddle_subscription_id) DO UPDATE SET status = 'active'`,
      [userId, `sub_paygjob_${name}`],
    );
    await pg.query(
      `INSERT INTO audit_log (req_id, api_key, org_id, agent_id, service, action, status, duration_ms, created_at)
       SELECT 'r' || g, 'k', $1, 'a', 'stripe', 'charge', 'success', 1, $2 FROM generate_series(1, $3) g`,
      [orgId, at, actions],
    );
    // A blocked call and a duplicate are never billed.
    await pg.query(
      `INSERT INTO audit_log (req_id, api_key, org_id, agent_id, service, action, status, duration_ms, created_at)
       VALUES ('rb', 'k', $1, 'a', 'stripe', 'charge', 'blocked', 1, $2), ('rd', 'k', $1, 'a', 'stripe', 'charge', 'deduplicated', 1, $2)`,
      [orgId, at],
    );
  }
});

test('charge job: claims each month once, skips under a block, never charges without Paddle', { skip: !CHARGE_CHECK }, async () => {
  const { key } = lastMonth();
  const rows = await pg.query("SELECT org_id, actions, blocks, carried_out, amount_cents, status, error FROM billing_charges WHERE org_id LIKE 'org_paygjob_%' AND month = $1 ORDER BY org_id", [key]);
  const by = Object.fromEntries(rows.rows.map((r) => [r.org_id, r]));
  const over = by.org_paygjob_over;
  const under = by.org_paygjob_under;
  assert.ok(over && under, `expected both rows, got ${JSON.stringify(rows.rows)}`);
  assert.equal(Number(under.actions), 300);
  assert.equal(under.status, 'skipped');
  assert.equal(Number(under.carried_out), 0);
  // 2600 ran, 500 free: 2100 billable = 2 blocks ($2), 100 carried over.
  assert.equal(Number(over.actions), 2600);
  assert.equal(Number(over.blocks), 2);
  assert.equal(Number(over.amount_cents), 200);
  assert.equal(Number(over.carried_out), 100);
  assert.equal(over.status, 'failed');
  assert.match(over.error, /PADDLE_API_KEY/);
});
