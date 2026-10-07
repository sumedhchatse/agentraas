// Clears rate-limit counters and the circuit breakers between test files
// (run-each.sh). Back to back, the files register more accounts from one IP
// than the signup limit allows, and one file's deliberate failures would
// open the shared mockpay breaker for the next. Never run against production.
const Redis = require('ioredis');

(async () => {
  // No default: deleting keys must only ever hit the Redis it was pointed at
  // (on a dev laptop, 6379 can be another project's Redis).
  if (!process.env.REDIS_URL) throw new Error('REDIS_URL must be set');
  const redis = new Redis(process.env.REDIS_URL);
  for (const pattern of ['*limit*', 'circuit:*']) {
    let cursor = '0';
    do {
      const [next, keys] = await redis.scan(cursor, 'MATCH', pattern, 'COUNT', 500);
      cursor = next;
      if (keys.length) await redis.del(...keys);
    } while (cursor !== '0');
  }
  redis.disconnect();
})().catch((err) => { console.error(err); process.exit(1); });
