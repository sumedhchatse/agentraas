// The "no keys yet, no key needed" shortcut (agent::db::verify_api_key) is
// self-host only: a fresh install works before anyone opens the dashboard.
// On Cloud an org_id nobody registered must not accept calls. Runs in either
// mode and checks the rule for the mode the server is in.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const RUN_ID = Date.now();

async function deploymentMode() {
  const reg = await client.post('/api/v1/auth/register', { email: `zc-${RUN_ID}@internal.test`, password: 'validpassword123', org_id: `org_zc_owner_${RUN_ID}` });
  const token = new URL(reg.data.dev_verify_url).searchParams.get('verify_token');
  const cookie = (await client.get(`/api/v1/auth/verify-email?token=${token}`)).headers['set-cookie'][0].split(';')[0];
  return (await client.get('/api/v1/auth/me', { headers: { Cookie: cookie } })).data.user.deployment_mode;
}

test('an org with no keys accepts calls only when self-hosted', async () => {
  const cloud = (await deploymentMode()) === 'cloud';
  const org = `org_zc_unregistered_${RUN_ID}`;
  const body = { service: 'mockpay', action: 'payment.create', payload: { amount: 3, fail: false } };

  const webhook = await client.post(`/v1/webhook/${org}/agent1`, body, { headers: { Authorization: 'Bearer ar_live_made_up' } });
  const mcp = await client.post('/mcp',
    { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'mockpay_payment_create', arguments: { org_id: org, agent_id: 'agent2', payload: { amount: 4, fail: false } } } },
    { headers: { 'x-agentraas-key': 'ar_live_made_up' } });

  if (cloud) {
    assert.equal(webhook.status, 401, JSON.stringify(webhook.data));
    assert.equal(mcp.data.result.isError, true);
    assert.match(mcp.data.result.content[0].text, /Invalid or missing API key/);
  } else {
    assert.equal(webhook.status, 200, JSON.stringify(webhook.data));
    assert.equal(mcp.data.result.isError, false, mcp.data.result.content[0].text);
  }
});
