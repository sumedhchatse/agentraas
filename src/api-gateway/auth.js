const bcrypt = require('bcryptjs');

const SALT_ROUNDS = 12;
const LOGIN_ATTEMPT_LIMIT = 10;
const LOGIN_ATTEMPT_WINDOW_SECONDS = 15 * 60; // 15 minutes

async function hashPassword(password) {
  return bcrypt.hash(password, SALT_ROUNDS);
}

async function verifyPassword(password, hash) {
  return bcrypt.compare(password, hash);
}

function isValidEmail(email) {
  return typeof email === 'string' && /^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email);
}

function isValidPassword(password) {
  // Keep this in sync with whatever you tell users on the register form.
  return typeof password === 'string' && password.length >= 8;
}

// org_id / agent_id become part of URL paths (webhook route), Redis keys, and
// audit log rows — keep them short and to a safe charset so they can't be used
// to smuggle unexpected characters into any of those.
function isValidIdentifier(value) {
  return typeof value === 'string' && value.length > 0 && value.length <= 100 && /^[a-zA-Z0-9_-]+$/.test(value);
}

// Atomic per-key counter in Redis. Returns true if the caller is still under
// the limit (and should proceed), false if they've been rate limited.
// Keyed by IP + email so one bad actor can't lock out a real user's email,
// and a bot can't hammer many emails from one IP without also getting capped.
async function checkLoginRateLimit(redis, ip, email) {
  const key = `loginlimit:${ip}:${email}`;
  const attempts = await redis.incr(key);
  if (attempts === 1) {
    await redis.expire(key, LOGIN_ATTEMPT_WINDOW_SECONDS);
  }
  return attempts <= LOGIN_ATTEMPT_LIMIT;
}

async function clearLoginRateLimit(redis, ip, email) {
  await redis.del(`loginlimit:${ip}:${email}`);
}

module.exports = {
  hashPassword,
  verifyPassword,
  isValidEmail,
  isValidPassword,
  isValidIdentifier,
  checkLoginRateLimit,
  clearLoginRateLimit,
};
