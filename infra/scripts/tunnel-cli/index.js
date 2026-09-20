#!/usr/bin/env node
// agentraas-tunnel — CLI local dev tunnel (SPEC-TUNNEL.md). Local script
// for now, not published to npm (npm publishing itself is blocked on the
// account being locked this session) — run with:
//   node infra/scripts/tunnel-cli/index.js --port 13001 --org org_acme \
//     --agent agent_1 --key ar_live_... [--host agentraas.io]
//
// Opens a WebSocket to /api/v1/tunnel/connect/:org/:agent, prints the
// public URL it's given, and for every incoming "request" frame, replays
// it against http://localhost:<port> and sends the response back.

const http = require('http');
const https = require('https');
const WebSocket = require('ws');

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    args[argv[i].replace(/^--/, '')] = argv[i + 1];
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const port = args.port;
const org = args.org;
const agent = args.agent;
const key = args.key;
const host = args.host || 'agentraas.io';

if (!port || !org || !agent || !key) {
  console.error('Usage: node index.js --port <local-port> --org <org_id> --agent <agent_id> --key <api_key> [--host agentraas.io]');
  process.exit(1);
}

const wsUrl = `wss://${host}/api/v1/tunnel/connect/${encodeURIComponent(org)}/${encodeURIComponent(agent)}`;
const ws = new WebSocket(wsUrl, { headers: { Authorization: `Bearer ${key}` } });

ws.on('open', () => {
  console.log(`Connecting to ${host}...`);
});

ws.on('message', async (raw) => {
  let msg;
  try {
    msg = JSON.parse(raw.toString());
  } catch {
    return;
  }

  if (msg.type === 'connected') {
    console.log(`\n  Tunnel is live: ${msg.url}`);
    console.log(`  Forwarding to:  http://localhost:${port}\n`);
    return;
  }

  if (msg.type === 'request') {
    const startedAt = Date.now();
    const bodyBuf = msg.body_base64 ? Buffer.from(msg.body_base64, 'base64') : Buffer.alloc(0);
    const headers = { ...msg.headers };
    delete headers['host'];
    delete headers['content-length'];

    const localReq = http.request(
      { host: 'localhost', port: Number(port), path: msg.path, method: msg.method, headers },
      (localRes) => {
        const chunks = [];
        localRes.on('data', (c) => chunks.push(c));
        localRes.on('end', () => {
          const body = Buffer.concat(chunks);
          const ms = Date.now() - startedAt;
          console.log(`${msg.method} ${msg.path} -> ${localRes.statusCode} (${ms}ms)`);
          ws.send(
            JSON.stringify({
              type: 'response',
              correlation_id: msg.correlation_id,
              status: localRes.statusCode,
              content_type: localRes.headers['content-type'] || 'application/octet-stream',
              body_base64: body.toString('base64'),
            })
          );
        });
      }
    );
    localReq.on('error', (err) => {
      console.log(`${msg.method} ${msg.path} -> local connection failed: ${err.message}`);
      ws.send(
        JSON.stringify({
          type: 'response',
          correlation_id: msg.correlation_id,
          status: 502,
          content_type: 'text/plain',
          body_base64: Buffer.from(`agentraas-tunnel: could not reach localhost:${port} (${err.message})`).toString('base64'),
        })
      );
    });
    if (bodyBuf.length) localReq.write(bodyBuf);
    localReq.end();
  }
});

ws.on('close', (code, reason) => {
  console.log(`\nTunnel closed (${code}${reason ? ': ' + reason : ''}). Re-run this command to open a new one.`);
  process.exit(0);
});

ws.on('error', (err) => {
  console.error(`Connection error: ${err.message}`);
  process.exit(1);
});
