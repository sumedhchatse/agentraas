#!/usr/bin/env node
// replay-traffic.js — sends a curated set of {service, action, payload}
// requests through AgentRaaS's webhook endpoint against a target
// instance (a staging/canary build, say), reporting pass/fail/timing.
//
// Scope note: this does NOT replay real historical production traffic —
// audit_log only ever stores a payload_hash (irreversible) and, for
// Enterprise+DLP orgs, a redacted text preview, never the actual
// payload bytes (see infra/migrations/020_audit_log_redacted_payload.sql
// — deliberate, so sensitive request bodies aren't retained
// indefinitely). A genuine "replay real traffic" tool would need a new,
// explicitly opt-in raw-payload capture table — a separate feature, not
// built here. This instead replays a hand-curated or externally-captured
// request set (JSON Lines), which is still useful for regression-testing
// a new build against a known traffic pattern before it goes live.
//
// Usage:
//   node replay-traffic.js --file requests.jsonl --url http://localhost:13001 \
//     --org myorg --agent myagent --key ar_live_...
//
// requests.jsonl: one JSON object per line, each {"service":"stripe","action":"charge.create","payload":{...}}

const fs = require('fs');
const readline = require('readline');

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    const key = argv[i].replace(/^--/, '');
    args[key] = argv[i + 1];
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['file', 'url', 'org', 'agent', 'key']) {
    if (!args[required]) {
      console.error(`Missing --${required}. Usage: node replay-traffic.js --file requests.jsonl --url http://host --org ORG --agent AGENT --key KEY`);
      process.exit(1);
    }
  }

  const webhookUrl = `${args.url.replace(/\/$/, '')}/v1/webhook/${encodeURIComponent(args.org)}/${encodeURIComponent(args.agent)}`;
  const rl = readline.createInterface({ input: fs.createReadStream(args.file) });

  let total = 0, ok = 0, failed = 0, errors = 0;
  const start = Date.now();

  for await (const line of rl) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    total++;
    let req;
    try {
      req = JSON.parse(trimmed);
    } catch {
      errors++;
      console.error(`line ${total}: not valid JSON, skipping`);
      continue;
    }
    const reqStart = Date.now();
    try {
      const res = await fetch(webhookUrl, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${args.key}` },
        body: JSON.stringify({ service: req.service, action: req.action, payload: req.payload || {} }),
      });
      const ms = Date.now() - reqStart;
      if (res.ok) {
        ok++;
      } else {
        failed++;
      }
      console.log(`[${total}] ${req.service}.${req.action} -> ${res.status} (${ms}ms)`);
    } catch (err) {
      errors++;
      console.error(`[${total}] ${req.service}.${req.action} -> request error: ${err.message}`);
    }
  }

  const totalMs = Date.now() - start;
  console.log(`\n${total} requests, ${ok} ok, ${failed} non-2xx, ${errors} request errors, ${totalMs}ms total`);
  process.exit(failed > 0 || errors > 0 ? 1 : 0);
}

main();
