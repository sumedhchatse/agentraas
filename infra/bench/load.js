// k6 load test: fixed arrival rate against the local stack, either straight
// to the built-in mock upstream (TARGET=direct) or through AgentRaaS
// (TARGET=via, the default). Run both at the same RATE and compare: the
// difference is what AgentRaaS adds under load. Every call has a unique
// payload, so each one does the full path (auth, dedup claim, caps, forward,
// audit write); DUP=1 sends one payload over and over to time cached replies.
// Set up with: python3 infra/scripts/bench-overhead.py --setup-only
import http from 'k6/http';
import { check } from 'k6';

const BASE = __ENV.BASE || 'http://localhost:13001';
const via = (__ENV.TARGET || 'via') === 'via';
const dup = __ENV.DUP === '1';

export const options = {
  summaryTrendStats: ['p(50)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    load: {
      executor: 'constant-arrival-rate',
      rate: Number(__ENV.RATE || 500),
      timeUnit: '1s',
      duration: __ENV.DURATION || '30s',
      preAllocatedVUs: 200,
      maxVUs: 1000,
    },
  },
};

// A fresh nonce per run, so a second run's payloads aren't already deduplicated by the first.
export function setup() {
  return { run: Date.now() };
}

export default function (data) {
  const n = dup ? 424242 : __VU * 10000000 + __ITER;
  const res = via
    ? http.post(`${BASE}/v1/webhook/${__ENV.ORG}/${__ENV.AGENT}`,
        JSON.stringify({ service: 'mockpay', action: 'payment.create', payload: { amount: n, run: dup ? 0 : data.run, fail: false } }),
        { headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${__ENV.KEY}` } })
    : http.post(`${BASE}/internal/mockpay`, JSON.stringify({ amount: n, fail: false }),
        { headers: { 'Content-Type': 'application/json' } });
  // Print the first non-200 reply per VU, so a failing run shows why (429, 503 breaker, 409 pending...).
  if (res.status !== 200 && !globalThis.reported) {
    globalThis.reported = true;
    console.warn(`${res.status} ${String(res.body).slice(0, 200)}`);
  }
  check(res, { '200': (r) => r.status === 200 });
}
