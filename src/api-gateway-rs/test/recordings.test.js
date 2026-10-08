// Record and replay (crates/api/src/recordings.rs): calls made with
// X-AgentRaaS-Record are kept; the same calls with X-AgentRaaS-Replay are
// answered from the recording, in order, without reaching the provider.
// Needs migration 055. Run with the server already up.

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const { Client } = require('pg');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true, maxRedirects: 0 });
const RUN_ID = Date.now();

async function registerAndVerify(email, orgId) {
  const reg = await client.post('/api/v1/auth/register', { email, password: 'validpassword123', org_id: orgId });
  assert.equal(reg.status, 200, JSON.stringify(reg.data));
  const token = new URL(reg.data.dev_verify_url).searchParams.get('verify_token');
  const res = await client.get(`/api/v1/auth/verify-email?token=${token}`);
  return res.headers['set-cookie'][0].split(';')[0];
}

let pg, orgId, agentId, cookie, key;
test.before(async () => {
  pg = new Client({ connectionString: process.env.DATABASE_URL || 'postgres://agentraas:agentraas@localhost:5432/agentraas' });
  await pg.connect();
  orgId = `org_rec_${RUN_ID}`;
  agentId = `agent_rec_${RUN_ID}`;
  cookie = await registerAndVerify(`rec-${RUN_ID}@internal.test`, orgId);
  key = (await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'recording test' }, { headers: { Cookie: cookie } })).data.api_key;
});
test.after(async () => { await pg.end(); });

const pay = (amount, headers) => client.post(`/v1/webhook/${orgId}/${agentId}`,
  { service: 'mockpay', action: 'payment.create', payload: { amount, fail: false } },
  { headers: { Authorization: `Bearer ${key}`, ...headers } });
const recording = (name, who = cookie) => client.get(`/api/v1/recordings/${name}?org_id=${orgId}`, { headers: { Cookie: who } });
async function waitForCalls(name, n) {
  for (let i = 0; i < 50; i++) {
    const r = await recording(name);
    if (r.data.calls && r.data.calls.length >= n) return r.data.calls;
    await new Promise((res) => setTimeout(res, 100));
  }
  assert.fail(`recording ${name} never reached ${n} calls`);
}
const charges = async () => Number((await pg.query(
  "SELECT COUNT(*) FROM audit_log WHERE org_id = $1 AND status = 'success'", [orgId])).rows[0].count);

test('a recorded run replays in order, without calling the provider', async () => {
  const name = `run_${RUN_ID}`;
  const a = await pay(21, { 'X-AgentRaaS-Record': name });
  const b = await pay(22, { 'X-AgentRaaS-Record': name });
  assert.equal(a.status, 200, JSON.stringify(a.data));
  assert.equal(b.status, 200, JSON.stringify(b.data));
  const calls = await waitForCalls(name, 2);
  assert.deepEqual(calls.map((c) => c.payload.amount), [21, 22]);
  assert.equal(calls[0].response.upstream_response.id, a.data.upstream_response.id);

  const before = await charges();
  const replayHeaders = { 'X-AgentRaaS-Replay': name, 'X-AgentRaaS-Run-Id': `replay_${RUN_ID}_1` };
  const r1 = await pay(21, replayHeaders);
  const r2 = await pay(99, replayHeaders);
  assert.equal(r1.status, 200, JSON.stringify(r1.data));
  assert.equal(r1.data.upstream_response.id, a.data.upstream_response.id);
  assert.deepEqual(r1.data.replay, { recording: name, position: 1, payload_matches: true });
  assert.equal(r2.data.upstream_response.id, b.data.upstream_response.id);
  assert.equal(r2.data.replay.payload_matches, false, 'a changed payload is flagged');

  const extra = await pay(23, replayHeaders);
  assert.equal(extra.status, 404, 'a call the recording does not have');
  assert.match(extra.data.error, /no call #3/);
  assert.equal(await charges(), before, 'nothing reached the provider during replay');

  const fresh = await pay(21, { 'X-AgentRaaS-Replay': name, 'X-AgentRaaS-Run-Id': `replay_${RUN_ID}_2` });
  assert.equal(fresh.data.replay.position, 1, 'a new run id starts from the top');
});

test('recordings are listed, private to the org, and deletable', async () => {
  const name = `list_${RUN_ID}`;
  assert.equal((await pay(31, { 'X-AgentRaaS-Record': name })).status, 200);
  await waitForCalls(name, 1);
  const list = await client.get(`/api/v1/recordings?org_id=${orgId}`, { headers: { Cookie: cookie } });
  assert.equal(list.data.recordings.find((r) => r.name === name).calls, 1);

  const other = await registerAndVerify(`rec-other-${RUN_ID}@internal.test`, `org_rec_other_${RUN_ID}`);
  assert.equal((await recording(name, other)).status, 403);

  const del = await client.delete(`/api/v1/recordings/${name}?org_id=${orgId}`, { headers: { Cookie: cookie } });
  assert.equal(del.data.deleted, 1);
  assert.equal((await recording(name)).data.calls.length, 0);

  const { rows } = await pg.query('SELECT encrypted_payload FROM recordings WHERE org_id = $1 LIMIT 1', [orgId]);
  assert.ok(!rows[0].encrypted_payload.includes('amount'), 'stored encrypted');
});
