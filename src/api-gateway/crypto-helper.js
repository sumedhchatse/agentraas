// ─── CREDENTIAL ENCRYPTION (AES-256-GCM) ───
// Each stored credential is "iv:authTag:ciphertext", all hex. GCM gives us
// authenticated encryption, so tampering with a stored row fails to decrypt
// rather than silently returning garbage.
//
// Pulled out of server.js so src/ee/auth (SsoManager) can encrypt/decrypt
// per-org OIDC client secrets with the exact same scheme service_credentials
// already uses, without requiring server.js back into ee/ code (that would
// be circular — server.js requires ee/auth, not the other way around).
const crypto = require('crypto');

const CREDENTIALS_ENCRYPTION_KEY_RAW = process.env.CREDENTIALS_ENCRYPTION_KEY;
if (!CREDENTIALS_ENCRYPTION_KEY_RAW) {
  console.error('CREDENTIALS_ENCRYPTION_KEY is not set. Generate one with: openssl rand -base64 32');
  process.exit(1);
}
const CREDENTIALS_ENCRYPTION_KEY = Buffer.from(CREDENTIALS_ENCRYPTION_KEY_RAW, 'base64');
if (CREDENTIALS_ENCRYPTION_KEY.length !== 32) {
  console.error('CREDENTIALS_ENCRYPTION_KEY must decode to exactly 32 bytes (base64 of a 256-bit key). Generate one with: openssl rand -base64 32');
  process.exit(1);
}

function encryptCredential(plaintext) {
  const iv = crypto.randomBytes(12);
  const cipher = crypto.createCipheriv('aes-256-gcm', CREDENTIALS_ENCRYPTION_KEY, iv);
  const encrypted = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()]);
  const authTag = cipher.getAuthTag();
  return [iv.toString('hex'), authTag.toString('hex'), encrypted.toString('hex')].join(':');
}

function decryptCredential(stored) {
  const [ivHex, tagHex, dataHex] = stored.split(':');
  const decipher = crypto.createDecipheriv('aes-256-gcm', CREDENTIALS_ENCRYPTION_KEY, Buffer.from(ivHex, 'hex'));
  decipher.setAuthTag(Buffer.from(tagHex, 'hex'));
  return Buffer.concat([decipher.update(Buffer.from(dataHex, 'hex')), decipher.final()]).toString('utf8');
}

module.exports = { encryptCredential, decryptCredential };
