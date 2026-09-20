// Integration tests for the CLI local dev tunnel (SPEC-TUNNEL.md,
// tunnel.rs). Covers the real round-trip (public request -> WebSocket ->
// simulated CLI -> local server -> response back through the same path)
// and the "one active tunnel per org" abuse control. Doesn't cover the
// dashboard SSE inspector or the 2-hour expiry (both would need either a
// browser EventSource client or manipulating the clock) - see
// project_plan/current-plan.md for what's verified vs not.
//
// Run with the server already up: node --test test/tunnel.test.js

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const http = require('node:http');
const WebSocket = require('ws');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const WS_BASE_URL = BASE_URL.replace(/^http/, 'ws');
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

// Starts a throwaway local HTTP server (standing in for "the developer's
// own AgentRaaS instance") that echoes the method/path it received.
function startLocalEchoServer() {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      let chunks = [];
      req.on('data', (c) => chunks.push(c));
      req.on('end', () => {
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(JSON.stringify({ method: req.method, path: req.url, bodyLength: Buffer.concat(chunks).length }));
      });
    });
    server.listen(0, '127.0.0.1', () => resolve(server));
  });
}

// Minimal stand-in for infra/scripts/tunnel-cli/index.js's own forwarding
// logic - connects, answers every "request" frame by hitting the local
// echo server, sends the response back. Resolves once "connected" arrives
// with the tunnel's public URL.
function connectSimulatedCli(orgId, agentId, apiKey, localPort) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(`${WS_BASE_URL}/api/v1/tunnel/connect/${orgId}/${agentId}`, { headers: { Authorization: `Bearer ${apiKey}` } });
    ws.on('message', (raw) => {
      const msg = JSON.parse(raw.toString());
      if (msg.type === 'connected') {
        resolve({ ws, publicUrl: msg.url });
        return;
      }
      if (msg.type === 'request') {
        const body = msg.body_base64 ? Buffer.from(msg.body_base64, 'base64') : Buffer.alloc(0);
        const localReq = http.request({ host: '127.0.0.1', port: localPort, path: msg.path, method: msg.method }, (localRes) => {
          const chunks = [];
          localRes.on('data', (c) => chunks.push(c));
          localRes.on('end', () => {
            ws.send(JSON.stringify({
              type: 'response', correlation_id: msg.correlation_id, status: localRes.statusCode,
              content_type: 'application/json', body_base64: Buffer.concat(chunks).toString('base64'),
            }));
          });
        });
        if (body.length) localReq.write(body);
        localReq.end();
      }
    });
    ws.on('error', reject);
  });
}

test('a real request through the tunnel reaches the local server and the response comes back correctly', async () => {
  const email = `tunnel-roundtrip-${RUN_ID}@internal.test`;
  const orgId = `org_tunnel_rt_${RUN_ID}`;
  const agentId = `agent_tunnel_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'tunnel test' }, { headers: { Cookie: sessionCookie } });
  assert.equal(connectRes.status, 200, JSON.stringify(connectRes.data));
  const apiKey = connectRes.data.api_key;

  const localServer = await startLocalEchoServer();
  const localPort = localServer.address().port;
  const { ws, publicUrl } = await connectSimulatedCli(orgId, agentId, apiKey, localPort);

  try {
    const publicPath = new URL(publicUrl).pathname; // e.g. /t/tn_xxxxxxxx
    const res = await client.post(`${publicPath}/webhooks/stripe`, { hello: 'world' });
    assert.equal(res.status, 200, JSON.stringify(res.data));
    assert.equal(res.data.method, 'POST');
    assert.equal(res.data.path, '/webhooks/stripe');
    assert.ok(res.data.bodyLength > 0);
  } finally {
    ws.close();
    localServer.close();
  }
});

test('opening a second tunnel for the same org closes the first', async () => {
  const email = `tunnel-single-${RUN_ID}@internal.test`;
  const orgId = `org_tunnel_single_${RUN_ID}`;
  const agentId = `agent_tunnel_single_${RUN_ID}`;
  const sessionCookie = await registerAndVerify(email, 'validpassword123', orgId);

  const connectRes = await client.post('/api/v1/agents/connect', { org_id: orgId, agent_id: agentId, label: 'tunnel test 2' }, { headers: { Cookie: sessionCookie } });
  const apiKey = connectRes.data.api_key;

  const localServer = await startLocalEchoServer();
  const localPort = localServer.address().port;

  const first = await connectSimulatedCli(orgId, agentId, apiKey, localPort);
  const closedFirst = new Promise((resolve) => first.ws.on('close', resolve));

  const second = await connectSimulatedCli(orgId, agentId, apiKey, localPort);

  await closedFirst; // the first connection must be closed by the server once the second opens

  try {
    // The second (current) tunnel still works.
    const publicPath = new URL(second.publicUrl).pathname;
    const res = await client.get(publicPath);
    assert.equal(res.status, 200, JSON.stringify(res.data));
  } finally {
    second.ws.close();
    localServer.close();
  }
});

test('a request to an unknown tunnel id 404s cleanly', async () => {
  const res = await client.get('/t/tn_doesnotexist');
  assert.equal(res.status, 404, JSON.stringify(res.data));
});
