// Integration tests for Enterprise SSO + RBAC, against ar-api-rs
// (crates/api/src/ee/sso.rs) over HTTP only.
//
// Run with the server already up and ENTERPRISE_MODE=true: npm test
// (from src/api-gateway-rs/). If ENTERPRISE_MODE is unset, every gated
// endpoint below correctly 403s instead of behaving as asserted — that's
// the ENTERPRISE_MODE gate itself working, not a bug, but this suite
// assumes it's enabled to exercise the actual feature.
//
// What this file DOES cover: config CRUD validation/authorization
// (including multi-IdP-per-org), the bootstrap-ownership path for a
// brand-new org's first SSO config (and that it cements into a real admin
// membership row), masked-secret handling, login/callback 404s for
// unconfigured/disabled/ambiguous (multi-config, no config_id) orgs, the
// additive GET /api/v1/auth/me `orgs` field, invite-by-email (create,
// list, accept — both new-account and existing-account paths), member
// list/role-change/removal, and auditor read-only enforcement on write
// endpoints.
//
// The two pure static helpers this file used to unit-test directly
// (match_org_by_email_domain / map_claims_to_role) are now native Rust
// tests next to their implementation — see the `#[cfg(test)] mod tests`
// block in `ee/sso.rs` — rather than reaching into Rust internals from
// JS, which isn't possible the way the old Node `require('../ee/auth')`
// was.
//
// What this file CANNOT cover without a live external IdP (or a stub OIDC
// provider, neither of which exists in this repo's test infra today):
// the actual authorization-code exchange, real discovery against a real
// issuer's /.well-known/openid-configuration, and a full end-to-end
// "log in via SSO and land with a valid session cookie" round-trip. That
// path is manual/operator-verified for now.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });

// Team invites moved from Enterprise-only to Pro+ (tasks/todo.md Task 8) —
// tests that create an invite need their org upgraded first. Plan changes
// go straight through Postgres, same as test/hitl.test.js, since there's
// no self-serve "set my plan" API by design.
let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});
async function setPlan(orgId, plan) {
  await pg.query('UPDATE users SET plan = $1 WHERE org_id = $2', [plan, orgId]);
}

const RUN_ID = Date.now();

async function registerAndVerify(email, password, orgId) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password, org_id: orgId });
  assert.equal(registerRes.status, 200, `Expected registration to succeed: ${JSON.stringify(registerRes.data)}`);
  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200);
  return verifyRes.headers['set-cookie'][0].split(';')[0];
}

