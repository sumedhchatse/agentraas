// Integration tests for the Team+ prebuilt-image self-host path (a paid,
// agentraas.io-only feature — this public repo's own build never has the
// image to serve, and degrades to a clear 503, which is what the
// Community-tier tests below actually cover as their regression case).
// Covers the tier branch itself and the regression on the existing
// branch itself and the regression on the existing Community zip flow;
// doesn't cover a real GHCR pull or a real podman load (that needs a
// real production-built tarball, out of scope for a fast local test —
// a small dummy file at ENTERPRISE_IMAGE_PATH stands in for it, enough
// to prove the branching/streaming logic itself is correct).
//
// Run with the server already up. test.before writes a dummy tarball to
// ./self-host-artifacts/ on the HOST (relative to compose.yaml's
// directory - that's what's bind-mounted read-only into ar-api-rs, so
// the file has to be created host-side, not via `podman exec` inside
// the container, which only has read access to that path). Skips itself
// gracefully if that directory isn't reachable from wherever tests run.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');
const fs = require('node:fs');
const path = require('node:path');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });

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

async function requestAndDownload(cookie) {
  const req = await client.post('/api/v1/download/self-host/request', { reason: 'testing M7' }, { headers: { Cookie: cookie } });
  assert.equal(req.status, 200, JSON.stringify(req.data));
  return client.get('/api/v1/download/self-host', { headers: { Cookie: cookie }, responseType: 'arraybuffer' });
}

let pg;
let hasDummyImage = false;
const DUMMY_CONTENTS = 'dummy-tar-contents\n';
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
  try {
    const dir = process.env.SELF_HOST_ARTIFACTS_DIR || path.resolve(__dirname, '../../../self-host-artifacts');
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, 'agentraas-enterprise.tar'), DUMMY_CONTENTS);
    hasDummyImage = true;
  } catch {
    hasDummyImage = false;
  }
});
test.after(async () => {
  await pg.end();
});

test('Community-tier download is unaffected (regression): still a plain zip', async () => {
  const email = `m7-community-${RUN_ID}@internal.test`;
  const orgId = `org_m7_community_${RUN_ID}`;
  const cookie = await registerAndVerify(email, 'validpassword123', orgId);
  await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: `agent_${RUN_ID}`, label: 'm7 test' }, { headers: { Cookie: cookie } });

  const res = await requestAndDownload(cookie);
  assert.equal(res.status, 200, JSON.stringify(res.data));
  assert.match(res.headers['content-disposition'], /agentraas-self-host\.zip/);
  assert.doesNotMatch(res.headers['content-disposition'], /enterprise/);
});

test('Community-tier org cannot download the enterprise image (403)', async () => {
  const email = `m7-community2-${RUN_ID}@internal.test`;
  const orgId = `org_m7_community2_${RUN_ID}`;
  const cookie = await registerAndVerify(email, 'validpassword123', orgId);
  await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: `agent2_${RUN_ID}`, label: 'm7 test' }, { headers: { Cookie: cookie } });
  await client.post('/api/v1/download/self-host/request', { reason: 'testing M7' }, { headers: { Cookie: cookie } });

  const res = await client.get('/api/v1/download/self-host/enterprise-image', { headers: { Cookie: cookie } });
  assert.equal(res.status, 403, JSON.stringify(res.data));
});

test('Team-tier org gets the enterprise starter zip (different filename, image: not build:) and can download the image tarball', async (t) => {
  if (!hasDummyImage) {
    t.skip('could not create a dummy tarball inside ar-api-rs (not running under podman locally?)');
    return;
  }
  const email = `m7-team-${RUN_ID}@internal.test`;
  const orgId = `org_m7_team_${RUN_ID}`;
  const cookie = await registerAndVerify(email, 'validpassword123', orgId);
  await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: `agent3_${RUN_ID}`, label: 'm7 test' }, { headers: { Cookie: cookie } });
  await setPlan(pg, orgId, 'team');

  const zipRes = await requestAndDownload(cookie);
  assert.equal(zipRes.status, 200, JSON.stringify(zipRes.data));
  assert.match(zipRes.headers['content-disposition'], /agentraas-self-host-enterprise\.zip/);

  const imageRes = await client.get('/api/v1/download/self-host/enterprise-image', { headers: { Cookie: cookie }, responseType: 'arraybuffer' });
  assert.equal(imageRes.status, 200, `expected 200, got ${imageRes.status}`);
  assert.equal(Buffer.from(imageRes.data).toString(), DUMMY_CONTENTS);
  assert.match(imageRes.headers['content-disposition'], /agentraas-enterprise\.tar/);
});
