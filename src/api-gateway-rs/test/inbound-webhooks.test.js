// Integration tests for Inbound Webhooks' tier gating (ee/inbound_webhooks.rs)
// — the first automated coverage this feature has had; up to now it was
// only manually curl-verified (both today's tier work and this morning's
// security audit fixes to receive()'s signature verification, which this
// file does not re-test). Covers exactly what Task 6 of tasks/todo.md
// changed: create() requiring Agency+, specifically NOT satisfied by Pro
// alone.
//
// Run with the server already up AND ENTERPRISE_MODE=true (ar-api-rs
// always runs with this set — see compose.yaml).

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

function createBody(orgId) {
  return { org_id: orgId, provider: 'github', webhook_secret: 'whsec_12345678', destination_url: 'https://example.com/hook' };
}

let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});

test('inbound webhook creation requires Agency specifically — Community and Pro both rejected, Agency succeeds', async () => {
  const email = `webhooktier-${RUN_ID}@internal.test`;
  const orgId = `org_webhooktier_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  const authHeaders = { headers: { Cookie: sessionCookie } };

  const communityRes = await client.post('/api/v1/inbound-webhooks', createBody(orgId), authHeaders);
  assert.equal(communityRes.status, 403, JSON.stringify(communityRes.data));

  await setPlan(pg, orgId, 'pro');
  const proRes = await client.post('/api/v1/inbound-webhooks', createBody(orgId), authHeaders);
  assert.equal(proRes.status, 403, `Pro alone must not be enough: ${JSON.stringify(proRes.data)}`);
  assert.match(proRes.data.error, /Agency plan/);

  await setPlan(pg, orgId, 'agency');
  const agencyRes = await client.post('/api/v1/inbound-webhooks', createBody(orgId), authHeaders);
  assert.equal(agencyRes.status, 200, JSON.stringify(agencyRes.data));
  assert.equal(agencyRes.data.provider, 'github');
});
