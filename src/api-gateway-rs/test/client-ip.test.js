// The per-IP rate limits must key on an address the client can't choose.
// Without CLIENT_IP_HEADER (the local default) that is the TCP peer, so a
// forged X-Forwarded-For must not show up in the rate-limit key.
// Run: node --test test/client-ip.test.js  (REDIS_URL pointing at the stack's Redis)

const test = require('node:test');
const assert = require('node:assert/strict');
const axios = require('axios');
const Redis = require('ioredis');

const BASE_URL = process.env.TEST_BASE_URL || 'http://localhost:3000';
const client = axios.create({ baseURL: BASE_URL, validateStatus: () => true });
const redis = new Redis(process.env.REDIS_URL || 'redis://localhost:6379');
test.after(() => redis.disconnect());

test('a forged X-Forwarded-For does not choose the rate-limit identity', async () => {
  const forged = `198.51.100.${Math.floor(Math.random() * 200) + 1}`;
  await client.post('/api/v1/auth/register', {}, { headers: { 'X-Forwarded-For': forged } });
  assert.equal(await redis.exists(`registerlimit:${forged}`), 0, 'forged address was trusted');
});