async function connectAgentToOrg(sessionCookie, orgId) {
  const res = await client.post(
    '/api/v1/agents/connect',
    { org_id: orgId, agent_id: `agent_${RUN_ID}` },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(res.status, 200, `Expected agent connect to succeed: ${JSON.stringify(res.data)}`);
}

const VALID_CONFIG = {
  issuer_url: 'https://example-idp.test/oauth2/default',
  client_id: 'test-client-id',
  client_secret: 'test-client-secret-value-123',
  allowed_domains: 'acme.com,acme.io',
};

test('SSO login is 404 for an org with no config', async () => {
  const res = await client.get(`/api/v1/auth/sso/no-such-org-${RUN_ID}/login`);
  assert.equal(res.status, 404);
});

test('POST config rejects a non-https issuer_url', async () => {
  const email = `sso-badissuer-${RUN_ID}@internal.test`;
  const orgId = `org_sso_badissuer_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await connectAgentToOrg(sessionCookie, orgId);

  const res = await client.post(
    `/api/v1/auth/sso/${orgId}/configs`,
    { ...VALID_CONFIG, issuer_url: 'http://not-https.test' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(res.status, 422);
});

test('POST config rejects an invalid default_role', async () => {
  const email = `sso-badrole-${RUN_ID}@internal.test`;
  const orgId = `org_sso_badrole_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await connectAgentToOrg(sessionCookie, orgId);

  const res = await client.post(
    `/api/v1/auth/sso/${orgId}/configs`,
    { ...VALID_CONFIG, default_role: 'superadmin' },
    { headers: { Cookie: sessionCookie } }
  );
  assert.equal(res.status, 422);
});

test('bootstrap: an org owner can create the first SSO config (cementing them as org admin), secret is never returned in plaintext, and login is then enabled', async () => {
  const email = `sso-bootstrap-${RUN_ID}@internal.test`;
  const orgId = `org_sso_bootstrap_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await connectAgentToOrg(sessionCookie, orgId);

  const postRes = await client.post(`/api/v1/auth/sso/${orgId}/configs`, VALID_CONFIG, { headers: { Cookie: sessionCookie } });
  assert.equal(postRes.status, 200, JSON.stringify(postRes.data));
  assert.ok(!JSON.stringify(postRes.data).includes(VALID_CONFIG.client_secret), 'POST response must not echo the raw secret');
  const configId = postRes.data.id;

  const getRes = await client.get(`/api/v1/auth/sso/${orgId}/configs`, { headers: { Cookie: sessionCookie } });
  assert.equal(getRes.status, 200);
  assert.ok(!JSON.stringify(getRes.data).includes(VALID_CONFIG.client_secret), 'GET response must not contain the raw secret');
  assert.ok(getRes.data.configs[0].client_secret_preview.includes('••••'), 'Secret should be masked');

  // Login should now be reachable (single enabled config -> auto-picked,
  // no config_id needed — 302 redirect to the fake IdP, we don't follow it).
  const loginRes = await client.get(`/api/v1/auth/sso/${orgId}/login`, { maxRedirects: 0, validateStatus: () => true });
  assert.notEqual(loginRes.status, 404);

  // Bootstrap cemented a real admin membership row — a second admin-gated
  // call (e.g. member list) must now succeed via the fast path, not just
  // the one-time fallback.
  const membersRes = await client.get(`/api/v1/auth/sso/${orgId}/members`, { headers: { Cookie: sessionCookie } });
  assert.equal(membersRes.status, 200);
  assert.equal(membersRes.data.members[0].role, 'admin');

  // With config_id supplied it's still reachable directly.
  const loginByIdRes = await client.get(`/api/v1/auth/sso/${orgId}/login?config_id=${configId}`, { maxRedirects: 0, validateStatus: () => true });
  assert.notEqual(loginByIdRes.status, 404);
});

test('multi-IdP: an org with more than one enabled config requires config_id at login', async () => {
  const email = `sso-multi-${RUN_ID}@internal.test`;
  const orgId = `org_sso_multi_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await connectAgentToOrg(sessionCookie, orgId);

  const c1 = await client.post(`/api/v1/auth/sso/${orgId}/configs`, VALID_CONFIG, { headers: { Cookie: sessionCookie } });
  assert.equal(c1.status, 200);
  const c2 = await client.post(`/api/v1/auth/sso/${orgId}/configs`, { ...VALID_CONFIG, issuer_url: 'https://second-idp.test' }, { headers: { Cookie: sessionCookie } });
  assert.equal(c2.status, 200);

  const listRes = await client.get(`/api/v1/auth/sso/${orgId}/configs`, { headers: { Cookie: sessionCookie } });
  assert.equal(listRes.data.configs.length, 2);

  const ambiguousLogin = await client.get(`/api/v1/auth/sso/${orgId}/login`);
  assert.equal(ambiguousLogin.status, 404, 'no config_id, more than one enabled config -> ambiguous, should 404');

  const specificLogin = await client.get(`/api/v1/auth/sso/${orgId}/login?config_id=${c2.data.id}`, { maxRedirects: 0, validateStatus: () => true });
  assert.notEqual(specificLogin.status, 404);
});

test('a user with no relationship to the org cannot configure or view its SSO settings', async () => {
  const ownerEmail = `sso-owner-${RUN_ID}@internal.test`;
  const orgId = `org_sso_unauthorized_${RUN_ID}`;
  const ownerCookie = await registerAndVerify(ownerEmail, 'validpassword123', orgId);
  await connectAgentToOrg(ownerCookie, orgId);
  const postRes = await client.post(`/api/v1/auth/sso/${orgId}/configs`, VALID_CONFIG, { headers: { Cookie: ownerCookie } });
  assert.equal(postRes.status, 200);

  const strangerEmail = `sso-stranger-${RUN_ID}@internal.test`;
  const strangerCookie = await registerAndVerify(strangerEmail, 'validpassword123');
  const getRes = await client.get(`/api/v1/auth/sso/${orgId}/configs`, { headers: { Cookie: strangerCookie } });
  assert.equal(getRes.status, 403);

  const postAsStranger = await client.post(`/api/v1/auth/sso/${orgId}/configs`, VALID_CONFIG, { headers: { Cookie: strangerCookie } });
  assert.equal(postAsStranger.status, 403);
});

test('DELETE config disables that IdP (back to 404 if it was the only one)', async () => {
  const email = `sso-delete-${RUN_ID}@internal.test`;
  const orgId = `org_sso_delete_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);
  await connectAgentToOrg(sessionCookie, orgId);

  const postRes = await client.post(`/api/v1/auth/sso/${orgId}/configs`, VALID_CONFIG, { headers: { Cookie: sessionCookie } });
  const deleteRes = await client.delete(`/api/v1/auth/sso/${orgId}/configs/${postRes.data.id}`, { headers: { Cookie: sessionCookie } });
  assert.equal(deleteRes.status, 200);

  const loginRes = await client.get(`/api/v1/auth/sso/${orgId}/login`);
  assert.equal(loginRes.status, 404);
});

test('invite flow: admin invites a new user by email, invite is accepted, membership + role are granted, and write access is enforced by role', async () => {
  const adminEmail = `invite-admin-${RUN_ID}@internal.test`;
  const orgId = `org_invite_${RUN_ID}`;
  const adminCookie = await registerAndVerify(adminEmail, 'validpassword123', orgId);
  await setPlan(orgId, 'pro'); // team invites require Pro+
  await connectAgentToOrg(adminCookie, orgId);
  // Bootstraps the admin into a real org_members row via any admin-gated call.
  await client.get(`/api/v1/auth/sso/${orgId}/configs`, { headers: { Cookie: adminCookie } });

  const inviteeEmail = `invitee-${RUN_ID}@internal.test`;
  const inviteRes = await client.post(
    `/api/v1/auth/sso/${orgId}/invites`,
    { email: inviteeEmail, role: 'auditor' },
    { headers: { Cookie: adminCookie } }
  );
  assert.equal(inviteRes.status, 200, JSON.stringify(inviteRes.data));
  assert.ok(inviteRes.data.dev_accept_url, 'expected a dev_accept_url since SMTP is not configured');

  const pendingRes = await client.get(`/api/v1/auth/sso/${orgId}/invites`, { headers: { Cookie: adminCookie } });
  assert.equal(pendingRes.status, 200);
  assert.equal(pendingRes.data.invites.length, 1);
  assert.equal(pendingRes.data.invites[0].email, inviteeEmail);

  const inviteToken = new URL(inviteRes.data.dev_accept_url).searchParams.get('invite_token');
  const acceptRes = await client.post('/api/v1/auth/invites/accept', { token: inviteToken, password: 'validpassword123' });
  assert.equal(acceptRes.status, 200, JSON.stringify(acceptRes.data));
  assert.equal(acceptRes.data.role, 'auditor');
  const inviteeCookie = acceptRes.headers['set-cookie'][0].split(';')[0];

  // Accepting again with the same (now-consumed) token must fail.
  const reacceptRes = await client.post('/api/v1/auth/invites/accept', { token: inviteToken, password: 'validpassword123' });
  assert.equal(reacceptRes.status, 400);

  // Auditor can read...
  const readRes = await client.get('/api/v1/stats', { headers: { Cookie: inviteeCookie } });
  assert.equal(readRes.status, 200);
  // ...but not write in that org.
  const writeRes = await client.post(
    '/api/v1/agents/connect',
    { org_id: orgId, agent_id: `auditor_agent_${RUN_ID}` },
    { headers: { Cookie: inviteeCookie } }
  );
  assert.equal(writeRes.status, 403);

  // Admin can list/change/remove members.
  const membersRes = await client.get(`/api/v1/auth/sso/${orgId}/members`, { headers: { Cookie: adminCookie } });
  assert.equal(membersRes.status, 200);
  const inviteeMember = membersRes.data.members.find((m) => m.email === inviteeEmail);
  assert.equal(inviteeMember.role, 'auditor');

  const promoteRes = await client.put(
    `/api/v1/auth/sso/${orgId}/members/${inviteeMember.user_id}`,
    { role: 'developer' },
    { headers: { Cookie: adminCookie } }
  );
  assert.equal(promoteRes.status, 200);
  // Now a developer, write access should be restored.
  const writeAfterPromote = await client.post(
    '/api/v1/agents/connect',
    { org_id: orgId, agent_id: `dev_agent_${RUN_ID}` },
    { headers: { Cookie: inviteeCookie } }
  );
  assert.equal(writeAfterPromote.status, 200);

  const removeRes = await client.delete(`/api/v1/auth/sso/${orgId}/members/${inviteeMember.user_id}`, { headers: { Cookie: adminCookie } });
  assert.equal(removeRes.status, 200);
  const membersAfterRemove = await client.get(`/api/v1/auth/sso/${orgId}/members`, { headers: { Cookie: adminCookie } });
  assert.ok(!membersAfterRemove.data.members.some((m) => m.email === inviteeEmail));
});

test('invite accept for an already-registered email does not require/touch a password, just grants membership', async () => {
  const adminEmail = `invite-admin2-${RUN_ID}@internal.test`;
  const orgId = `org_invite2_${RUN_ID}`;
  const adminCookie = await registerAndVerify(adminEmail, 'validpassword123', orgId);
  await setPlan(orgId, 'pro'); // team invites require Pro+
  await connectAgentToOrg(adminCookie, orgId);
  await client.get(`/api/v1/auth/sso/${orgId}/configs`, { headers: { Cookie: adminCookie } });

  const existingEmail = `existing-user-${RUN_ID}@internal.test`;
  await registerAndVerify(existingEmail, 'originalpassword123');

  const inviteRes = await client.post(
    `/api/v1/auth/sso/${orgId}/invites`,
    { email: existingEmail, role: 'developer' },
    { headers: { Cookie: adminCookie } }
  );
  const inviteToken = new URL(inviteRes.data.dev_accept_url).searchParams.get('invite_token');

  // No password supplied — should still work, since the account already exists.
  const acceptRes = await client.post('/api/v1/auth/invites/accept', { token: inviteToken });
  assert.equal(acceptRes.status, 200, JSON.stringify(acceptRes.data));

  // Original password still works — it was never touched.
  const loginRes = await client.post('/api/v1/auth/login', { email: existingEmail, password: 'originalpassword123' });
  assert.equal(loginRes.status, 200);
});

test('GET /api/v1/auth/me includes an empty orgs array for a non-SSO user (additive-field regression check)', async () => {
  const email = `sso-me-${RUN_ID}@internal.test`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123');
  const res = await client.get('/api/v1/auth/me', { headers: { Cookie: sessionCookie } });
  assert.equal(res.status, 200);
  assert.deepEqual(res.data.user.orgs, []);
});
