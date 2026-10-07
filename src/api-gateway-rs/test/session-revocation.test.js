// Server-side session revocation (auth/mod.rs, migration 053): logout ends
// that one token, a password change ends every other session but keeps the
// caller logged in, "log out everywhere" ends them all. Also checks the CSP
// header is on the dashboard. Run with the server already up.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const RUN_ID = Date.now();
const EMAIL = `sessions-${RUN_ID}@internal.test`;

const cookieOf = (res) => res.headers['set-cookie'][0].split(';')[0];
const me = async (cookie) => (await client.get('/api/v1/auth/me', { headers: { Cookie: cookie } })).status;
const login = async (password) => {
  const res = await client.post('/api/v1/auth/login', { email: EMAIL, password });
  assert.equal(res.status, 200, JSON.stringify(res.data));
  return cookieOf(res);
};

test('logout, password change and log out everywhere revoke sessions server-side', async () => {
  const reg = await client.post('/api/v1/auth/register', { email: EMAIL, password: 'firstpassword1', org_id: `org_sess_${RUN_ID}` });
  assert.equal(reg.status, 200, JSON.stringify(reg.data));
  const token = new URL(reg.data.dev_verify_url).searchParams.get('verify_token');
  const a = cookieOf(await client.get(`/api/v1/auth/verify-email?token=${token}`));
  const b = await login('firstpassword1');
  assert.equal(await me(a), 200);
  assert.equal(await me(b), 200);

  // Logout ends that token only, even though the cookie itself is still a
  // validly signed, unexpired JWT someone could have copied.
  assert.equal((await client.post('/api/v1/auth/logout', {}, { headers: { Cookie: a } })).status, 200);
  assert.equal(await me(a), 401);
  assert.equal(await me(b), 200);

  // Password change: every other session ends, the caller gets a fresh cookie.
  const other = await login('firstpassword1');
  const change = await client.post('/api/v1/auth/password', { current_password: 'firstpassword1', new_password: 'secondpassword2' }, { headers: { Cookie: b } });
  assert.equal(change.status, 200, JSON.stringify(change.data));
  const c = cookieOf(change);
  assert.equal(await me(other), 401, 'a session from before the change must end');
  assert.equal(await me(b), 401, 'the old cookie of the caller ends too');
  assert.equal(await me(c), 200, 'the fresh cookie keeps the caller logged in');

  // Log out everywhere.
  const d = await login('secondpassword2');
  assert.equal((await client.post('/api/v1/auth/logout-all', {}, { headers: { Cookie: d } })).status, 200);
  assert.equal(await me(c), 401);
  assert.equal(await me(d), 401);
  assert.equal(await me(await login('secondpassword2')), 200, 'logging in again works');
});

test('the dashboard carries a Content-Security-Policy', async () => {
  const res = await client.get('/dashboard');
  const csp = res.headers['content-security-policy'] || '';
  assert.match(csp, /frame-ancestors 'none'/);
  assert.match(csp, /object-src 'none'/);
  assert.match(csp, /script-src 'self' https:\/\/cdn\.paddle\.com/);
  assert.doesNotMatch(csp, /script-src[^;]*unsafe-inline/, 'no inline scripts allowed');
  const js = await client.get('/js/dashboard.js');
  assert.equal(js.status, 200);
  assert.match(js.headers['content-type'], /javascript/);
  assert.equal((await client.get('/js/..%2Fdashboard.js')).status, 404);
  assert.equal((await client.get('/js/nope.js')).status, 404);
});
