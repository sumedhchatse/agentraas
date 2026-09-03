// Integration tests for auth, credentials, and custom-action validation —
// complementing dedup.test.js, which covers the exactly-once logic specifically.
//
// Run with the server already up: podman exec -it ar-api npm test

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });

// Unique per test run so re-running the suite doesn't collide with leftover accounts.
const RUN_ID = Date.now();

// Registration requires email verification before login. With no SMTP
// configured (the state these tests run in), the verify link comes back
// directly in the register response instead of only being emailed — the
// same fallback a self-hoster without email set up would get.
async function registerAndVerify(email, password) {
  const registerRes = await client.post('/api/v1/auth/register', { email, password });
  assert.equal(registerRes.status, 200, `Expected registration to succeed: ${JSON.stringify(registerRes.data)}`);
  assert.ok(registerRes.data.dev_verify_url, 'Expected a dev_verify_url since SMTP is not configured in this test environment');

  const token = new URL(registerRes.data.dev_verify_url).searchParams.get('verify_token');
  const verifyRes = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  assert.equal(verifyRes.status, 200, `Expected verification to succeed: ${JSON.stringify(verifyRes.data)}`);
  const setCookie = verifyRes.headers['set-cookie'];
  assert.ok(setCookie?.length > 0, 'Expected verify-email to also log the user in');
  return setCookie[0].split(';')[0];
}

test('registration rejects a weak password', async () => {
  const res = await client.post('/api/v1/auth/register', {
    email: `weakpass-${RUN_ID}@internal.test`,
    password: 'short',
  });
  assert.equal(res.status, 422);
});

test('registration rejects an invalid email', async () => {
  const res = await client.post('/api/v1/auth/register', {
    email: 'not-an-email',
    password: 'validpassword123',
  });
  assert.equal(res.status, 422);
});

test('duplicate registration is rejected with 409', async () => {
  const email = `dup-${RUN_ID}@internal.test`;
  const first = await client.post('/api/v1/auth/register', { email, password: 'validpassword123' });
  assert.equal(first.status, 200);

  const second = await client.post('/api/v1/auth/register', { email, password: 'validpassword123' });
  assert.equal(second.status, 409);
});

test('registration does not log the user in — login is blocked until email is verified', async () => {
  const email = `unverified-${RUN_ID}@internal.test`;
  const registerRes = await client.post('/api/v1/auth/register', { email, password: 'validpassword123' });
  assert.equal(registerRes.status, 200);
  assert.ok(!registerRes.headers['set-cookie'], 'Register should not set a session cookie before verification');

  const loginRes = await client.post('/api/v1/auth/login', { email, password: 'validpassword123' });
  assert.equal(loginRes.status, 403);
  assert.equal(loginRes.data.code, 'EMAIL_NOT_VERIFIED');
});

test('verifying the email logs the user in, and an invalid token is rejected', async () => {
  const email = `verify-${RUN_ID}@internal.test`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123');
  assert.ok(sessionCookie.startsWith('ar_session='));

  const badTokenRes = await client.get('/api/v1/auth/verify-email?token=not-a-real-token');
  assert.equal(badTokenRes.status, 400);

  // Now that it's verified, a normal login should work too.
  const loginRes = await client.post('/api/v1/auth/login', { email, password: 'validpassword123' });
  assert.equal(loginRes.status, 200);
});

test('login with a wrong password fails without revealing whether the account exists', async () => {
  const email = `wrongpass-${RUN_ID}@internal.test`;
  await client.post('/api/v1/auth/register', { email, password: 'correctpassword123' });
  // Note: intentionally left unverified — the password check happens before
  // the verification check, so this should still be a generic 401, not a
  // verification-related error, regardless of verified status.

  const wrongPassword = await client.post('/api/v1/auth/login', { email, password: 'incorrectpassword' });
  const nonexistentAccount = await client.post('/api/v1/auth/login', { email: `nobody-${RUN_ID}@internal.test`, password: 'whatever123' });

  assert.equal(wrongPassword.status, 401);
  assert.equal(nonexistentAccount.status, 401);
  assert.equal(wrongPassword.data.error, nonexistentAccount.data.error, 'Both failure cases should return the identical generic error');
});

test('forgot-password always returns the same generic message, for a real or fake email', async () => {
  const realEmail = `forgot-real-${RUN_ID}@internal.test`;
  await client.post('/api/v1/auth/register', { email: realEmail, password: 'validpassword123' });

  const realRes = await client.post('/api/v1/auth/forgot-password', { email: realEmail });
  const fakeRes = await client.post('/api/v1/auth/forgot-password', { email: `nobody-${RUN_ID}@internal.test` });

  assert.equal(realRes.status, 200);
  assert.equal(fakeRes.status, 200);
  assert.equal(realRes.data.message, fakeRes.data.message, 'Response must not reveal whether the email has an account');
});

test('reset-password rejects an invalid/unknown token', async () => {
  const res = await client.post('/api/v1/auth/reset-password', {
    token: 'not-a-real-token',
    new_password: 'newvalidpassword123',
  });
  assert.equal(res.status, 400);
});

// ─── Credentials: encryption round-trip and masking ───
test('saved credentials are masked in listings, never returned in plaintext', async () => {
  const email = `creds-${RUN_ID}@internal.test`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123');
  const authHeaders = { headers: { Cookie: sessionCookie } };

  const orgId = `org_creds_test_${RUN_ID}`;
  const secretValue = 'sk_live_this_is_a_real_secret_value_123456';

  const saveRes = await client.post('/api/v1/credentials', {
    org_id: orgId, service: 'stripe', credentials: { api_key: secretValue },
  }, authHeaders);
  assert.equal(saveRes.status, 200);
  assert.notEqual(saveRes.data.masked_preview, secretValue, 'Save response must not echo the raw secret');
  assert.ok(saveRes.data.masked_preview.includes('••••'), 'Preview should be masked');

  const listRes = await client.get('/api/v1/credentials', authHeaders);
  assert.equal(listRes.status, 200);
  const bodyText = JSON.stringify(listRes.data);
  assert.ok(!bodyText.includes(secretValue), 'The full secret must never appear anywhere in the credentials list response');
});

// ─── Custom Actions: SSRF guard ───
test('custom action registration rejects a private/internal target URL', async () => {
  const email = `ssrf-${RUN_ID}@internal.test`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123');
  const authHeaders = { headers: { Cookie: sessionCookie } };

  const attempts = [
    'http://127.0.0.1/internal',
    'http://localhost/internal',
    'http://10.0.0.5/internal',
    'http://ar-postgres/internal',
  ];

  for (const target_url of attempts) {
    const res = await client.post('/api/v1/custom-actions', {
      org_id: `org_ssrf_test_${RUN_ID}`,
      name: `ssrf_probe_${RUN_ID}`,
      method: 'POST',
      target_url,
      auth_type: 'none',
    }, authHeaders);
    assert.equal(res.status, 422, `Expected ${target_url} to be rejected, got ${res.status}: ${JSON.stringify(res.data)}`);
  }
});

test('custom action registration rejects an invalid identifier (org_id with disallowed characters)', async () => {
  const email = `identifier-${RUN_ID}@internal.test`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123');
  const authHeaders = { headers: { Cookie: sessionCookie } };

  const res = await client.post('/api/v1/custom-actions', {
    org_id: 'org with spaces!',
    name: `valid_name_${RUN_ID}`,
    method: 'POST',
    target_url: 'https://httpbin.org/post',
    auth_type: 'none',
  }, authHeaders);
  assert.equal(res.status, 422);
});
