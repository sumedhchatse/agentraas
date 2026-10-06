// Self-host package download: every plan gets the same source zip, and it
// carries the full edition (ee/ and the license files), since self-hosting
// includes every feature (LICENSE.md). The old Team+ prebuilt-image route
// (M7) is gone. Run with the server already up.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

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

let pg;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
});
test.after(async () => {
  await pg.end();
});

for (const plan of ['free', 'team']) {
  test(`${plan} plan gets the full-edition source zip`, async () => {
    const orgId = `org_shdl_${plan}_${RUN_ID}`;
    const cookie = await registerAndVerify(`shdl-${plan}-${RUN_ID}@internal.test`, 'validpassword123', orgId);
    await pg.query('UPDATE users SET plan = $1 WHERE org_id = $2', [plan, orgId]);
    await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: `agent_${plan}_${RUN_ID}`, label: 'download test' }, { headers: { Cookie: cookie } });
    const req = await client.post('/api/v1/download/self-host/request', { reason: 'testing download' }, { headers: { Cookie: cookie } });
    assert.equal(req.status, 200, JSON.stringify(req.data));

    const res = await client.get('/api/v1/download/self-host', { headers: { Cookie: cookie }, responseType: 'arraybuffer' });
    assert.equal(res.status, 200);
    assert.match(res.headers['content-disposition'], /filename="agentraas-self-host\.zip"/);
    // Zip entry names are stored uncompressed in the central directory.
    const zip = Buffer.from(res.data);
    for (const name of ['src/api-gateway-rs/crates/api/src/ee/mod.rs', 'src/api-gateway-rs/crates/core/src/dlp.rs', 'LICENSE-AGPL', 'LICENSE-FSL.md']) {
      assert.ok(zip.includes(name), `zip is missing ${name}`);
    }
    assert.ok(!zip.includes('landing-worker'), 'zip must not include the private landing-worker');

    const image = await client.get('/api/v1/download/self-host/enterprise-image', { headers: { Cookie: cookie } });
    assert.equal(image.status, 404);
  });
}
