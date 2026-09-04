// Load .env before anything below reads process.env. Values already set by
// the container runtime (e.g. compose.yaml's environment: block) take
// precedence — dotenv never overrides an existing process.env value.
require('dotenv').config();

const fastify = require('fastify')({
  logger: true,
  // Safe because ar-api's port is bound to 127.0.0.1 only (see compose.yaml)
  // — nothing except something already running on this same server (i.e.
  // the reverse proxy, once deployed) can ever connect directly, so there's
  // no path for an external attacker to spoof the immediate peer address.
  trustProxy: true,
});

// Preserve the raw request body alongside the parsed JSON — Paddle webhook
// signature verification (see /api/v1/webhooks/paddle) must be computed
// over the exact bytes Paddle sent; re-serializing the parsed JSON object
// produces a different byte sequence and silently breaks verification.
// Harmless for every other route: this only adds request.rawBody, parsing
// still happens identically to Fastify's own default JSON parser.
fastify.addContentTypeParser('application/json', { parseAs: 'buffer' }, function (request, body, done) {
  request.rawBody = body;
  try {
    const json = body.length === 0 ? {} : JSON.parse(body);
    done(null, json);
  } catch (err) {
    err.statusCode = 400;
    done(err, undefined);
  }
});

// Same raw-body preservation for form-urlencoded bodies — Twilio sends
// webhooks as application/x-www-form-urlencoded, not JSON, so the parser
// above never runs for it. Without this, Twilio's inbound webhook
// signature verification would silently have no raw body to check against.
fastify.addContentTypeParser('application/x-www-form-urlencoded', { parseAs: 'buffer' }, function (request, body, done) {
  request.rawBody = body;
  try {
    const parsed = Object.fromEntries(new URLSearchParams(body.toString('utf8')));
    done(null, parsed);
  } catch (err) {
    err.statusCode = 400;
    done(err, undefined);
  }
});

const Redis = require('ioredis');
const { EventEmitter } = require('events');
const { Pool } = require('pg');
const crypto = require('crypto');
const axios = require('axios');
const dns = require('dns').promises;
const { URL } = require('url');
const nodemailer = require('nodemailer');
const archiver = require('archiver');
const fs = require('fs');
const path = require('path');
const { loadConfig, buildServiceRoutes, getValidationRules } = require('./config-loader');
const { validateFields, isValidRuleDefinition, isValidDedupRuleDefinition } = require('./validator');
const {
  hashPassword,
  verifyPassword,
  isValidEmail,
  isValidPassword,
  isValidIdentifier,
  checkLoginRateLimit,
  clearLoginRateLimit,
} = require('./auth');
const { Paddle, Environment } = require('@paddle/paddle-node-sdk');
const { encryptCredential, decryptCredential } = require('./crypto-helper');

// ─── Optional Enterprise (ee/) modules ───
// The MIT core repo and the commercial ee/ repo are separate (see
// RESTRUCTURE_PLAN.md's licensing section) — a pure core checkout has no
// src/ee directory at all, and this app must still boot and serve every
// Community-tier route normally. Each require below falls back to a stub
// that throws a clear error only if actually invoked — which, since every
// call site is already gated behind ENTERPRISE_MODE (or, for SsoManager,
// degrades to today's permissive Community behavior — see its stub below),
// should never happen in a real Community deployment.
function loadOptionalEe(modulePath, fallback) {
  try {
    return require(modulePath);
  } catch (err) {
    if (err.code === 'MODULE_NOT_FOUND') {
      fastify.log.warn(`Enterprise module ${modulePath} not installed — its features stay unavailable (this is expected for a Community-only checkout).`);
      return fallback;
    }
    throw err;
  }
}
function eeStub(featureName) {
  return () => { throw new Error(`${featureName} requires the Enterprise ee/ module, which isn't installed on this deployment.`); };
}

const { verifyWebhookSignature, timingSafeEqualStrings } = loadOptionalEe('./ee/hmac', {
  verifyWebhookSignature: eeStub('Inbound webhook verification'),
  timingSafeEqualStrings: (a, b) => a === b,
});
const { redactPII } = loadOptionalEe('./ee/dlp', { redactPII: eeStub('DLP redaction') });
const { SsoManager } = loadOptionalEe('./ee/auth', {
  // Matches today's Community behavior exactly when ee/auth is absent: no
  // org has any membership row, so every permission check that treats "no
  // row" as permissive (checkOrgWritePermission, getRole-based reads) just
  // behaves as if Enterprise RBAC never existed. Actual SSO/RBAC routes are
  // all gated behind requireEnterpriseMode and never reach this stub.
  SsoManager: class SsoManagerStub {
    constructor() {}
    async getRole() { return null; }
    static matchOrgByEmailDomain() { return false; }
    static mapClaimsToRole(_claims, defaultRole) { return defaultRole; }
    async listConfigsInternal() { return []; }
    async listConfigsForAdminView() { return []; }
    async getConfigById() { return null; }
    async findLoginConfig() { return null; }
    async createConfig() { return eeStub('Enterprise SSO')(); }
    async updateConfig() { return eeStub('Enterprise SSO')(); }
    async deleteConfig() {}
    async buildAuthorizationUrl() { return eeStub('Enterprise SSO')(); }
    async handleCallback() { return eeStub('Enterprise SSO')(); }
    async upsertMembership() {}
  },
});
const { MaintenanceQueue } = loadOptionalEe('./ee/maintenance', {
  MaintenanceQueue: class MaintenanceQueueStub {
    constructor() {}
    async isPaused() { return false; } // never paused if the module isn't installed — matches today's behavior
    async pause() { return eeStub('Pause & Buffer maintenance mode')(); }
    async resume() {}
    async queueLength() { return 0; }
    async enqueue() {}
    async drain() { return { processed: 0, failed: 0 }; }
  },
});
const { createProxy } = require('./core/proxy');
const { createMcp } = require('./core/mcp');
const { registerDashboardRoutes } = require('./core/dashboard');

// ─── PADDLE (Agency tier billing) ───
// PADDLE_API_KEY and PADDLE_WEBHOOK_SECRET only exist once the operator has
// created a real Paddle account and configured a notification destination —
// checkout/billing endpoints degrade to a clear 503 rather than crashing
// the whole server when they're unset (e.g. on a fresh self-host install,
// or before the cloud operator has finished Paddle onboarding).
const PADDLE_API_KEY = process.env.PADDLE_API_KEY || null;
const PADDLE_WEBHOOK_SECRET = process.env.PADDLE_WEBHOOK_SECRET || null;
const PADDLE_AGENCY_PRICE_ID = process.env.PADDLE_AGENCY_PRICE_ID || null;
const PADDLE_PRICE_ID_BY_PLAN = { agency: PADDLE_AGENCY_PRICE_ID };
const PADDLE_ENVIRONMENT = process.env.PADDLE_ENVIRONMENT === 'production' ? Environment.production : Environment.sandbox;
const paddleClient = PADDLE_API_KEY ? new Paddle(PADDLE_API_KEY, { environment: PADDLE_ENVIRONMENT }) : null;
if (!paddleClient) {
  fastify.log.warn('PADDLE_API_KEY not set — billing/checkout endpoints will return 503 until configured.');
}

// ─── SELF-HOST PACKAGE SNAPSHOT ───
// Reading directly from /repo (a read-only cross-container bind mount) on
// every download request proved unreliable — rootless Podman's UID/SELinux
// handling produced inconsistent EACCES errors on different subdirectories
// between requests (data/minio one time, infra/migrations the next), not
// tied to any one specific bad directory. Instead, copy the needed files
// ONCE at container startup into a location this container already reliably
// owns and reads from (alongside its own running code), and have the
// download endpoint read from that stable local copy instead.
const SELF_HOST_SNAPSHOT_DIR = '/tmp/.self-host-snapshot';
try {
  if (fs.existsSync('/repo')) {
    if (fs.existsSync(SELF_HOST_SNAPSHOT_DIR)) fs.rmSync(SELF_HOST_SNAPSHOT_DIR, { recursive: true, force: true });
    fs.mkdirSync(SELF_HOST_SNAPSHOT_DIR, { recursive: true });

    const snapshotDirs = ['src', 'infra', 'config'];
    const snapshotFiles = [
      'README.md', 'LICENSE.md', 'PRIVACY.md', 'TERMS.md', 'SECURITY.md',
      'CODE_OF_CONDUCT.md', 'CONTRIBUTING.md', 'GETTING_STARTED.md', 'compose.yaml', 'install.sh',
      '.env.example', '.gitignore',
    ];

    for (const dir of snapshotDirs) {
      const srcDir = path.join('/repo', dir);
      if (fs.existsSync(srcDir)) {
        fs.cpSync(srcDir, path.join(SELF_HOST_SNAPSHOT_DIR, dir), {
          recursive: true,
          filter: (srcPath) => {
            if (srcPath.includes('node_modules')) return false;
            if (srcPath.endsWith('.env')) return false;
            if (srcPath.endsWith('.log')) return false;
            return true;
          },
        });
      }
    }
    for (const file of snapshotFiles) {
      const srcFile = path.join('/repo', file);
      if (fs.existsSync(srcFile)) fs.copyFileSync(srcFile, path.join(SELF_HOST_SNAPSHOT_DIR, file));
    }
    fastify.log.info(`Self-host package snapshot created at ${SELF_HOST_SNAPSHOT_DIR}`);
  }
} catch (err) {
  fastify.log.warn(`Could not create self-host package snapshot — download endpoint may not work: ${err.message}`);
}

// ─── CONFIG ───
const PORT = process.env.PORT || 3000;
// The externally-reachable URL for this deployment — used to build links that
// go into emails (verification, password reset) and the Connect Agent modal's
// webhook/MCP URLs. This is NOT the same as PORT above: PORT is what the app
// listens on *inside* the container, but compose.yaml maps that to a
// different host port (13000 in this project's local dev setup), and the app
// has no way to know that mapping on its own — request.hostname drops the
// port entirely, and trusting a client-supplied Host header for something
// that goes into an email is a spoofing risk. Set this explicitly for any
// real deployment (e.g. https://agentraas.io in production).
const PUBLIC_URL = process.env.PUBLIC_URL || 'http://localhost:13000';
// LOCAL DEV / TEST CONVENIENCE ONLY — never set this in production. Normally
// dev_verify_url only appears in responses when SMTP isn't configured at all
// (the self-host-without-email fallback). But a dev machine can have BOTH
// real SMTP configured (for manually testing actual email delivery) AND a
// need for the automated test suite to work reliably regardless — this flag
// lets both coexist. Exposing this in a real production environment would
// let anyone skip proving they own an email address entirely.
const EXPOSE_DEV_VERIFY_URL = process.env.EXPOSE_DEV_VERIFY_URL === 'true';
const REDIS_URL = process.env.REDIS_URL || 'redis://localhost:6379';
const DATABASE_URL = process.env.DATABASE_URL || 'postgres://agentraas:devpassword@localhost:15432/agentraas';
const JWT_SECRET = process.env.JWT_SECRET;
const NODE_ENV = process.env.NODE_ENV || 'development';

if (!JWT_SECRET) {
  fastify.log.error('JWT_SECRET is not set. Refusing to start — set it in your env/secrets before running in production.');
  process.exit(1);
}

// CREDENTIALS_ENCRYPTION_KEY validation and encrypt/decryptCredential live in
// ./crypto-helper now — shared with src/ee/auth (SsoManager encrypts stored
// OIDC client secrets the same way) without a circular require back into
// server.js.

// Load service configuration from JSON file
let SERVICE_CONFIG;
let SERVICE_ROUTES;
let VALIDATION_RULES;

try {
  SERVICE_CONFIG = loadConfig();
  SERVICE_ROUTES = buildServiceRoutes(SERVICE_CONFIG);
  VALIDATION_RULES = getValidationRules(SERVICE_CONFIG);
  fastify.log.info(`Loaded ${Object.keys(SERVICE_ROUTES).length} service routes from config`);
} catch (err) {
  fastify.log.error(`Failed to load config: ${err.message}`);
  process.exit(1);
}

// Resolves the validation rule that actually applies to one service.action
// call, for one org: a dashboard-managed custom rule (see the Validation
// Rules panel and /api/v1/validation-rules below) always wins if one
// exists, falling back to the curated service's static config-driven rule
// (VALIDATION_RULES) otherwise. Custom Actions (service === 'custom') have
// no static fallback at all — a custom rule is the only way to validate
// one. Checked on every proxied request (proxy/mcp), so this stays a
// single indexed lookup rather than anything heavier.
async function getEffectiveValidationRule(orgId, service, action) {
  const custom = await pg.query(
    'SELECT fields FROM custom_validation_rules WHERE org_id = $1 AND service = $2 AND action = $3',
    [orgId, service, action]
  );
  if (custom.rows.length > 0) return { fields: custom.rows[0].fields };
  if (service === 'custom') return null;
  const staticRule = VALIDATION_RULES.find((r) => r.service === service && r.action === action);
  return staticRule ? { fields: staticRule.fields } : null;
}

// Resolves the per-field dedup rule that applies to one service.action
// call, for one org (see the Dedup Rules panel and /api/v1/dedup-rules
// below, and custom_dedup_rules) — analogous to getEffectiveValidationRule
// above, but there's no static/curated fallback: every action defaults to
// whole-payload-hash dedup (src/core/proxy's hashPayload) unless an org
// explicitly configures a field-based rule here. Checked on every proxied
// request (proxy/mcp) whenever no client-supplied idempotency key is
// present, so this stays a single indexed lookup.
async function getEffectiveDedupRule(orgId, service, action) {
  const custom = await pg.query(
    'SELECT fields FROM custom_dedup_rules WHERE org_id = $1 AND service = $2 AND action = $3',
    [orgId, service, action]
  );
  return custom.rows.length > 0 ? { fields: custom.rows[0].fields } : null;
}

const redis = new Redis(REDIS_URL);
const pg = new Pool({ connectionString: DATABASE_URL });

// ─── INSTANT OUTAGE NOTIFICATIONS (Slack / Discord / Telegram) ───
// Fires when the circuit breaker blocks a request for a service this org
// actually uses — not a global broadcast (the circuit itself is shared
// across every org calling that service; see recordFailure/getCircuitState
// in src/core/proxy), only orgs actually affected. Rate-limited to once
// per org+service per 60s window (matching the circuit's own half-open
// retry cadence) via Redis SETNX, so a burst of blocked requests during
// one outage sends one notification, not one per request.
async function sendOutageNotification(orgId, message) {
  const rows = await pg.query('SELECT type, encrypted_target, extra FROM notification_webhooks WHERE org_id = $1', [orgId]);
  for (const row of rows.rows) {
    try {
      const target = decryptCredential(row.encrypted_target);
      if (row.type === 'slack') {
        await axios.post(target, { text: message }, { timeout: 5000 });
      } else if (row.type === 'discord') {
        await axios.post(target, { content: message }, { timeout: 5000 });
      } else if (row.type === 'telegram') {
        await axios.post(`https://api.telegram.org/bot${target}/sendMessage`, { chat_id: row.extra, text: message }, { timeout: 5000 });
      }
    } catch (err) {
      fastify.log.warn(`Outage notification failed (org=${orgId}, type=${row.type}): ${err.message}`);
    }
  }
}

async function notifyCircuitOpen(orgId, service) {
  const claimed = await redis.set(`circuit-notified:${orgId}:${service}`, '1', 'EX', 60, 'NX');
  if (!claimed) return; // already notified for this org+service within the last 60s
  await sendOutageNotification(
    orgId,
    `⚠️ AgentRaaS Circuit Breaker Activated: ${service} is unresponsive. Requests to it are being shielded for your org until it recovers.`
  );
}

// ─── ACTIVE HEALTH CHECKS (proactive, opt-in, per-org) ───
// Separate from the passive circuit breaker (src/core/proxy), which only
// reacts to real agent traffic — this pings a service directly, on a timer,
// using an org's own stored credentials, so a dead/revoked key or an
// outage surfaces before an agent's real request ever hits it.
//
// Only a small, deliberately curated set of services: each entry below is
// a genuinely read-only, side-effect-free, well-established endpoint
// (Stripe's balance check, Slack's auth.test) — guessing wrong here would
// mean firing an unverified request at a live production account,
// automatically, on a schedule. Every other configured service simply
// isn't eligible; the reliability report (traffic-observed, not active)
// still covers them.
//
// Deliberately per-org and never fed into the shared circuit:<service>
// state — see the comment on migration 031 for why: a failing check here
// usually means THIS org's credential went bad, not a service-wide outage,
// and tripping the shared breaker off one org's stale key would block
// every other org's real traffic too.
const HEALTH_CHECK_SPECS = {
  stripe: { method: 'GET', url: 'https://api.stripe.com/v1/balance', authHeader: 'Authorization', contentType: 'application/x-www-form-urlencoded' },
  slack: { method: 'POST', url: 'https://slack.com/api/auth.test', authHeader: 'Authorization', contentType: 'application/json' },
  mockpay: { method: 'POST', url: 'http://localhost:3000/internal/mockpay', authType: 'none', internal: true, contentType: 'application/json' },
};
const HEALTH_CHECK_INTERVAL_MS = 5 * 60 * 1000;

async function runHealthChecks() {
  let settings;
  try {
    settings = await pg.query(`SELECT org_id, service FROM health_check_settings`);
  } catch (err) {
    fastify.log.error(`Health check run failed to load settings: ${err.message}`);
    return;
  }
  await Promise.all(settings.rows.map(async ({ org_id, service }) => {
    const spec = HEALTH_CHECK_SPECS[service];
    if (!spec) return; // defensive — enabling is already gated to known specs
    const start = Date.now();
    let ok = true, error = null;
    try {
      await proxy.forwardAction(spec, service, 'health_check', org_id, service === 'mockpay' ? { amount: 1, fail: false } : {}, 'healthcheck_' + crypto.randomBytes(6).toString('hex'));
    } catch (err) {
      ok = false;
      error = (extractUpstreamErrorMessage(err.response?.data) || err.message || '').slice(0, 500);
    }
    const latencyMs = Date.now() - start;
    pg.query(
      `INSERT INTO health_check_results (org_id, service, ok, latency_ms, error) VALUES ($1, $2, $3, $4, $5)`,
      [org_id, service, ok, latencyMs, error]
    ).catch((err) => fastify.log.warn({ err }, 'Health check result write failed'));

    const notifiedKey = `healthcheck-notified:${org_id}:${service}`;
    if (!ok) {
      const claimed = await redis.set(notifiedKey, '1', 'EX', 1800, 'NX'); // at most one failure alert per org+service per 30min
      if (claimed) {
        sendOutageNotification(org_id, `🔴 AgentRaaS Active Monitoring: your ${service} credentials failed an automated health check (${error}). This checks your stored credentials directly — separate from live traffic.`).catch(() => {});
      }
    } else {
      const wasNotified = await redis.get(notifiedKey);
      if (wasNotified) {
        await redis.del(notifiedKey);
        sendOutageNotification(org_id, `✅ AgentRaaS Active Monitoring: your ${service} health check is passing again.`).catch(() => {});
      }
    }
  }));
}

// Whether the Enterprise migrations (021+, org_members/orgs/sso_configs/
// org_invites) have been applied — a pure Community/core-only checkout
// never runs them, so anything referencing those tables (getUserOrgIds)
// checks this instead of assuming the schema exists. Checked once at
// startup; the brief window before this resolves treats it as false
// (correct for a Community deployment, and requests essentially never
// arrive before this single fast query does).
let hasEnterpriseSchema = false;
pg.query(`SELECT to_regclass('public.org_members') IS NOT NULL AS exists`)
  .then((r) => { hasEnterpriseSchema = r.rows[0]?.exists === true; })
  .catch(() => { hasEnterpriseSchema = false; });

// ─── Real-time usage updates (SSE) ───
// A subscribed ioredis connection can't run normal commands, so this is a
// second, dedicated connection purely for pub/sub — publishing itself still
// uses the main `redis` client. One Redis subscription fans out in-process
// to every open SSE connection via usageEvents, rather than each SSE
// connection opening its own Redis subscription.
const usagePubSub = new Redis(REDIS_URL);
const usageEvents = new EventEmitter();
usagePubSub.subscribe('usage:updates').catch((err) => fastify.log.error({ err }, 'Failed to subscribe to usage:updates'));
usagePubSub.on('message', (channel, message) => {
  if (channel !== 'usage:updates') return;
  try { usageEvents.emit('update', JSON.parse(message)); } catch (err) { /* ignore malformed message */ }
});
const ssoManager = new SsoManager(pg, { encryptCredential, decryptCredential });
const maintenanceQueue = new MaintenanceQueue(redis);

// Email is optional. If SMTP isn't configured, forgot-password still works —
// the reset link is logged server-side instead of emailed, which is fine for
// local/dev use but means production needs real SMTP env vars set to actually
// deliver the email to the user.
const SMTP_HOST = process.env.SMTP_HOST;
const mailTransport = SMTP_HOST
  ? nodemailer.createTransport({
      host: SMTP_HOST,
      port: parseInt(process.env.SMTP_PORT || '587', 10),
      secure: process.env.SMTP_PORT === '465',
      auth: process.env.SMTP_USER ? { user: process.env.SMTP_USER, pass: process.env.SMTP_PASS } : undefined,
    })
  : null;

// Shared email template — inline styles and a table-based layout, since many
// email clients strip <style> blocks and don't support modern CSS (flexbox,
// grid). White background rather than the dashboard's dark theme: dark
// backgrounds render inconsistently across email clients, and a plain white
// email is the safer, more standard choice.
// Server-side HTML escaping for values interpolated into email bodies (e.g.
// the recipient's own email address) — an email is rendered by a mail
// client, not executed with any privilege, so the practical risk here is
// low, but escaping user-supplied strings before they land in HTML is
// correct practice regardless.
function escapeHtml(str) {
  if (str === null || str === undefined) return '';
  return String(str)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function buildEmailHtml({ heading, bodyHtml, ctaText, ctaUrl, expiryNote }) {
  return `<!DOCTYPE html>
<html><head><meta charset="utf-8"></head>
<body style="margin:0;padding:0;background:#F3F1EC;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#F3F1EC;padding:40px 20px;">
<tr><td align="center">
<table role="presentation" width="480" cellpadding="0" cellspacing="0" style="background:#FFFFFF;border-radius:14px;overflow:hidden;max-width:480px;width:100%;">
  <tr><td style="background:#0A0D14;padding:24px 32px;">
    <table role="presentation" cellpadding="0" cellspacing="0"><tr>
      <td style="width:28px;height:28px;background:#00E0A8;border-radius:8px;text-align:center;vertical-align:middle;font-size:14px;">🛡️</td>
      <td style="padding-left:10px;color:#FFFFFF;font-size:17px;font-weight:700;">AgentRaaS</td>
    </tr></table>
  </td></tr>
  <tr><td style="padding:36px 32px 28px;">
    <h1 style="margin:0 0 16px;font-size:21px;color:#14171F;">${heading}</h1>
    <div style="font-size:15px;line-height:1.6;color:#3A4152;margin-bottom:28px;">${bodyHtml}</div>
    <table role="presentation" cellpadding="0" cellspacing="0"><tr>
      <td style="background:#00E0A8;border-radius:999px;">
        <a href="${ctaUrl}" style="display:inline-block;padding:13px 28px;color:#06231C;font-weight:700;font-size:15px;text-decoration:none;">${ctaText}</a>
      </td>
    </tr></table>
    <p style="font-size:13px;color:#8B93A6;margin-top:24px;line-height:1.5;">
      Or copy and paste this link into your browser:<br>
      <a href="${ctaUrl}" style="color:#059669;word-break:break-all;">${ctaUrl}</a>
    </p>
    ${expiryNote ? `<p style="font-size:13px;color:#8B93A6;margin-top:16px;">${expiryNote}</p>` : ''}
  </td></tr>
  <tr><td style="padding:20px 32px;background:#F9F8F5;border-top:1px solid #E4E0D6;">
    <p style="font-size:12.5px;color:#8B93A6;margin:0;line-height:1.5;">
      If you didn't request this, you can safely ignore this email.
      Questions? Reach us at <a href="mailto:support@agentraas.io" style="color:#059669;">support@agentraas.io</a>.
    </p>
  </td></tr>
</table>
</td></tr>
</table>
</body></html>`;
}

async function sendPasswordResetEmail(toEmail, resetUrl) {
  if (!mailTransport) {
    fastify.log.warn(`SMTP not configured — password reset link for ${toEmail}: ${resetUrl}`);
    return;
  }
  try {
    await mailTransport.sendMail({
      from: process.env.SMTP_FROM || 'AgentRaaS <no-reply@agentraas.local>',
      to: toEmail,
      subject: 'Reset your AgentRaaS password',
      text: `We received a request to reset your AgentRaaS password.\n\nReset it here: ${resetUrl}\n\nThis link expires in 1 hour and works once. If you didn't request this, you can safely ignore this email.`,
      html: buildEmailHtml({
        heading: 'Reset your password',
        bodyHtml: `We received a request to reset the password on your AgentRaaS account (<strong>${escapeHtml(toEmail)}</strong>). Click the button below to choose a new one.`,
        ctaText: 'Reset password',
        ctaUrl: resetUrl,
        expiryNote: 'This link expires in 1 hour and works once.',
      }),
    });
  } catch (err) {
    fastify.log.error({ err }, 'Failed to send password reset email');
  }
}

async function sendVerificationEmail(toEmail, verifyUrl) {
  if (!mailTransport) {
    // Self-hosted without SMTP configured: the operator can still verify by
    // checking their own server logs — they own the server, this is a
    // reasonable fallback rather than a hard block.
    fastify.log.warn(`SMTP not configured — email verification link for ${toEmail}: ${verifyUrl}`);
    return;
  }
  try {
    await mailTransport.sendMail({
      from: process.env.SMTP_FROM || 'AgentRaaS <no-reply@agentraas.local>',
      to: toEmail,
      subject: 'Verify your AgentRaaS account',
      text: `Welcome to AgentRaaS! Verify your email to activate your account.\n\nVerify here: ${verifyUrl}\n\nThis link expires in 24 hours. If you didn't create this account, you can safely ignore this email.`,
      html: buildEmailHtml({
        heading: 'Verify your email',
        bodyHtml: `Welcome to AgentRaaS — the exactly-once execution layer for AI agents. Click the button below to verify <strong>${escapeHtml(toEmail)}</strong> and activate your account.`,
        ctaText: 'Verify email',
        ctaUrl: verifyUrl,
        expiryNote: 'This link expires in 24 hours.',
      }),
    });
  } catch (err) {
    fastify.log.error({ err }, 'Failed to send verification email');
  }
}

// ─── AUTH PLUGINS ───
fastify.register(require('@fastify/cookie'));
fastify.register(require('@fastify/jwt'), {
  secret: JWT_SECRET,
  cookie: { cookieName: 'ar_session', signed: false },
});

// preHandler for any route that requires a logged-in dashboard user.
async function requireAuth(request, reply) {
  try {
    await request.jwtVerify();
  } catch (err) {
    return reply.status(401).send({ error: 'Not authenticated' });
  }
}

// Checks admin status fresh from the DB on every request, rather than baking
// it into the JWT — if someone's admin access is revoked, that takes effect
// immediately instead of waiting up to 7 days for their token to expire.
async function requireAdmin(request, reply) {
  const result = await pg.query('SELECT is_admin FROM users WHERE id = $1', [request.user.sub]);
  if (result.rows.length === 0 || !result.rows[0].is_admin) {
    return reply.status(403).send({ error: 'Admin access required.' });
  }
}
const requireAdminRateLimited = [requireAuth, dashboardRateLimit, requireAdmin];

// Per-org admin check for Enterprise SSO/RBAC (src/ee/auth) — a DIFFERENT
// axis from requireAdmin/is_admin above (one system-wide cloud-operator
// account). This checks org_members.role for the org named in the route
// (request.params.orgId), fresh from the DB every time, same "never trust
// a cached/JWT-carried role" reasoning as requireAdmin. Low-QPS path (org
// admins configuring SSO, not a hot per-request check), so the extra query
// per call is not a concern.
async function requireOrgAdmin(request, reply) {
  const orgId = request.params.orgId;
  const role = await ssoManager.getRole(request.user.sub, orgId);
  if (role === 'admin') return;

  // Bootstrap fallback: a brand-new org has no org_members admin row yet
  // (nobody can be its org-admin before anyone's ever accessed org-admin
  // features for it). Fall back to the existing loose ownership notion
  // (getUserOrgIds) so whoever already owns this org_id in the legacy
  // sense can bootstrap it — checked as "no ADMIN exists yet", not "no
  // members at all", since an org can otherwise pick up non-admin members
  // (e.g. an invited auditor) before its owner ever exercises admin access.
  // Granting access here also cements it into a real admin membership row,
  // so this is a one-time bootstrap per org, not a standing fallback that
  // has to keep re-deriving from legacy ownership on every single call.
  const adminExists = await pg.query(`SELECT 1 FROM org_members WHERE org_id = $1 AND role = 'admin' LIMIT 1`, [orgId]);
  if (adminExists.rows.length === 0) {
    const ownedOrgIds = await getUserOrgIds(request.user.sub);
    if (ownedOrgIds.includes(orgId)) {
      // org_members.org_id FKs to orgs.org_id — this may be the very
      // first Enterprise touch this org_id has ever had, so the orgs row
      // might not exist yet (normally created by whichever endpoint runs
      // next, e.g. createConfig/invites — but that's too late, this
      // upsert needs it to exist right now).
      await pg.query(`INSERT INTO orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING`, [orgId]);
      await ssoManager.upsertMembership(request.user.sub, orgId, 'admin');
      return;
    }
  }

  return reply.status(403).send({ error: 'Org admin access required.' });
}
const requireOrgAdminRateLimited = [requireAuth, dashboardRateLimit, requireOrgAdmin];

const COOKIE_OPTS = {
  httpOnly: true,
  secure: NODE_ENV === 'production',
  sameSite: 'lax',
  path: '/',
  maxAge: 60 * 60 * 24 * 7, // 7 days
};

// ─── HELPERS ───
// hashPayload/generateRequestId moved into src/core/proxy (used only by
// handleRequest/handleMCP, both moved there too).

// Audit logs record which key was used for debugging/traceability, but must never
// store or expose the working secret itself — that would let anyone with dashboard
// access (or DB access) recover a live agent's real API key from its own activity log.
function maskApiKeyForAudit(apiKey) {
  if (!apiKey || apiKey === 'anonymous') return apiKey || 'anonymous';
  return apiKey.length > 8 ? `${apiKey.slice(0, 8)}…` : '••••';
}

function maskedPreview(credentials) {
  const val = credentials.api_key || credentials.username || Object.values(credentials)[0] || '';
  if (!val || typeof val !== 'string') return '••••';
  return val.length > 8 ? `${val.slice(0, 4)}••••${val.slice(-4)}` : '••••';
}

// Upstream services report errors in different shapes: some nest as
// {error: {message: "..."}} (Stripe-style), others respond with a flat
// {error: "..."} string. Handle both so real error text actually surfaces
// instead of silently falling through to a generic message.
function extractUpstreamErrorMessage(responseData) {
  if (!responseData) return undefined;
  if (typeof responseData.error === 'string') return responseData.error;
  if (typeof responseData.error?.message === 'string') return responseData.error.message;
  return undefined;
}

// ─── SSRF guard for custom action target URLs ───
// Only runs at registration time (by the trusted, logged-in dashboard user) —
// agents never supply a live target URL themselves, only invoke by name. This
// still blocks a human from accidentally (or maliciously) pointing a custom
// action at internal infrastructure.
const BLOCKED_HOSTNAMES = new Set(['localhost', 'ar-postgres', 'ar-redis', 'ar-minio', 'ar-api']);

function isPrivateOrReservedIp(ip) {
  if (/^127\./.test(ip)) return true;               // loopback
  if (/^10\./.test(ip)) return true;                 // private
  if (/^192\.168\./.test(ip)) return true;            // private
  if (/^169\.254\./.test(ip)) return true;            // link-local / cloud metadata range
  if (/^172\.(1[6-9]|2\d|3[0-1])\./.test(ip)) return true; // private
  if (ip === '::1') return true;                      // IPv6 loopback
  if (/^fc00:/i.test(ip) || /^fe80:/i.test(ip)) return true; // IPv6 private/link-local
  return false;
}

async function validateTargetUrl(targetUrl) {
  let parsed;
  try { parsed = new URL(targetUrl); } catch { return 'Invalid URL.'; }
  if (!['http:', 'https:'].includes(parsed.protocol)) return 'Only http:// and https:// URLs are allowed.';
  const hostname = parsed.hostname.toLowerCase();
  if (BLOCKED_HOSTNAMES.has(hostname) || hostname.endsWith('.local')) {
    return 'Local or internal hostnames are not allowed.';
  }
  try {
    const addresses = await dns.lookup(hostname, { all: true });
    for (const addr of addresses) {
      if (isPrivateOrReservedIp(addr.address)) {
        return 'Target resolves to a private/internal IP address, which is not allowed.';
      }
    }
  } catch (err) {
    return 'Could not resolve the target hostname.';
  }
  return null; // valid
}

// ─── PUBLIC TOOL: "Is My Webhook Safe?" audit ─── unauthenticated lead-magnet
// (see /webhook-audit page) — fires 3 identical POSTs at a URL the visitor
// supplies and reports whether the responses look idempotent. Two things
// this needs precisely because it's unauthenticated and takes a
// caller-supplied URL: the same SSRF guard as custom-action registration
// (validateTargetUrl, above — blocks localhost/private/link-local/metadata
// IPs) and an IP rate limit (reusing the login limiter's 10-per-15-min
// bucket) so this endpoint can't be turned into an open outbound-request
// proxy or SSRF/DoS-scanning tool.
fastify.post('/api/v1/tools/webhook-audit', async (request, reply) => {
  const { url } = request.body || {};
  if (typeof url !== 'string' || url.length === 0 || url.length > 2000) {
    return reply.status(422).send({ error: 'A webhook URL is required.' });
  }
  const underLimit = await checkLoginRateLimit(redis, request.ip, 'webhook-audit');
  if (!underLimit) {
    return reply.status(429).send({ error: 'Too many audits from this IP. Try again in 15 minutes.' });
  }
  const urlError = await validateTargetUrl(url);
  if (urlError) return reply.status(422).send({ error: urlError });

  // Identical payload across all 3 calls (same test_id) — this is the
  // "retry storm" the tool is simulating: an agent/workflow retrying the
  // exact same action, not 3 different actions.
  const testId = crypto.randomBytes(8).toString('hex');
  const payload = {
    agentraas_webhook_audit: true,
    note: 'Free idempotency test from AgentRaaS (agentraas.io/webhook-audit) — 3 identical requests fired in parallel to check whether this endpoint deduplicates retries. Safe to ignore or discard.',
    test_id: testId,
    amount: 100,
    currency: 'usd',
  };

  async function fireOne(attempt) {
    const start = Date.now();
    try {
      const res = await axios.post(url, payload, { timeout: 8000, validateStatus: () => true, maxRedirects: 3 });
      return { attempt, status: res.status, latency_ms: Date.now() - start, body_snippet: JSON.stringify(res.data ?? '').slice(0, 500) };
    } catch (err) {
      return { attempt, status: null, latency_ms: Date.now() - start, error: err.code || err.message };
    }
  }

  const results = await Promise.all([fireOne(1), fireOne(2), fireOne(3)]);
  const anyErrored = results.some((r) => r.status === null);
  const allSucceeded = results.every((r) => typeof r.status === 'number' && r.status < 500);
  const bodies = results.map((r) => (r.status === null ? `__error:${r.error}` : r.body_snippet));
  const allIdentical = bodies.every((b) => b === bodies[0]);

  let verdict, verdict_label;
  if (anyErrored) {
    verdict = 'inconclusive';
    verdict_label = "One or more requests didn't complete (network error or timeout) — try again, or double-check the URL.";
  } else if (!allSucceeded) {
    verdict = 'inconclusive';
    verdict_label = 'The endpoint returned a server error, so duplicate-processing risk could not be determined from this run.';
  } else if (allIdentical) {
    verdict = 'likely_safe';
    verdict_label = 'All 3 identical requests got back the exact same response — consistent with idempotent handling.';
  } else {
    verdict = 'vulnerable';
    verdict_label = 'The 3 identical requests got back 3 different responses — consistent with 3 separate records/charges being created.';
  }

  return {
    url, test_id: testId, verdict, verdict_label, results,
    disclaimer: 'This is an HTTP-level heuristic based only on the responses your endpoint sent back — we cannot see your database. "Likely safe" is not a guarantee; "vulnerable" is strong evidence, not certainty.',
  };
});

// Agent-facing rate limit (webhook/SDK/MCP) — token-bucket implementation
// now lives in src/core/proxy (checkAgentRateLimit), constructed below via
// createProxy() once its other dependencies exist.
const AGENT_RATE_LIMIT_PER_MIN = parseInt(process.env.AGENT_RATE_LIMIT_PER_MIN || '300', 10);

// Automatic retry-with-backoff for transient upstream failures (network
// errors, 429, 5xx) — see forwardWithRetry in src/core/proxy. 3 total
// attempts, ~300/600ms backoff by default: enough to ride out a brief blip
// without making a synchronous caller wait too long for a service that's
// genuinely down (the circuit breaker, not retry count, is what protects
// against that case).
const PROXY_RETRY_MAX_ATTEMPTS = parseInt(process.env.PROXY_RETRY_MAX_ATTEMPTS || '3', 10);
const PROXY_RETRY_BASE_DELAY_MS = parseInt(process.env.PROXY_RETRY_BASE_DELAY_MS || '300', 10);

// Separate, more generous limit for the dashboard's own API (a logged-in human
// clicking around, or a compromised session script) — distinct from the
// agent-traffic limiter above since the two have very different normal request
// rates. Keyed by user id, since requireAuth runs first in the preHandler chain.
const DASHBOARD_RATE_LIMIT_PER_MIN = parseInt(process.env.DASHBOARD_RATE_LIMIT_PER_MIN || '120', 10);
async function dashboardRateLimit(request, reply) {
  const windowKey = `ratelimit:dashboard:${request.user.sub}:${Math.floor(Date.now() / 60000)}`;
  const count = await redis.incr(windowKey);
  if (count === 1) await redis.expire(windowKey, 65);
  if (count > DASHBOARD_RATE_LIMIT_PER_MIN) {
    return reply.status(429).send({ error: 'Rate limit exceeded. Slow down and try again shortly.' });
  }
}
// Convenience: every authenticated dashboard route uses this same pair, in order —
// requireAuth first so dashboardRateLimit can rely on request.user being set.
const requireAuthRateLimited = [requireAuth, dashboardRateLimit];

// ─── Usage limits (see LICENSE.md) ───
// DEPLOYMENT_MODE decides whether a limit applies at all:
//   'self-hosted' (default) — unlimited, on every plan. Per LICENSE.md
//     Section 2, self-hosted deployments have no usage limit — contractual
//     or technical — regardless of tier.
//   'cloud' — set this only on a deployment YOU operate. The 500/month
//     free-tier limit (Agency/Enterprise raise it) is then actually
//     enforced, matching LICENSE.md Section 2's statement that
//     Licensor-operated instances enforce it technically.
const DEPLOYMENT_MODE = process.env.DEPLOYMENT_MODE || 'self-hosted';

// ─── Enterprise tier gate ───
// A different axis from DEPLOYMENT_MODE (cloud vs self-hosted) — this is
// "is the Enterprise tier actually enabled on this deployment," set by
// compose.ee.yaml. Previously declared but never read anywhere (all four
// ee/ modules ran unconditionally regardless of tier) — this is that gate.
// Community-tier behavior (everything except the four gated things below)
// is completely unaffected either way.
const ENTERPRISE_MODE = process.env.ENTERPRISE_MODE === 'true';
async function requireEnterpriseMode(request, reply) {
  if (!ENTERPRISE_MODE) {
    return reply.status(403).send({ error: 'This is an Enterprise-tier feature. Set ENTERPRISE_MODE=true (see compose.ee.yaml) to enable it.' });
  }
}
const CLOUD_MONTHLY_LIMIT = parseInt(process.env.CLOUD_MONTHLY_LIMIT || '500', 10);

function currentMonthKey() {
  const d = new Date();
  return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, '0')}`;
}

// Counts only genuine fresh successful executions — not cached replays,
// blocked, or errored requests — matching "forwarded agent actions" in
// LICENSE.md Section 2.
async function incrementMonthlyUsage(orgId) {
  const key = `usage:${orgId}:${currentMonthKey()}`;
  const count = await redis.incr(key);
  if (count === 1) await redis.expire(key, 60 * 60 * 24 * 40); // safety-net TTL; a new month uses a new key anyway
  // Fire-and-forget — a dashboard tab watching this org via SSE (see
  // /api/v1/usage/stream) updates instantly instead of waiting for its next
  // poll. Never blocks or fails the actual request on a publish error.
  redis.publish('usage:updates', JSON.stringify({ org_id: orgId, total: count })).catch(() => {});
  return count;
}

async function getMonthlyUsage(orgId) {
  const val = await redis.get(`usage:${orgId}:${currentMonthKey()}`);
  return parseInt(val || '0', 10);
}

// Every org a user owns or belongs to, via any of the four ways that gets
// established (a connected agent's api_keys row, a custom action, a saved
// credential, or Enterprise org_members — the latter covers SSO/invite-
// provisioned members whose own users.org_id may point somewhere else
// entirely). Every dashboard-facing endpoint needs this to scope its query
// to only the current user's own data — without it, one user's dashboard
// would show every user's activity, which is a real isolation bug.
async function getUserOrgIds(userId) {
  // org_members only exists once the Enterprise migrations (021+) have run
  // — a pure Community/core-only deployment never applies them, so this
  // clause is conditional rather than a hard schema dependency (see
  // hasEnterpriseSchema below).
  const orgMembersClause = hasEnterpriseSchema ? ` UNION SELECT DISTINCT org_id FROM org_members WHERE user_id = $1` : '';
  const result = await pg.query(
    `SELECT org_id FROM users WHERE id = $1 AND org_id IS NOT NULL
     UNION SELECT DISTINCT org_id FROM api_keys WHERE user_id = $1
     UNION SELECT DISTINCT org_id FROM custom_actions WHERE user_id = $1
     UNION SELECT DISTINCT org_id FROM service_credentials WHERE user_id = $1
     UNION SELECT DISTINCT org_id FROM custom_validation_rules WHERE created_by = $1
     UNION SELECT DISTINCT org_id FROM custom_dedup_rules WHERE created_by = $1${orgMembersClause}`,
    [userId]
  );
  return result.rows.map((r) => r.org_id);
}

// Enterprise RBAC write gate: an 'auditor' org_members row means read-only
// for that org. No membership row at all (the overwhelming majority of
// orgs — Community-tier, never touched SSO/invites) falls back to today's
// permissive behavior unchanged: any logged-in user can write under any
// org_id they name, exactly as before this feature existed. This only
// starts restricting anything once an org has actually opted into
// Enterprise membership.
async function checkOrgWritePermission(userId, orgId) {
  const role = await ssoManager.getRole(userId, orgId);
  return role !== 'auditor';
}

// Only actually blocks anything when DEPLOYMENT_MODE=cloud — on a self-hosted
// deployment this always returns ok:true, since enforcement there isn't
// technically meaningful (see the note on DEPLOYMENT_MODE above). Orgs owned
// by an admin are exempt entirely, even in cloud mode — admins doing
// maintenance/testing shouldn't get blocked by the same cap regular users are.

// The plan ('free' | 'agency') of whichever user owns a given org — checked via
// the same three ownership paths used everywhere else (default org_id on
// the user row, or via api_keys/custom_actions/service_credentials).
// Defaults to 'free' if no owner is found (shouldn't normally happen, but
// fail toward the more restrictive limit rather than the generous one).
async function getOrgOwnerPlan(orgId) {
  const result = await pg.query(
    `SELECT plan FROM users WHERE org_id = $1
     UNION SELECT u.plan FROM users u JOIN api_keys a ON a.user_id = u.id WHERE a.org_id = $1
     UNION SELECT u.plan FROM users u JOIN custom_actions c ON c.user_id = u.id WHERE c.org_id = $1
     UNION SELECT u.plan FROM users u JOIN service_credentials s ON s.user_id = u.id WHERE s.org_id = $1
     LIMIT 1`,
    [orgId]
  );
  return result.rows.length > 0 ? result.rows[0].plan : 'free';
}

// Agency tier ($149/mo, up to 10 client tenants, white-label dashboard,
// outbound rate smoothing) — available cloud-hosted (via PADDLE_AGENCY_PRICE_ID
// checkout below) or self-hosted (an operator can assign it directly, the
// same way org_limit_overrides is set via infra/scripts/set-org-limit.sh).
const PLAN_MONTHLY_LIMITS = {
  free: CLOUD_MONTHLY_LIMIT,
  agency: parseInt(process.env.AGENCY_MONTHLY_LIMIT || '50000', 10),
};
const PLAN_RATE_LIMITS = {
  free: AGENT_RATE_LIMIT_PER_MIN,
  agency: parseInt(process.env.AGENCY_RATE_LIMIT_PER_MIN || '2000', 10),
};
// "Up to 10 Client Tenants" per the pricing card — enforced when an agency-
// plan user tries to touch a NEW org_id (via Connect Agent, Credentials, or
// Custom Actions) beyond their current distinct count. Existing tenants
// they already own are never retroactively blocked.
const AGENCY_MAX_CLIENT_TENANTS = parseInt(process.env.AGENCY_MAX_CLIENT_TENANTS || '10', 10);

async function checkAgencyTenantCap(userId, orgId) {
  const userResult = await pg.query('SELECT plan, org_id FROM users WHERE id = $1', [userId]);
  if (userResult.rows.length === 0 || userResult.rows[0].plan !== 'agency') return { ok: true };
  const ownOrgId = userResult.rows[0].org_id; // the agency's own home org — not a "client tenant", doesn't count against the cap
  if (orgId === ownOrgId) return { ok: true };
  const clientTenantIds = (await getUserOrgIds(userId)).filter((id) => id !== ownOrgId);
  if (clientTenantIds.includes(orgId)) return { ok: true }; // already a known tenant, not a new one
  if (clientTenantIds.length >= AGENCY_MAX_CLIENT_TENANTS) {
    return { ok: false, limit: AGENCY_MAX_CLIENT_TENANTS };
  }
  return { ok: true };
}

// The limit that actually applies to a given org — a per-org override if one
// has been set (see infra/scripts/set-org-limit.sh) takes top priority,
// then the owning user's plan (agency gets a higher limit than free),
// otherwise the global CLOUD_MONTHLY_LIMIT default.
async function getEffectiveLimit(orgId) {
  const override = await pg.query('SELECT monthly_limit FROM org_limit_overrides WHERE org_id = $1', [orgId]);
  if (override.rows.length > 0) return override.rows[0].monthly_limit;
  const plan = await getOrgOwnerPlan(orgId);
  return PLAN_MONTHLY_LIMITS[plan] || CLOUD_MONTHLY_LIMIT;
}

// Same idea for the per-agent rate limit — agency gets a higher ceiling.
async function getEffectiveRateLimit(orgId) {
  const plan = await getOrgOwnerPlan(orgId);
  return PLAN_RATE_LIMITS[plan] || AGENT_RATE_LIMIT_PER_MIN;
}

async function checkUsageLimit(orgId) {
  if (DEPLOYMENT_MODE !== 'cloud') return { ok: true };

  // Exempt if the org's owner is either the admin, or a local-range account
  // (id 1-9 — personal/founder accounts, by the local/service/external
  // convention established for user ids).
  const ownerIsExempt = await pg.query(
    `SELECT 1 FROM users u WHERE (u.is_admin = true OR u.id BETWEEN 1 AND 9) AND (
       u.org_id = $1 OR u.id IN (
         SELECT user_id FROM api_keys WHERE org_id = $1
         UNION SELECT user_id FROM custom_actions WHERE org_id = $1
         UNION SELECT user_id FROM service_credentials WHERE org_id = $1
       )
     ) LIMIT 1`,
    [orgId]
  );
  if (ownerIsExempt.rows.length > 0) return { ok: true, exempt: true };

  const limit = await getEffectiveLimit(orgId);
  const count = await getMonthlyUsage(orgId);
  return { ok: count < limit, count, limit };
}

// Looks up a user-supplied credential for this org+service first (self-serve path);
// falls back to the operator-set env var if nothing's been saved yet, so existing
// env-var-based setups (and your local dev keys) keep working unchanged.
async function getCredential(service, orgId) {
  const row = await pg.query(
    `SELECT encrypted_payload FROM service_credentials
     WHERE org_id=$1 AND service=$2 AND revoked_at IS NULL
     ORDER BY created_at DESC LIMIT 1`,
    [orgId, service]
  );
  if (row.rows.length > 0) {
    try {
      return JSON.parse(decryptCredential(row.rows[0].encrypted_payload));
    } catch (err) {
      fastify.log.error({ err }, `Failed to decrypt stored credential for ${service}/${orgId}`);
    }
  }
  const envVar = `AGENTRAAS_KEY_${service.toUpperCase()}_${orgId}`;
  const envVal = process.env[envVar] || process.env[`AGENTRAAS_KEY_${service.toUpperCase()}_DEFAULT`];
  if (!envVal) return null;
  // env vars are historically a single string; support both "key" and "user:pass" shapes.
  if (envVal.includes(':')) {
    const [username, ...rest] = envVal.split(':');
    return { username, password: rest.join(':') };
  }
  return { api_key: envVal };
}

const DASHBOARD_RANGES = {
  '24h': '24 hours',
  '7d': '7 days',
  '30d': '30 days',
  '90d': '90 days',
};
// Bucket granularity for the timeseries endpoint per range, so a 90-day chart
// doesn't try to render one point per hour.
const DASHBOARD_BUCKETS = {
  '24h': 'hour',
  '7d': 'day',
  '30d': 'day',
  '90d': 'day',
};

// Checks a webhook request's API key against api_keys for that org/agent pair.
// Backward-compatible by design: if nobody has ever run "Connect Agent" for this
// org_id/agent_id, there's nothing to enforce against, so requests pass through
// unauthenticated (this preserves existing demo/local-testing behavior). Once at
// least one key has been generated for that pair, a valid matching key becomes
// required — connecting an agent is what turns on enforcement for it.
async function verifyApiKey(providedKey, orgId, agentId) {
  const keysExist = await pg.query(
    'SELECT 1 FROM api_keys WHERE org_id=$1 AND agent_id=$2 AND revoked_at IS NULL LIMIT 1',
    [orgId, agentId]
  );
  if (keysExist.rows.length === 0) return { ok: true, enforced: false };
  if (!providedKey || providedKey === 'anonymous') return { ok: false, enforced: true };

  const prefix = providedKey.slice(0, 16);
  const hash = crypto.createHash('sha256').update(providedKey).digest('hex');
  const result = await pg.query(
    'SELECT id FROM api_keys WHERE key_prefix=$1 AND key_hash=$2 AND org_id=$3 AND agent_id=$4 AND revoked_at IS NULL',
    [prefix, hash, orgId, agentId]
  );
  if (result.rows.length === 0) return { ok: false, enforced: true };

  pg.query('UPDATE api_keys SET last_used_at = NOW() WHERE id=$1', [result.rows[0].id]).catch(() => {});
  return { ok: true, enforced: true };
}

// ─── HEALTH ───
fastify.get('/health', async () => {
  const [redisHealth, pgHealth] = await Promise.all([
    redis.ping().then(() => 'ok').catch(() => 'down'),
    pg.query('SELECT 1').then(() => 'ok').catch(() => 'down'),
  ]);
  return {
    status: redisHealth === 'ok' && pgHealth === 'ok' ? 'ok' : 'degraded',
    redis: redisHealth,
    postgres: pgHealth,
    env: NODE_ENV,
    time: new Date().toISOString()
  };
});

// ─── STATIC VENDOR FILES ───
// Chart.js is bundled locally (not loaded from a CDN) so the dashboard works
// on air-gapped self-hosted deployments and isn't at the mercy of ad-blockers
// or CDN outages. This route is what actually serves it — the file existing
// on disk alone doesn't make fastify serve it without an explicit route.
fastify.get('/vendor/chart.umd.min.js', async (request, reply) => {
  const filePath = path.join(__dirname, 'public', 'vendor', 'chart.umd.min.js');
  if (!fs.existsSync(filePath)) return reply.status(404).send({ error: 'chart.umd.min.js not found on this deployment.' });
  reply.type('application/javascript').send(fs.readFileSync(filePath, 'utf8'));
});

// ─── STATIC LANDING PAGE ───

// Legal/docs documents, served directly rather than linking to GitHub — these
// should stay readable even once the source repo itself is private. Rendered
// as styled HTML (client-side, via marked.js from a CDN) rather than raw
// plaintext, so they read like part of the product instead of a text dump.
const DOC_FILES = { license: 'LICENSE.md', privacy: 'PRIVACY.md', terms: 'TERMS.md', security: 'SECURITY.md', readme: 'README.md' };
const DOC_TITLES = { license: 'License', privacy: 'Privacy policy', terms: 'Terms of service', security: 'Security', readme: 'Documentation' };

function renderDocPage(title, rawMarkdown) {
  // JSON.stringify safely escapes quotes/backslashes/newlines for embedding
  // in a script tag; the </script> replacement guards against markdown
  // content that happens to contain that literal sequence breaking out of
  // the script block early.
  const safeMarkdown = JSON.stringify(rawMarkdown).replace(/<\/script/gi, '<\\/script');
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>${title} — AgentRaaS</title>
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'%3E%3Cpath d='M12 2.5L4.5 5.5V11C4.5 16 7.8 20.2 12 21.5C16.2 20.2 19.5 16 19.5 11V5.5L12 2.5Z' fill='%2306231C' stroke='%2300E0A8' stroke-width='0.8'/%3E%3Ccircle cx='12' cy='11.5' r='2.4' fill='%2300E0A8'/%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;600;700;800&family=Manrope:wght@400;500;600;700;800&display=swap" rel="stylesheet">
<style>
  :root {
    --ink: #0A0D14; --ink-2: #0F1420;
    --border-dark: #232A3A;
    --text: #F5F6F9; --muted: #8B93A6;
    --signal: #00E0A8;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: var(--ink); color: var(--text); font-family: 'Manrope', sans-serif; font-size: 16px; line-height: 1.7; }
  .wrap { max-width: 820px; margin: 0 auto; padding: 60px 32px 100px; }
  a { color: var(--signal); }
  header.nav { border-bottom: 1px solid var(--border-dark); padding: 18px 32px; display: flex; align-items: center; justify-content: space-between; }
  .brand { display: flex; align-items: center; gap: 10px; text-decoration: none; color: var(--text); }
  .brand .logo { width: 34px; height: 34px; background: var(--signal); border-radius: 9px; display: flex; align-items: center; justify-content: center; }
  .brand h1 { font-family: 'Space Grotesk', sans-serif; font-size: 18px; font-weight: 700; }
  .brand span { display: block; font-size: 12px; color: var(--muted); font-weight: 500; }
  .back-link { font-size: 14px; color: var(--muted); }
  #doc-content h1 { font-family: 'Space Grotesk', sans-serif; font-size: 34px; margin-bottom: 20px; }
  #doc-content h2 { font-family: 'Space Grotesk', sans-serif; font-size: 24px; margin: 36px 0 14px; }
  #doc-content h3 { font-family: 'Space Grotesk', sans-serif; font-size: 17px; margin: 24px 0 10px; color: var(--signal); }
  #doc-content p { margin-bottom: 14px; color: #C5CAD6; }
  #doc-content ol, #doc-content ul { margin: 0 0 16px 22px; color: #C5CAD6; }
  #doc-content li { margin-bottom: 6px; }
  #doc-content code { background: var(--ink-2); border: 1px solid var(--border-dark); border-radius: 5px; padding: 2px 6px; font-size: 14px; font-family: 'Space Grotesk', monospace; color: var(--signal); }
  #doc-content pre { background: var(--ink-2); border: 1px solid var(--border-dark); border-radius: 10px; padding: 18px 20px; overflow-x: auto; margin-bottom: 16px; }
  #doc-content pre code { background: none; border: none; padding: 0; color: #C5CAD6; }
  #doc-content hr { border: none; border-top: 1px solid var(--border-dark); margin: 32px 0; }
  #doc-content table { width: 100%; border-collapse: collapse; margin-bottom: 20px; }
  #doc-content th, #doc-content td { text-align: left; padding: 10px 14px; border-bottom: 1px solid var(--border-dark); font-size: 14.5px; }
  #doc-content blockquote { border-left: 3px solid var(--signal); padding-left: 16px; color: var(--muted); margin-bottom: 16px; }
</style>
</head>
<body>
<header class="nav">
  <a href="/" class="brand">
    <span class="logo"><svg width="18" height="18" viewBox="0 0 24 24"><path d="M12 2.5L4.5 5.5V11C4.5 16 7.8 20.2 12 21.5C16.2 20.2 19.5 16 19.5 11V5.5L12 2.5Z" fill="#06231C" stroke="#00E0A8" stroke-width="1"/><circle cx="12" cy="11.5" r="2.6" fill="#00E0A8"/></svg></span>
    <div><h1>AgentRaaS</h1><span>Agent Reliability as a Service</span></div>
  </a>
  <a href="/dashboard" class="back-link">← Back to dashboard</a>
</header>
<div class="wrap">
  <div id="doc-content">Loading…</div>
</div>
<script src="https://cdnjs.cloudflare.com/ajax/libs/marked/12.0.0/marked.min.js"></script>
<script>
  const rawMarkdown = ${safeMarkdown};
  document.getElementById('doc-content').innerHTML = marked.parse(rawMarkdown);
</script>
</body>
</html>`;
}

for (const [route, filename] of Object.entries(DOC_FILES)) {
  fastify.get(`/${route}`, async (request, reply) => {
    const docPath = path.join(SELF_HOST_SNAPSHOT_DIR, filename);
    if (!fs.existsSync(docPath)) return reply.status(404).send({ error: `${filename} not found on this deployment.` });
    reply.type('text/html').send(renderDocPage(DOC_TITLES[route] || filename, fs.readFileSync(docPath, 'utf8')));
  });
}

fastify.get('/', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'landing.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({error:'Landing page not found'});
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});

// ─── STATIC DASHBOARD ───
fastify.get('/dashboard', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'index.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({error:'Dashboard not found'});
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});
fastify.get('/dashboard/', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'index.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({error:'Dashboard not found'});
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});

// ─── STATIC GUIDE (no login required — linked from the landing page too) ───
fastify.get('/guide', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'guide.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({error:'Guide not found'});
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});

// ─── STATIC WEBHOOK AUDIT TOOL (no login required — public lead magnet) ───
fastify.get('/webhook-audit', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'webhook-audit.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({error:'Webhook audit tool not found'});
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});

// ─── SEO: robots.txt + sitemap.xml ───
// Only the marketing/content pages — /dashboard is a logged-in app, not
// content, and shouldn't be crawled or indexed as if it were a landing page.
const SITEMAP_ROUTES = ['/', '/guide', '/webhook-audit', '/status', ...Object.keys(DOC_FILES).map((r) => `/${r}`)];
fastify.get('/robots.txt', async (request, reply) => {
  reply.type('text/plain').send(`User-agent: *\nAllow: /\nDisallow: /dashboard\nSitemap: ${PUBLIC_URL}/sitemap.xml\n`);
});
fastify.get('/sitemap.xml', async (request, reply) => {
  const urls = SITEMAP_ROUTES.map((route) => `  <url><loc>${PUBLIC_URL}${route}</loc></url>`).join('\n');
  reply.type('application/xml').send(`<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls}\n</urlset>\n`);
});

// ─── PUBLIC STATUS PAGE ─── unauthenticated, safe to expose: circuit
// state and uptime are already platform-wide (shared across every org
// calling a service, not per-org — see the reliability report's own
// scoping note), so nothing org-specific ever appears here. Internal-only
// services (mockpay) are excluded — nothing a real visitor would recognize
// or care about. Fixed 90-day window, not caller-configurable.
fastify.get('/api/v1/public/status', async (request, reply) => {
  const services = Object.keys(SERVICE_CONFIG).filter((s) => !SERVICE_CONFIG[s].internal);
  const [uptimeResult, circuitStates] = await Promise.all([
    pg.query(
      `WITH events AS (
         SELECT service, to_state, occurred_at,
                LEAD(occurred_at) OVER (PARTITION BY service ORDER BY occurred_at) AS next_at
         FROM circuit_breaker_events
         WHERE service = ANY($1) AND occurred_at >= NOW() - INTERVAL '90 days'
       )
       SELECT service,
              COALESCE(SUM(EXTRACT(EPOCH FROM (LEAST(COALESCE(next_at, NOW()), NOW()) - occurred_at))) FILTER (WHERE to_state = 'open'), 0) AS open_seconds
       FROM events
       GROUP BY service`,
      [services]
    ),
    proxy.getCircuitStatesBatch(services),
  ]);
  const openSecondsMap = {};
  for (const row of uptimeResult.rows) openSecondsMap[row.service] = parseFloat(row.open_seconds) || 0;
  const RANGE_SECONDS = 90 * 86400;

  const report = services.map((svc) => {
    const openSeconds = Math.min(openSecondsMap[svc] || 0, RANGE_SECONDS);
    const uptimePct = Math.round((1 - openSeconds / RANGE_SECONDS) * 10000) / 100;
    const circuitState = circuitStates[svc];
    const status = circuitState === 'open' ? 'down' : circuitState === 'half-open' ? 'degraded' : 'operational';
    return { service: svc, status, uptime_90d: uptimePct };
  });
  const overall = report.some((s) => s.status === 'down') ? 'major_outage' : report.some((s) => s.status === 'degraded') ? 'degraded' : 'operational';

  return { overall, generated_at: new Date().toISOString(), services: report.sort((a, b) => a.service.localeCompare(b.service)) };
});

fastify.get('/status', async (request, reply) => {
  const htmlPath = path.join(__dirname, 'public', 'status.html');
  if (!fs.existsSync(htmlPath)) return reply.status(404).send({ error: 'Status page not found' });
  reply.type('text/html').send(fs.readFileSync(htmlPath, 'utf8'));
});

// ─── AUTH ROUTES ───
fastify.post('/api/v1/auth/register', async (request, reply) => {
  const { email, password, org_id } = request.body || {};

  if (!isValidEmail(email)) return reply.status(422).send({ error: 'Enter a valid email address.' });
  if (!isValidPassword(password)) return reply.status(422).send({ error: 'Password must be at least 8 characters.' });

  const existing = await pg.query('SELECT id FROM users WHERE email = $1', [email]);
  if (existing.rows.length > 0) return reply.status(409).send({ error: 'An account with that email already exists.' });

  const passwordHash = await hashPassword(password);
  // Most users are simple/no-code — they'll never manually "Connect Agent"
  // with a hand-typed org_id. Auto-generate a default one so they have a
  // working home org (and a lookup target for a limit override) from the
  // moment they register, not only once they've done something technical.
  const defaultOrgId = org_id || `org_${crypto.randomBytes(6).toString('hex')}`;
  const result = await pg.query(
    `INSERT INTO users (email, password_hash, org_id) VALUES ($1, $2, $3) RETURNING id, email, org_id, plan, created_at`,
    [email, passwordHash, defaultOrgId]
  );
  const user = result.rows[0];

  const rawToken = crypto.randomBytes(32).toString('hex');
  const tokenHash = crypto.createHash('sha256').update(rawToken).digest('hex');
  const expiresAt = new Date(Date.now() + 24 * 60 * 60 * 1000); // 24 hours
  await pg.query(
    `INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1,$2,$3)`,
    [user.id, tokenHash, expiresAt]
  );
  const verifyUrl = `${PUBLIC_URL}/dashboard?verify_token=${rawToken}`;
  await sendVerificationEmail(email, verifyUrl);

  // No session cookie yet — login is blocked until the email is verified.
  const responseBody = { registered: true, message: 'Check your email to verify your account before logging in.' };
  if (!mailTransport || EXPOSE_DEV_VERIFY_URL) {
    // SMTP isn't configured — the link is already being logged server-side
    // (see sendVerificationEmail), so including it here too isn't a new
    // exposure, just a more convenient version of the same fallback for
    // self-hosters without email set up. Or EXPOSE_DEV_VERIFY_URL is
    // explicitly set for local dev/test convenience even with real SMTP on.
    responseBody.dev_verify_url = verifyUrl;
  }
  return responseBody;
});

fastify.post('/api/v1/auth/login', async (request, reply) => {
  const { email, password } = request.body || {};
  if (!isValidEmail(email) || typeof password !== 'string') {
    return reply.status(422).send({ error: 'Email and password are required.' });
  }

  const ip = request.ip; // resolved via trustProxy, not manually parsed from a client-supplied header
  const underLimit = await checkLoginRateLimit(redis, ip, email);
  if (!underLimit) {
    return reply.status(429).send({ error: 'Too many login attempts. Try again in 15 minutes.' });
  }

  const result = await pg.query('SELECT id, email, org_id, plan, password_hash, is_admin, email_verified, must_change_password FROM users WHERE email = $1', [email]);
  if (result.rows.length === 0) {
    return reply.status(401).send({ error: 'Invalid email or password.' });
  }

  const user = result.rows[0];
  const valid = await verifyPassword(password, user.password_hash);
  if (!valid) {
    return reply.status(401).send({ error: 'Invalid email or password.' });
  }

  if (!user.email_verified) {
    return reply.status(403).send({ error: 'Please verify your email before logging in.', code: 'EMAIL_NOT_VERIFIED' });
  }

  await clearLoginRateLimit(redis, ip, email);
  await pg.query('UPDATE users SET last_login_at = NOW() WHERE id = $1', [user.id]);

  // The sole admin account's password is a rotating, single-use credential
  // by design — never persistently chosen, always regenerated after each
  // login. Find the next one via server logs (podman logs ar-api). This
  // supersedes must_change_password for the admin (cleared here too), which
  // only ever mattered for a one-time forced change — this is stronger.
  if (user.is_admin) {
    const rotatedPassword = crypto.randomBytes(18).toString('base64');
    const rotatedHash = await hashPassword(rotatedPassword);
    await pg.query('UPDATE users SET password_hash = $1, must_change_password = false WHERE id = $2', [rotatedHash, user.id]);
    fastify.log.warn(`[ADMIN PASSWORD ROTATED] New password for ${user.email}: ${rotatedPassword}`);
  }

  const token = await reply.jwtSign({ sub: user.id, email: user.email, org_id: user.org_id }, { expiresIn: '7d' });
  reply.setCookie('ar_session', token, COOKIE_OPTS);
  return { user: { id: user.id, email: user.email, org_id: user.org_id, plan: user.plan, is_admin: user.is_admin, deployment_mode: DEPLOYMENT_MODE, must_change_password: false } };
});

fastify.get('/api/v1/auth/verify-email', async (request, reply) => {
  const { token } = request.query || {};
  if (!token || typeof token !== 'string') return reply.status(400).send({ error: 'A verification token is required.' });

  const tokenHash = crypto.createHash('sha256').update(token).digest('hex');
  const result = await pg.query(
    `SELECT id, user_id FROM email_verification_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > NOW()`,
    [tokenHash]
  );
  if (result.rows.length === 0) {
    return reply.status(400).send({ error: 'This verification link is invalid or has expired. Request a new one.' });
  }

  const { id: tokenId, user_id: userId } = result.rows[0];
  const client = await pg.connect();
  let userRow;
  try {
    await client.query('BEGIN');
    const updated = await client.query('UPDATE users SET email_verified = true WHERE id = $1 RETURNING id, email, org_id, plan, is_admin, must_change_password', [userId]);
    userRow = updated.rows[0];
    await client.query('UPDATE email_verification_tokens SET used_at = NOW() WHERE id = $1', [tokenId]);
    await client.query('COMMIT');
  } catch (err) {
    await client.query('ROLLBACK');
    throw err;
  } finally {
    client.release();
  }

  // Verifying also logs the user in — no reason to make them type their
  // password again right after proving they own the account.
  const sessionToken = await reply.jwtSign({ sub: userRow.id, email: userRow.email, org_id: userRow.org_id }, { expiresIn: '7d' });
  reply.setCookie('ar_session', sessionToken, COOKIE_OPTS);
  return { verified: true, user: { id: userRow.id, email: userRow.email, org_id: userRow.org_id, plan: userRow.plan, is_admin: userRow.is_admin, deployment_mode: DEPLOYMENT_MODE, must_change_password: userRow.must_change_password } };
});

fastify.post('/api/v1/auth/resend-verification', async (request, reply) => {
  const { email } = request.body || {};
  const genericResponse = { message: 'If that account exists and needs verification, a new link has been sent.' };
  if (!isValidEmail(email)) return genericResponse;

  const ip = request.ip; // resolved via trustProxy, not manually parsed from a client-supplied header
  const underLimit = await checkLoginRateLimit(redis, ip, `resend-verify:${email}`);
  if (!underLimit) return genericResponse;

  const userResult = await pg.query('SELECT id, email_verified FROM users WHERE email = $1', [email]);
  if (userResult.rows.length === 0 || userResult.rows[0].email_verified) return genericResponse;

  const rawToken = crypto.randomBytes(32).toString('hex');
  const tokenHash = crypto.createHash('sha256').update(rawToken).digest('hex');
  const expiresAt = new Date(Date.now() + 24 * 60 * 60 * 1000);
  await pg.query(
    `INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1,$2,$3)`,
    [userResult.rows[0].id, tokenHash, expiresAt]
  );
  const verifyUrl = `${PUBLIC_URL}/dashboard?verify_token=${rawToken}`;
  await sendVerificationEmail(email, verifyUrl);

  if (!mailTransport || EXPOSE_DEV_VERIFY_URL) genericResponse.dev_verify_url = verifyUrl;
  return genericResponse;
});

fastify.post('/api/v1/auth/logout', async (request, reply) => {
  reply.clearCookie('ar_session', { path: '/' });
  return { loggedOut: true };
});

fastify.post('/api/v1/auth/forgot-password', async (request, reply) => {
  const { email } = request.body || {};
  // Always return the same generic response whether or not the email exists —
  // confirming/denying account existence here is its own information leak.
  const genericResponse = { message: 'If an account exists for that email, a reset link has been sent.' };
  if (!isValidEmail(email)) return genericResponse;

  const ip = request.ip; // resolved via trustProxy, not manually parsed from a client-supplied header
  const underLimit = await checkLoginRateLimit(redis, ip, `reset:${email}`);
  if (!underLimit) return genericResponse; // don't reveal rate limiting to a potential attacker either

  const userResult = await pg.query('SELECT id FROM users WHERE email = $1', [email]);
  if (userResult.rows.length === 0) return genericResponse;

  const rawToken = crypto.randomBytes(32).toString('hex');
  const tokenHash = crypto.createHash('sha256').update(rawToken).digest('hex');
  const expiresAt = new Date(Date.now() + 60 * 60 * 1000); // 1 hour

  await pg.query(
    `INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1,$2,$3)`,
    [userResult.rows[0].id, tokenHash, expiresAt]
  );

  const resetUrl = `${PUBLIC_URL}/dashboard?reset_token=${rawToken}`;
  await sendPasswordResetEmail(email, resetUrl);

  return genericResponse;
});

fastify.post('/api/v1/auth/reset-password', async (request, reply) => {
  const { token, new_password } = request.body || {};
  if (!token || typeof token !== 'string') return reply.status(422).send({ error: 'A reset token is required.' });
  if (!isValidPassword(new_password)) return reply.status(422).send({ error: 'New password must be at least 8 characters.' });

  const tokenHash = crypto.createHash('sha256').update(token).digest('hex');
  const result = await pg.query(
    `SELECT id, user_id FROM password_reset_tokens WHERE token_hash = $1 AND used_at IS NULL AND expires_at > NOW()`,
    [tokenHash]
  );
  if (result.rows.length === 0) {
    return reply.status(400).send({ error: 'This reset link is invalid or has expired. Request a new one.' });
  }

  const { id: tokenId, user_id: userId } = result.rows[0];
  const newHash = await hashPassword(new_password);

  const client = await pg.connect();
  try {
    await client.query('BEGIN');
    await client.query('UPDATE users SET password_hash = $1 WHERE id = $2', [newHash, userId]);
    await client.query('UPDATE password_reset_tokens SET used_at = NOW() WHERE id = $1', [tokenId]);
    await client.query('COMMIT');
  } catch (err) {
    await client.query('ROLLBACK');
    throw err;
  } finally {
    client.release();
  }

  return { reset: true };
});

fastify.get('/api/v1/auth/me', { preHandler: requireAuthRateLimited }, async (request) => {
  const result = await pg.query('SELECT is_admin, must_change_password FROM users WHERE id = $1', [request.user.sub]);
  const isAdmin = result.rows.length > 0 ? result.rows[0].is_admin : false;
  const mustChangePassword = result.rows.length > 0 ? result.rows[0].must_change_password : false;
  // Additive field — empty for any user who has never authenticated via
  // Enterprise SSO (src/ee/auth). Existing non-SSO consumers of this
  // endpoint see no shape change otherwise.
  const membershipResult = await pg.query('SELECT org_id, role FROM org_members WHERE user_id = $1', [request.user.sub]);
  return { user: { id: request.user.sub, email: request.user.email, org_id: request.user.org_id, is_admin: isAdmin, deployment_mode: DEPLOYMENT_MODE, must_change_password: mustChangePassword, orgs: membershipResult.rows } };
});

fastify.post('/api/v1/auth/password', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { current_password, new_password } = request.body || {};
  if (!isValidPassword(new_password)) {
    return reply.status(422).send({ error: 'New password must be at least 8 characters.' });
  }
  const result = await pg.query('SELECT password_hash FROM users WHERE id=$1', [request.user.sub]);
  if (result.rows.length === 0) return reply.status(404).send({ error: 'User not found.' });
  const valid = await verifyPassword(current_password, result.rows[0].password_hash);
  if (!valid) return reply.status(401).send({ error: 'Current password is incorrect.' });
  const newHash = await hashPassword(new_password);
  await pg.query('UPDATE users SET password_hash=$1, must_change_password=false WHERE id=$2', [newHash, request.user.sub]);
  return { updated: true };
});

// ─── ENTERPRISE SSO + RBAC (src/ee/auth) ───
// OIDC login for orgs that have configured one or more identity providers
// (infra/migrations/022_sso_multi_idp.sql). Membership can come from SSO
// (domain + best-effort role-claim matching) or a manual email invite
// (see the invites/members endpoints below) — see RESTRUCTURE_PLAN.md and
// src/ee/auth/README.md for scope. Every endpoint here is gated on
// ENTERPRISE_MODE.
const SSO_CALLBACK_URL = `${PUBLIC_URL}/api/v1/auth/sso/callback`;
const SSO_FLOW_COOKIE_OPTS = { ...COOKIE_OPTS, maxAge: 60 * 10 }; // 10 minutes — just long enough to complete a login round-trip at the IdP
const ROLE_VALUES = ['admin', 'developer', 'auditor'];
const INVITE_TTL_MS = 7 * 24 * 60 * 60 * 1000; // 7 days

function validateSsoConfigBody(body) {
  const { issuer_url, client_id, client_secret, allowed_domains, default_role } = body || {};
  let parsedIssuer;
  try { parsedIssuer = new URL(issuer_url); } catch { parsedIssuer = null; }
  if (!parsedIssuer || parsedIssuer.protocol !== 'https:') return 'issuer_url must be a valid https:// URL.';
  if (!client_id || typeof client_id !== 'string') return 'client_id is required.';
  if (!client_secret || typeof client_secret !== 'string') return 'client_secret is required.';
  if (!allowed_domains || typeof allowed_domains !== 'string') return 'allowed_domains is required (comma-separated email domains).';
  if (default_role !== undefined && !ROLE_VALUES.includes(default_role)) return `default_role must be one of: ${ROLE_VALUES.join(', ')}`;
  return null;
}

fastify.get('/api/v1/auth/sso/:orgId/configs', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  const configs = await ssoManager.listConfigsForAdminView(request.params.orgId);
  return { configs };
});

fastify.post('/api/v1/auth/sso/:orgId/configs', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const validationError = validateSsoConfigBody(request.body);
  if (validationError) return reply.status(422).send({ error: validationError });
  const { issuer_url, client_id, client_secret, allowed_domains, default_role, enabled } = request.body;

  const id = await ssoManager.createConfig(request.params.orgId, {
    issuerUrl: issuer_url, clientId: client_id, clientSecret: client_secret,
    allowedDomains: allowed_domains, defaultRole: default_role || 'developer', enabled: enabled !== false,
  });
  return { created: true, id };
});

fastify.put('/api/v1/auth/sso/:orgId/configs/:configId', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const existing = await ssoManager.getConfigById(request.params.configId);
  if (!existing || existing.org_id !== request.params.orgId) return reply.status(404).send({ error: 'No such SSO configuration for this org.' });
  const validationError = validateSsoConfigBody(request.body);
  if (validationError) return reply.status(422).send({ error: validationError });
  const { issuer_url, client_id, client_secret, allowed_domains, default_role, enabled } = request.body;

  await ssoManager.updateConfig(request.params.configId, {
    issuerUrl: issuer_url, clientId: client_id, clientSecret: client_secret,
    allowedDomains: allowed_domains, defaultRole: default_role || 'developer', enabled: enabled !== false,
  });
  return { updated: true };
});

fastify.delete('/api/v1/auth/sso/:orgId/configs/:configId', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const existing = await ssoManager.getConfigById(request.params.configId);
  if (!existing || existing.org_id !== request.params.orgId) return reply.status(404).send({ error: 'No such SSO configuration for this org.' });
  await ssoManager.deleteConfig(request.params.configId);
  return { deleted: true };
});

fastify.get('/api/v1/auth/sso/:orgId/login', async (request, reply) => {
  const enterpriseCheck = await requireEnterpriseMode(request, reply);
  if (enterpriseCheck) return enterpriseCheck;
  const orgId = request.params.orgId;
  const config = await ssoManager.findLoginConfig(orgId, request.query.config_id);
  if (!config) {
    return reply.status(404).send({ error: 'SSO is not configured for this org, or more than one IdP exists and no config_id was specified.' });
  }

  let flow;
  try {
    flow = await ssoManager.buildAuthorizationUrl(config, SSO_CALLBACK_URL);
  } catch (err) {
    fastify.log.error({ err, orgId }, 'Failed to start SSO login (IdP discovery/config error)');
    return reply.status(502).send({ error: 'Could not reach this org’s identity provider. Contact your admin.' });
  }

  reply.setCookie('ar_sso_flow', JSON.stringify({
    orgId, configId: config.id,
    state: flow.state,
    codeVerifier: flow.codeVerifier,
    nonce: flow.nonce,
  }), SSO_FLOW_COOKIE_OPTS);
  return reply.redirect(flow.url);
});

fastify.get('/api/v1/auth/sso/callback', async (request, reply) => {
  const enterpriseCheck = await requireEnterpriseMode(request, reply);
  if (enterpriseCheck) return enterpriseCheck;
  const rawFlow = request.cookies?.ar_sso_flow;
  reply.clearCookie('ar_sso_flow', { path: '/' });
  if (!rawFlow) {
    return reply.status(400).send({ error: 'Missing or expired SSO login attempt. Try logging in again.' });
  }
  let flow;
  try { flow = JSON.parse(rawFlow); } catch { return reply.status(400).send({ error: 'Invalid SSO login attempt.' }); }

  const config = await ssoManager.getConfigById(flow.configId);
  if (!config || !config.enabled || config.org_id !== flow.orgId) {
    return reply.status(404).send({ error: 'SSO is not configured or is disabled for this org.' });
  }

  let claims;
  try {
    const currentUrl = new URL(request.url, PUBLIC_URL);
    claims = await ssoManager.handleCallback(config, {
      currentUrl,
      state: flow.state,
      codeVerifier: flow.codeVerifier,
      nonce: flow.nonce,
    });
  } catch (err) {
    fastify.log.error({ err, orgId: flow.orgId }, 'SSO callback failed (token exchange/verification error)');
    return reply.status(401).send({ error: 'SSO login failed. Try again, or contact your admin.' });
  }

  if (claims.email_verified === false || !claims.email) {
    return reply.status(403).send({ error: 'Your identity provider did not return a verified email address.' });
  }
  if (!SsoManager.matchOrgByEmailDomain(claims.email, config.allowed_domains)) {
    return reply.status(403).send({ error: 'Your email domain is not allowed to sign in to this org.' });
  }

  const role = SsoManager.mapClaimsToRole(claims.claims, config.default_role);

  let userResult = await pg.query('SELECT id, email, org_id FROM users WHERE email = $1', [claims.email]);
  let user;
  if (userResult.rows.length === 0) {
    // SSO-provisioned account — the IdP already proved ownership of this
    // email, so it's created pre-verified. password_hash is NOT NULL in the
    // schema but is otherwise unused by this user unless they later go
    // through the normal forgot-password flow to set one.
    const unusablePasswordHash = await hashPassword(crypto.randomBytes(24).toString('hex'));
    const insertResult = await pg.query(
      `INSERT INTO users (email, password_hash, org_id, email_verified) VALUES ($1, $2, $3, true) RETURNING id, email, org_id`,
      [claims.email, unusablePasswordHash, flow.orgId]
    );
    user = insertResult.rows[0];
  } else {
    user = userResult.rows[0];
  }

  await ssoManager.upsertMembership(user.id, flow.orgId, role);

  const token = await reply.jwtSign({ sub: user.id, email: user.email, org_id: user.org_id }, { expiresIn: '7d' });
  reply.setCookie('ar_session', token, COOKIE_OPTS);
  return reply.redirect(`${PUBLIC_URL}/dashboard`);
});

// ─── Org members (admin-only management) ───
fastify.get('/api/v1/auth/sso/:orgId/members', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  const result = await pg.query(
    `SELECT u.id as user_id, u.email, m.role, m.created_at, m.updated_at
     FROM org_members m JOIN users u ON u.id = m.user_id
     WHERE m.org_id = $1 ORDER BY m.created_at ASC`,
    [request.params.orgId]
  );
  return { members: result.rows };
});

fastify.put('/api/v1/auth/sso/:orgId/members/:userId', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const { role } = request.body || {};
  if (!ROLE_VALUES.includes(role)) return reply.status(422).send({ error: `role must be one of: ${ROLE_VALUES.join(', ')}` });
  const result = await pg.query(
    `UPDATE org_members SET role = $3, updated_at = NOW() WHERE org_id = $1 AND user_id = $2 RETURNING user_id`,
    [request.params.orgId, request.params.userId, role]
  );
  if (result.rows.length === 0) return reply.status(404).send({ error: 'That user is not a member of this org.' });
  return { updated: true };
});

fastify.delete('/api/v1/auth/sso/:orgId/members/:userId', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  await pg.query(`DELETE FROM org_members WHERE org_id = $1 AND user_id = $2`, [request.params.orgId, request.params.userId]);
  return { deleted: true };
});

// ─── Org invites (manual, email-based — the non-SSO path into org_members) ───
async function sendOrgInviteEmail(toEmail, orgId, role, acceptUrl) {
  if (!mailTransport) {
    fastify.log.warn(`SMTP not configured — org invite link for ${toEmail} (org ${orgId}, role ${role}): ${acceptUrl}`);
    return;
  }
  try {
    await mailTransport.sendMail({
      from: process.env.SMTP_FROM || 'AgentRaaS <no-reply@agentraas.local>',
      to: toEmail,
      subject: `You've been invited to join ${orgId} on AgentRaaS`,
      text: `You've been invited to join the "${orgId}" org on AgentRaaS as ${role}.\n\nAccept here: ${acceptUrl}\n\nThis link expires in 7 days.`,
      html: buildEmailHtml({
        heading: 'You’ve been invited',
        bodyHtml: `You've been invited to join <strong>${escapeHtml(orgId)}</strong> on AgentRaaS as <strong>${escapeHtml(role)}</strong>.`,
        ctaText: 'Accept invite',
        ctaUrl: acceptUrl,
        expiryNote: 'This link expires in 7 days.',
      }),
    });
  } catch (err) {
    fastify.log.error({ err }, 'Failed to send org invite email');
  }
}

fastify.post('/api/v1/auth/sso/:orgId/invites', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const { email, role } = request.body || {};
  if (!isValidEmail(email)) return reply.status(422).send({ error: 'Enter a valid email address.' });
  const inviteRole = role || 'developer';
  if (!ROLE_VALUES.includes(inviteRole)) return reply.status(422).send({ error: `role must be one of: ${ROLE_VALUES.join(', ')}` });

  const orgId = request.params.orgId;
  await pg.query(`INSERT INTO orgs (org_id) VALUES ($1) ON CONFLICT (org_id) DO NOTHING`, [orgId]);

  const rawToken = crypto.randomBytes(32).toString('hex');
  const tokenHash = crypto.createHash('sha256').update(rawToken).digest('hex');
  const expiresAt = new Date(Date.now() + INVITE_TTL_MS);
  const result = await pg.query(
    `INSERT INTO org_invites (org_id, email, role, token_hash, invited_by_user_id, expires_at) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id`,
    [orgId, email, inviteRole, tokenHash, request.user.sub, expiresAt]
  );

  const acceptUrl = `${PUBLIC_URL}/dashboard?invite_token=${rawToken}`;
  await sendOrgInviteEmail(email, orgId, inviteRole, acceptUrl);

  const responseBody = { invited: true, id: result.rows[0].id };
  if (!mailTransport || EXPOSE_DEV_VERIFY_URL) responseBody.dev_accept_url = acceptUrl;
  return responseBody;
});

fastify.get('/api/v1/auth/sso/:orgId/invites', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  const result = await pg.query(
    `SELECT id, email, role, expires_at, created_at FROM org_invites
     WHERE org_id = $1 AND accepted_at IS NULL AND expires_at > NOW() ORDER BY created_at DESC`,
    [request.params.orgId]
  );
  return { invites: result.rows };
});

fastify.delete('/api/v1/auth/sso/:orgId/invites/:id', { preHandler: [...requireOrgAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  await pg.query(`DELETE FROM org_invites WHERE id = $1 AND org_id = $2`, [request.params.id, request.params.orgId]);
  return { deleted: true };
});

// Public — accepting an invite doesn't require an existing session (the
// invitee may not have an AgentRaaS account yet). `password` is required
// only to create a brand-new account; an existing user's password is left
// untouched, only their org_members role is granted.
fastify.post('/api/v1/auth/invites/accept', { preHandler: requireEnterpriseMode }, async (request, reply) => {
  const { token, password } = request.body || {};
  if (!token || typeof token !== 'string') return reply.status(422).send({ error: 'An invite token is required.' });

  const tokenHash = crypto.createHash('sha256').update(token).digest('hex');
  const inviteResult = await pg.query(
    `SELECT id, org_id, email, role FROM org_invites WHERE token_hash = $1 AND accepted_at IS NULL AND expires_at > NOW()`,
    [tokenHash]
  );
  if (inviteResult.rows.length === 0) {
    return reply.status(400).send({ error: 'This invite is invalid, already used, or has expired.' });
  }
  const invite = inviteResult.rows[0];

  let userResult = await pg.query('SELECT id, email, org_id FROM users WHERE email = $1', [invite.email]);
  let user;
  if (userResult.rows.length === 0) {
    if (!isValidPassword(password)) return reply.status(422).send({ error: 'Password must be at least 8 characters (required to create your account).' });
    const passwordHash = await hashPassword(password);
    const insertResult = await pg.query(
      `INSERT INTO users (email, password_hash, org_id, email_verified) VALUES ($1, $2, $3, true) RETURNING id, email, org_id`,
      [invite.email, passwordHash, invite.org_id]
    );
    user = insertResult.rows[0];
  } else {
    user = userResult.rows[0];
  }

  await ssoManager.upsertMembership(user.id, invite.org_id, invite.role);
  await pg.query(`UPDATE org_invites SET accepted_at = NOW() WHERE id = $1`, [invite.id]);

  const sessionToken = await reply.jwtSign({ sub: user.id, email: user.email, org_id: user.org_id }, { expiresIn: '7d' });
  reply.setCookie('ar_session', sessionToken, COOKIE_OPTS);
  return { accepted: true, org_id: invite.org_id, role: invite.role };
});

// ─── INTERNAL MOCK SERVICE ───
fastify.post('/internal/mockpay', async (request, reply) => {
  const {amount,fail} = request.body || {};
  // fail:true -> always fails. fail:false -> never fails (a real deterministic
  // override, used by automated tests to eliminate flakiness). fail omitted
  // entirely -> the original ~10% random failure, useful for manual demo/seed
  // data variety where occasional failures are actually the point.
  const shouldFail = fail === true || (fail === undefined && Math.random() < 0.1);
  if (shouldFail) {
    return reply.status(500).send({error:'MockPay temporarily unavailable',code:'mock_error'});
  }
  return reply.status(200).send({
    id:'mockpay_'+crypto.randomBytes(8).toString('hex'),
    amount:amount||0,
    status:'completed',
    processor:'MockPay',
    timestamp:new Date().toISOString()
  });
});

// ─── DASHBOARD APIs (all require a logged-in user) ───
// Route registration moved to src/core/dashboard (registerDashboardRoutes),
// called further down once proxy.getCircuitStatesBatch exists (see near the
// /mcp route) — logic is otherwise unchanged, per RESTRUCTURE_PLAN.md Phase 3.

// ─── BILLING (Agency tier via Paddle) ───
// Enterprise is custom/sales-assisted (see the landing page's "Contact
// sales" CTA) — it has no self-serve checkout, so Agency is the only plan
// this flow needs to handle.

// Returns what the frontend needs to open a Paddle.js overlay checkout —
// the checkout itself runs client-side; this just hands over the public
// client-side token, the price to check out, and custom_data so the
// webhook (server-side) can match the resulting subscription back to this
// user without any client-supplied user_id it could otherwise forge.
fastify.get('/api/v1/billing/checkout-info', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const plan = 'agency';
  const clientToken = process.env.PADDLE_CLIENT_TOKEN;
  const priceId = PADDLE_PRICE_ID_BY_PLAN[plan];
  if (!clientToken || !priceId) {
    return reply.status(503).send({ error: 'Billing is not configured on this deployment.' });
  }
  const userResult = await pg.query('SELECT email, plan FROM users WHERE id = $1', [request.user.sub]);
  if (userResult.rows.length === 0) return reply.status(404).send({ error: 'User not found.' });
  const user = userResult.rows[0];
  return {
    client_token: clientToken,
    environment: PADDLE_ENVIRONMENT,
    price_id: priceId,
    email: user.email,
    current_plan: user.plan,
    custom_data: { user_id: request.user.sub, plan },
  };
});

// Paddle webhook — subscription lifecycle events land here. Verifies the
// HMAC-SHA256 signature over the raw request body before trusting anything
// in the payload (see the addContentTypeParser near the top of this file
// for how request.rawBody is preserved), and records every processed
// event id so a redelivered webhook (Paddle retries on any non-2xx
// response, and can also send genuine duplicates) is a safe no-op instead
// of double-applying a plan change.
//
// NOTE: exact field names below (customData vs custom_data, camelCase vs
// snake_case) are per the SDK's documented convention but have not been
// verified against a live Paddle sandbox — test this against real
// sandbox events before relying on it in production, and adjust field
// access if actual payloads differ from what's coded here.
fastify.post('/api/v1/webhooks/paddle', async (request, reply) => {
  if (!paddleClient || !PADDLE_WEBHOOK_SECRET) {
    return reply.status(503).send({ error: 'Billing is not configured on this deployment.' });
  }
  const signature = request.headers['paddle-signature'];
  if (!signature) return reply.status(401).send({ error: 'Missing Paddle-Signature header.' });

  let event;
  try {
    event = await paddleClient.webhooks.unmarshal(request.rawBody.toString('utf8'), PADDLE_WEBHOOK_SECRET, signature);
  } catch (err) {
    fastify.log.warn(`Paddle webhook signature verification failed: ${err.message}`);
    return reply.status(401).send({ error: 'Invalid signature.' });
  }
  if (!event) return reply.status(400).send({ error: 'Could not parse webhook event.' });

  const eventId = event.eventId || event.event_id;
  const eventType = event.eventType || event.event_type;
  const sub = event.data;

  // Idempotency check — do this before any writes.
  const already = await pg.query('SELECT 1 FROM processed_webhook_events WHERE event_id = $1', [eventId]);
  if (already.rows.length > 0) return { received: true };

  const userId = sub?.customData?.user_id || sub?.custom_data?.user_id;
  // custom_data.plan is set at checkout time (see /api/v1/billing/checkout-info);
  // Agency is the only self-serve paid plan, so it's also the fallback for
  // any subscription created before this field existed.
  const targetPlan = sub?.customData?.plan || sub?.custom_data?.plan || 'agency';

  if (['subscription.created', 'subscription.activated', 'subscription.updated'].includes(eventType) && userId) {
    const status = sub.status;
    const periodEnd = sub.currentBillingPeriod?.endsAt || sub.current_billing_period?.ends_at || null;
    await pg.query(
      `INSERT INTO subscriptions (user_id, paddle_subscription_id, paddle_customer_id, status, current_period_end)
       VALUES ($1, $2, $3, $4, $5)
       ON CONFLICT (paddle_subscription_id) DO UPDATE SET status = $4, current_period_end = $5, updated_at = NOW()`,
      [userId, sub.id, sub.customerId || sub.customer_id, status, periodEnd]
    );
    const activeStatuses = ['active', 'trialing'];
    await pg.query('UPDATE users SET plan = $1 WHERE id = $2', [activeStatuses.includes(status) ? targetPlan : 'free', userId]);
  } else if (eventType === 'subscription.canceled' && sub) {
    await pg.query(`UPDATE subscriptions SET status = 'canceled', updated_at = NOW() WHERE paddle_subscription_id = $1`, [sub.id]);
    if (userId) await pg.query(`UPDATE users SET plan = 'free' WHERE id = $1`, [userId]);
  }

  await pg.query('INSERT INTO processed_webhook_events (event_id, source) VALUES ($1, $2) ON CONFLICT DO NOTHING', [eventId, 'paddle']);
  return { received: true };
});

// ─── INBOUND WEBHOOK SIGNATURE VERIFICATION ───
// Lets a user register a real provider webhook (Stripe, GitHub, Shopify,
// WhatsApp, Twilio), receive it at a unique AgentRaaS URL, have the
// signature verified against that provider's actual scheme (see
// src/ee/hmac), and only then forward the proven-authentic payload to
// their real destination. This is the inverse of the existing outbound
// proxy: outbound protects calls AgentRaaS makes TO a service; this
// protects calls a service makes TO the user's downstream system.
const INBOUND_WEBHOOK_PROVIDERS = new Set(['stripe', 'github', 'shopify', 'whatsapp', 'twilio', 'slack', 'linear', 'mailgun', 'sendgrid', 'hubspot']);

fastify.post('/api/v1/inbound-webhooks', { preHandler: [...requireAuthRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const { org_id, provider, webhook_secret, destination_url, whatsapp_verify_token } = request.body || {};
  if (!isValidIdentifier(org_id)) {
    return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  }
  if (!INBOUND_WEBHOOK_PROVIDERS.has(provider)) {
    return reply.status(422).send({ error: `provider must be one of: ${[...INBOUND_WEBHOOK_PROVIDERS].join(', ')}` });
  }
  if (typeof webhook_secret !== 'string' || webhook_secret.length < 8) {
    return reply.status(422).send({ error: 'webhook_secret is required (the signing secret from the provider\'s dashboard).' });
  }
  if (provider === 'whatsapp' && (typeof whatsapp_verify_token !== 'string' || whatsapp_verify_token.length < 8)) {
    return reply.status(422).send({ error: 'whatsapp_verify_token is required for the WhatsApp provider (used in Meta\'s GET handshake).' });
  }

  const urlError = await validateTargetUrl(destination_url);
  if (urlError) return reply.status(422).send({ error: `destination_url: ${urlError}` });

  const inboundToken = crypto.randomBytes(24).toString('hex');
  const encryptedSecret = encryptCredential(webhook_secret);

  await pg.query(
    `INSERT INTO inbound_webhooks (user_id, org_id, provider, webhook_secret, destination_url, inbound_token, whatsapp_verify_token)
     VALUES ($1, $2, $3, $4, $5, $6, $7)`,
    [request.user.sub, org_id, provider, encryptedSecret, destination_url, inboundToken, provider === 'whatsapp' ? whatsapp_verify_token : null]
  );

  return {
    inbound_url: `${PUBLIC_URL}/v1/inbound/${inboundToken}`,
    provider,
    org_id,
    destination_url,
  };
});

fastify.get('/api/v1/inbound-webhooks', { preHandler: [...requireAuthRateLimited, requireEnterpriseMode] }, async (request) => {
  const r = await pg.query(
    `SELECT id, org_id, provider, destination_url, inbound_token, created_at, last_used_at, revoked_at
     FROM inbound_webhooks WHERE user_id = $1 ORDER BY created_at DESC`,
    [request.user.sub]
  );
  return r.rows.map((row) => ({ ...row, inbound_url: `${PUBLIC_URL}/v1/inbound/${row.inbound_token}` }));
});

fastify.delete('/api/v1/inbound-webhooks/:id', { preHandler: [...requireAuthRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const r = await pg.query(
    `UPDATE inbound_webhooks SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id`,
    [request.params.id, request.user.sub]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Inbound webhook not found.' });
  return { revoked: true };
});

// Meta's WhatsApp webhook setup requires this GET handshake before it will
// register the callback URL at all — sends hub.mode/hub.verify_token/
// hub.challenge as query params, no signature (there's no body to sign
// yet). Echo back hub.challenge only if the verify token matches what was
// configured at registration time.
fastify.get('/v1/inbound/:token', async (request, reply) => {
  const reg = await pg.query(
    `SELECT provider, whatsapp_verify_token FROM inbound_webhooks WHERE inbound_token = $1 AND revoked_at IS NULL`,
    [request.params.token]
  );
  if (reg.rows.length === 0) return reply.status(404).send({ error: 'Not found.' });
  const { provider, whatsapp_verify_token } = reg.rows[0];
  if (provider !== 'whatsapp') return reply.status(404).send({ error: 'This provider does not use a GET handshake.' });

  const mode = request.query['hub.mode'];
  const verifyToken = request.query['hub.verify_token'];
  const challenge = request.query['hub.challenge'];
  if (mode === 'subscribe' && verifyToken && whatsapp_verify_token && timingSafeEqualStrings(verifyToken, whatsapp_verify_token)) {
    reply.type('text/plain').send(challenge);
    return;
  }
  return reply.status(403).send({ error: 'Verification failed.' });
});

// The actual inbound receiver — every provider's webhook lands here.
// Verifies the signature against that provider's real scheme (src/ee/hmac),
// and only forwards to the user's real destination if it's genuinely
// authentic. Returns a 5xx (not silently swallowing) if verification
// passes but the forward itself fails, so the provider's own retry
// mechanism kicks in rather than the webhook being silently dropped.
fastify.post('/v1/inbound/:token', async (request, reply) => {
  const reg = await pg.query(
    `SELECT id, user_id, org_id, provider, webhook_secret, destination_url FROM inbound_webhooks WHERE inbound_token = $1 AND revoked_at IS NULL`,
    [request.params.token]
  );
  if (reg.rows.length === 0) return reply.status(404).send({ error: 'Not found.' });
  const row = reg.rows[0];
  const secret = decryptCredential(row.webhook_secret);

  const rawBody = request.rawBody ? request.rawBody.toString('utf8') : '';
  const inboundUrl = `${PUBLIC_URL}/v1/inbound/${request.params.token}`;
  const result = verifyWebhookSignature(row.provider, {
    rawBody,
    headers: request.headers,
    requestUrl: inboundUrl,
    // Twilio and Mailgun both sign the parsed form body (not the raw
    // bytes) — see src/ee/hmac's verifyTwilio/verifyMailgun.
    params: (row.provider === 'twilio' || row.provider === 'mailgun') ? request.body : undefined,
    // HubSpot v3 signs method+uri+body+timestamp — uri is the exact
    // inbound URL the operator configured in their HubSpot app settings,
    // which must be this same inbound_url for the signature to match.
    method: row.provider === 'hubspot' ? request.method : undefined,
    uri: row.provider === 'hubspot' ? inboundUrl : undefined,
    secret,
  });

  if (!result.valid) {
    fastify.log.warn(`Inbound webhook signature verification failed for ${row.provider} (org ${row.org_id}): ${result.reason || 'signature mismatch'}`);
    return reply.status(401).send({ error: 'Invalid signature.' });
  }

  try {
    await axios.post(row.destination_url, request.rawBody, {
      headers: { 'Content-Type': request.headers['content-type'] || 'application/json' },
      timeout: 10000,
    });
  } catch (err) {
    fastify.log.error(`Inbound webhook forward to destination failed for ${row.provider} (org ${row.org_id}): ${err.message}`);
    return reply.status(502).send({ error: 'Signature verified, but could not forward to destination.' });
  }

  await pg.query('UPDATE inbound_webhooks SET last_used_at = NOW() WHERE id = $1', [row.id]);
  return { received: true };
});

// Collects the short request form before a self-host download unlocks —
// also enforces the connected-agent requirement here, so the form itself
// can't be submitted as a way around actually trying the product first.
fastify.post('/api/v1/download/self-host/request', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const hasConnectedAgent = await pg.query('SELECT 1 FROM api_keys WHERE user_id = $1 LIMIT 1', [request.user.sub]);
  if (hasConnectedAgent.rows.length === 0) {
    return reply.status(403).send({ error: 'Connect an agent first — the self-host download unlocks once you have.', code: 'NO_AGENT_CONNECTED' });
  }

  const { reason, company } = request.body || {};
  if (typeof reason !== 'string' || reason.trim().length < 3) {
    return reply.status(422).send({ error: 'Tell us a bit about what you\'ll use it for (at least a few words).' });
  }
  if (reason.length > 2000) {
    return reply.status(422).send({ error: 'Reason must be under 2000 characters.' });
  }
  const trimmedCompany = typeof company === 'string' ? company.trim().slice(0, 255) : null;

  await pg.query(
    'INSERT INTO self_host_download_requests (user_id, reason, company) VALUES ($1, $2, $3)',
    [request.user.sub, reason.trim(), trimmedCompany || null]
  );
  return { unlocked: true };
});

// Gated self-host package download — requires login, matching the "register,
// then download, then deploy" flow. Also gated on two further conditions
// (see POST /request below): the user must have actually connected an
// agent (real usage, not just curiosity), and must have submitted the
// short request form first. Zips from the local snapshot taken at
// container startup (SELF_HOST_SNAPSHOT_DIR — see the snapshot block near the
// top of this file), not read directly from /repo on each request, since that
// proved unreliable under rootless Podman's UID/SELinux handling. Excludes
// anything that shouldn't ship: secrets, .env files, node_modules, git history.
fastify.get('/api/v1/download/self-host', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const hasConnectedAgent = await pg.query('SELECT 1 FROM api_keys WHERE user_id = $1 LIMIT 1', [request.user.sub]);
  if (hasConnectedAgent.rows.length === 0) {
    return reply.status(403).send({ error: 'Connect an agent first — the self-host download unlocks once you have.', code: 'NO_AGENT_CONNECTED' });
  }
  const hasRequest = await pg.query('SELECT 1 FROM self_host_download_requests WHERE user_id = $1 LIMIT 1', [request.user.sub]);
  if (hasRequest.rows.length === 0) {
    return reply.status(403).send({ error: 'Submit the request form first.', code: 'NO_REQUEST_SUBMITTED' });
  }

  const repoPath = SELF_HOST_SNAPSHOT_DIR;
  if (!fs.existsSync(repoPath)) {
    return reply.status(500).send({ error: 'The self-host package is not available from this deployment.' });
  }

  // Pre-seed the downloader's own account into the package, so they don't
  // need to register and verify their email again on their own self-hosted
  // instance — they already proved they own this email address by logging
  // into AgentRaaS Cloud to get here. Reuses the existing password-reset
  // flow (a fresh single-use token) rather than inventing a new mechanism,
  // and rather than trying to carry over their actual Cloud password, which
  // would mean shipping a password hash inside a distributable file.
  //
  // IMPORTANT: all of this async work happens BEFORE reply.send(archive)
  // below. Doing it after the stream had already started (an earlier
  // version of this endpoint did) meant an error here could crash the
  // response mid-transfer, corrupting the download with no clean error
  // shown to the client — exactly what happened in testing.
  const seedEmail = request.user.email;
  const escapedEmail = seedEmail.replace(/'/g, "''");
  const rawSetupToken = crypto.randomBytes(32).toString('hex');
  const setupTokenHash = crypto.createHash('sha256').update(rawSetupToken).digest('hex');
  const unusablePasswordHash = await hashPassword(crypto.randomBytes(24).toString('hex'));

  const seedSql = `-- 010_seed_your_account.sql
-- Auto-generated when this package was downloaded. Pre-seeds YOUR account
-- (${seedEmail}) so you don't need to register again on this self-hosted
-- instance — you already verified this email on AgentRaaS Cloud.
-- Set your password using the link in SETUP_INSTRUCTIONS.txt.

INSERT INTO users (email, password_hash, is_admin, email_verified)
VALUES ('${escapedEmail}', '${unusablePasswordHash}', true, true)
ON CONFLICT (email) DO NOTHING;

INSERT INTO password_reset_tokens (user_id, token_hash, expires_at)
SELECT id, '${setupTokenHash}', NOW() + INTERVAL '7 days'
FROM users WHERE email = '${escapedEmail}';
`;

  const instructionsTxt = `AgentRaaS self-host — your account is pre-registered
=====================================================

Email: ${seedEmail}

This package was generated for your account, so you don't need to
register again. After running ./install.sh and the dashboard is up:

1. Go to http://localhost:13000/dashboard
2. Click "Forgot password" and enter: ${seedEmail}
   -- OR --
   Use this direct link (valid 7 days, works once):
   http://localhost:13000/dashboard?reset_token=${rawSetupToken}

Either way, you'll set a new password for THIS self-hosted instance
(separate from your AgentRaaS Cloud password) and be logged in as an
admin on this deployment.

If your instance runs on a different host/port than localhost:13000,
replace that part of the URL above accordingly.
`;

  // Everything above was synchronous prep or already-awaited async work.
  // From here on it's all synchronous archive setup, then send — no more
  // awaits in between, so nothing can crash the stream mid-transfer.
  reply.header('Content-Type', 'application/zip');
  reply.header('Content-Disposition', 'attachment; filename="agentraas-self-host.zip"');

  const archive = archiver('zip', { zlib: { level: 9 } });
  archive.on('error', (err) => { fastify.log.error({ err }, 'Zip packaging failed'); });

  // Explicit allowlist of what to include, rather than trying to glob
  // everything and exclude data/ and secrets/ via an ignore pattern — the
  // underlying glob library still needs to scandir an excluded directory to
  // know what's inside it before filtering it out, which fails here since
  // data/minio is owned by a different container's user, unreadable even
  // through the read-only mount. Never touching those directories at all
  // sidesteps the permission issue entirely.
  //
  // A few scripts under infra/scripts are cloud-operator tools, not
  // self-host tools — they manage the shared AgentRaaS Cloud deployment
  // (the single admin account, per-org limit overrides on an enforced
  // limit, our own internal test tooling) and don't apply to a self-hosted
  // single-tenant instance, where the downloader is already made admin of
  // their own instance automatically (see the seed SQL above) and no usage
  // limit is enforced at all.
  //
  // Migrations 013 and 014 are also cloud-specific — both hardcode the
  // literal email admin@agentraas.local. Running them on a self-host
  // instance would demote the downloader's own just-seeded admin account
  // (013's "demote everyone except admin@agentraas.local" doesn't match
  // their real email), stripping their admin access the moment the
  // database initializes. Self-host doesn't need single-admin enforcement
  // anyway — it's inherently single-tenant.
  const excludedScripts = new Set(['bootstrap-admin.sh', 'set-org-limit.sh', 'create-test-user.sh']);
  const excludedMigrations = new Set(['013_single_admin_and_forced_password_change.sql', '014_move_admin_to_local_range.sql']);
  const includeDirs = ['src', 'infra', 'config'];
  for (const dir of includeDirs) {
    const fullDirPath = path.join(repoPath, dir);
    if (fs.existsSync(fullDirPath)) {
      archive.directory(fullDirPath, dir, (entryData) => {
        if (entryData.name.includes('node_modules/')) return false;
        if (entryData.name === '.env' || entryData.name.endsWith('/.env')) return false;
        if (entryData.name.endsWith('.log')) return false;
        const baseName = entryData.name.split('/').pop();
        if (excludedScripts.has(baseName)) return false;
        if (excludedMigrations.has(baseName)) return false;
        return entryData;
      });
    }
  }

  const includeFiles = [
    'README.md', 'LICENSE.md', 'PRIVACY.md', 'TERMS.md', 'SECURITY.md',
    'CODE_OF_CONDUCT.md', 'CONTRIBUTING.md', 'GETTING_STARTED.md', 'compose.yaml', 'install.sh',
    '.env.example', '.gitignore',
  ];
  for (const file of includeFiles) {
    const fullFilePath = path.join(repoPath, file);
    if (fs.existsSync(fullFilePath)) {
      archive.file(fullFilePath, { name: file });
    }
  }

  archive.append(seedSql, { name: 'infra/migrations/010_seed_your_account.sql' });
  archive.append(instructionsTxt, { name: 'SETUP_INSTRUCTIONS.txt' });
  archive.finalize();

  reply.send(archive);
});


fastify.post('/api/v1/demo/seed', { preHandler: requireAdminRateLimited }, async (request) => {
  const services = Object.keys(SERVICE_CONFIG).map(s => ({s, actions: Object.keys(SERVICE_CONFIG[s].actions)}));
  const agents = ['agent_invoice','agent_booking','agent_crm','agent_payment','agent_sms','agent_whatsapp'];
  const statuses = ['success','success','success','deduplicated','blocked','error'];

  // Seed under an org the calling user actually owns, so the data they just
  // asked to see is actually visible on their own (now properly org-scoped)
  // dashboard. If they don't own any org yet (haven't connected an agent),
  // create one demo org for them first, establishing ownership via a
  // dummy api_keys row — never a usable key, just an ownership record.
  let orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) {
    const demoOrgId = `demo_${request.user.sub}`;
    const dummyKey = 'ar_demo_' + crypto.randomBytes(24).toString('hex');
    const dummyKeyHash = crypto.createHash('sha256').update(dummyKey).digest('hex');
    await pg.query(
      `INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)`,
      [request.user.sub, demoOrgId, 'demo_agent', 'Demo data', dummyKeyHash, dummyKey.slice(0, 16)]
    );
    orgIds = [demoOrgId];
  }

  for (let i=0; i<30; i++) {
    const svc = services[Math.floor(Math.random()*services.length)];
    const action = svc.actions[Math.floor(Math.random()*svc.actions.length)];
    const org = orgIds[Math.floor(Math.random()*orgIds.length)];
    const agent = agents[Math.floor(Math.random()*agents.length)];
    const status = statuses[Math.floor(Math.random()*statuses.length)];
    await pg.query(`INSERT INTO audit_log (req_id,api_key,org_id,agent_id,service,action,status,error_type,duration_ms,payload_hash,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,NOW() - INTERVAL '${Math.floor(Math.random()*24)} hours')`, [
      'req_'+crypto.randomBytes(8).toString('hex'), 'ak_demo', org, agent, svc.s, action, status,
      status==='blocked'?'validation_failed':(status==='error'?'upstream_timeout':null),
      Math.floor(Math.random()*200)+10, 'hash_'+i
    ]);
  }

  // Everything below gives the newer reliability panels (Reliability
  // report, Failed requests, Active monitoring, Custom actions) something
  // to show in a demo too, instead of showing up empty next to the
  // populated activity feed above. Outage alerts (notification_webhooks)
  // is deliberately NOT seeded here — it needs a real Slack/Discord/
  // Telegram target to be worth showing, so that one's a 10-second live
  // "add it" during the demo instead of fake data.
  const demoOrg = orgIds[0];

  // Circuit breaker history for one service — a realistic ~35min outage
  // and recovery ending ~19h ago, so the reliability report shows a real
  // (not flat 100%) uptime number for "stripe" alongside 100% for
  // everything else that never tripped.
  await pg.query(
    `INSERT INTO circuit_breaker_events (service, from_state, to_state, occurred_at) VALUES
     ('stripe', 'closed', 'open', NOW() - INTERVAL '20 hours'),
     ('stripe', 'open', 'half-open', NOW() - INTERVAL '19 hours 30 minutes'),
     ('stripe', 'half-open', 'closed', NOW() - INTERVAL '19 hours 25 minutes')`
  );

  // Dead-letter queue — two open (unreplayed) entries so "Failed requests"
  // has something to Edit & Replay or Dismiss during a demo.
  await pg.query(
    `INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7, NOW() - INTERVAL '3 hours')`,
    ['req_'+crypto.randomBytes(8).toString('hex'), demoOrg, 'agent_payment', 'stripe', 'charge.create',
     encryptCredential(JSON.stringify({ amount: 4999, currency: 'usd', customer: 'cus_demo123' })),
     'Upstream returned 503: Service temporarily unavailable']
  );
  await pg.query(
    `INSERT INTO dead_letter_queue (req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7, NOW() - INTERVAL '1 hour')`,
    ['req_'+crypto.randomBytes(8).toString('hex'), demoOrg, 'agent_whatsapp', 'whatsapp', 'message.send',
     encryptCredential(JSON.stringify({ to: '+14155552671', body: 'Your order has shipped!' })),
     'Upstream returned 500: Internal Server Error']
  );

  // One example Custom Action, pointed at a safe public echo endpoint
  // (httpbin.org) so it actually works if clicked/tested during a demo —
  // never a real customer-facing URL.
  await pg.query(
    `INSERT INTO custom_actions (user_id, org_id, name, method, target_url, auth_type, content_type)
     VALUES ($1,$2,$3,$4,$5,$6,$7)
     ON CONFLICT (org_id, name) WHERE revoked_at IS NULL DO NOTHING`,
    [request.user.sub, demoOrg, 'internal-order-webhook', 'POST', 'https://httpbin.org/post', 'none', 'application/json']
  );

  // Active monitoring on "mockpay" — the one service eligible that needs
  // no real credentials, so this actually runs for real on the next 5-min
  // scheduler tick and shows a genuine passing check, not fake data.
  await pg.query(
    `INSERT INTO health_check_settings (org_id, service, enabled_by) VALUES ($1,$2,$3) ON CONFLICT (org_id, service) DO NOTHING`,
    [demoOrg, 'mockpay', request.user.sub]
  );

  return { seeded: 30, circuit_events: 3, dlq_entries: 2, custom_actions: 1, health_checks_enabled: 1 };
});

// ─── SELF-SERVE CREDENTIALS: users add their own Stripe/WhatsApp/etc. keys ───
// Shared by both the standalone Credentials panel and Custom Action creation
// (which can save a credential in the same request for a one-step setup flow).
async function saveCredential(userId, orgId, serviceKey, credentials) {
  const encrypted = encryptCredential(JSON.stringify(credentials));
  const preview = maskedPreview(credentials);
  const client = await pg.connect();
  try {
    await client.query('BEGIN');
    await client.query(
      `UPDATE service_credentials SET revoked_at = NOW() WHERE org_id=$1 AND service=$2 AND revoked_at IS NULL`,
      [orgId, serviceKey]
    );
    await client.query(
      `INSERT INTO service_credentials (user_id, org_id, service, encrypted_payload, masked_preview) VALUES ($1,$2,$3,$4,$5)`,
      [userId, orgId, serviceKey, encrypted, preview]
    );
    await client.query('COMMIT');
  } catch (err) {
    await client.query('ROLLBACK');
    throw err;
  } finally {
    client.release();
  }
  return preview;
}

fastify.post('/api/v1/credentials', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, service, credentials } = request.body || {};
  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  const tenantCap = await checkAgencyTenantCap(request.user.sub, org_id);
  if (!tenantCap.ok) {
    return reply.status(402).send({ error: `Agency plan is limited to ${tenantCap.limit} client tenants. Contact support@agentraas.io to increase this.` });
  }

  // Accepts either a curated service name (from services.json) or a "custom:<name>"
  // key belonging to one of this org's registered custom actions — lets a user
  // rotate a custom action's credential independently, without re-registering it.
  const isCustomKey = typeof service === 'string' && service.startsWith('custom:');
  if (!service || (!SERVICE_CONFIG[service] && !isCustomKey)) {
    return reply.status(422).send({ error: 'A valid service name is required.' });
  }
  if (isCustomKey) {
    const customName = service.slice('custom:'.length);
    const exists = await pg.query(
      `SELECT 1 FROM custom_actions WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL`,
      [org_id, customName]
    );
    if (exists.rows.length === 0) {
      return reply.status(422).send({ error: `No custom action named "${customName}" registered for this org.` });
    }
  }

  if (!credentials || typeof credentials !== 'object' || Object.keys(credentials).length === 0) {
    return reply.status(422).send({ error: 'credentials object is required (e.g. { "api_key": "..." }).' });
  }

  const preview = await saveCredential(request.user.sub, org_id, service, credentials);

  return { saved: true, org_id, service, masked_preview: preview };
});

fastify.get('/api/v1/credentials', { preHandler: requireAuthRateLimited }, async (request) => {
  const r = await pg.query(
    `SELECT id, org_id, service, masked_preview, created_at
     FROM service_credentials WHERE user_id = $1 AND revoked_at IS NULL ORDER BY service`,
    [request.user.sub]
  );
  return r.rows;
});

fastify.delete('/api/v1/credentials/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const r = await pg.query(
    `UPDATE service_credentials SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id`,
    [request.params.id, request.user.sub]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Credential not found.' });
  return { revoked: true };
});

// Header names, per RFC 7230 token chars — kept narrow (no spaces/colons)
// since these get set directly as outbound HTTP headers.
function isValidHeaderName(name) {
  return typeof name === 'string' && /^[!#$%&'*+\-.^_`|~0-9A-Za-z]{1,100}$/.test(name);
}

const MAX_EXTRA_HEADERS = 10;
const MAX_FANOUT_URLS = 5;

// Dynamic Header & Secret Injection: validates and (for secret:true entries)
// encrypts a submitted extra_headers array before it's stored. Returns
// { error } or { headers: [...] } ready for JSONB storage.
function prepareExtraHeaders(extraHeaders) {
  if (extraHeaders === undefined) return { headers: [] };
  if (!Array.isArray(extraHeaders)) return { error: 'extra_headers must be an array.' };
  if (extraHeaders.length > MAX_EXTRA_HEADERS) return { error: `extra_headers supports at most ${MAX_EXTRA_HEADERS} entries.` };
  const prepared = [];
  for (const h of extraHeaders) {
    if (!h || typeof h !== 'object') return { error: 'Each extra_headers entry must be an object.' };
    if (!isValidHeaderName(h.name)) return { error: `"${h.name}" is not a valid header name.` };
    if (typeof h.value !== 'string' || h.value.length === 0 || h.value.length > 2000) {
      return { error: `extra_headers entry "${h.name}" needs a non-empty value (max 2000 characters).` };
    }
    prepared.push(h.secret ? { name: h.name, secret: true, value: encryptCredential(h.value) } : { name: h.name, secret: false, value: h.value });
  }
  return { headers: prepared };
}

// Multi-Destination Fan-Out: validates a submitted fanout_urls array with
// the exact same SSRF guard as target_url — a fan-out destination is just
// as capable of hitting internal infrastructure as the primary one.
async function prepareFanoutUrls(fanoutUrls) {
  if (fanoutUrls === undefined) return { urls: [] };
  if (!Array.isArray(fanoutUrls)) return { error: 'fanout_urls must be an array.' };
  if (fanoutUrls.length > MAX_FANOUT_URLS) return { error: `fanout_urls supports at most ${MAX_FANOUT_URLS} destinations.` };
  for (const url of fanoutUrls) {
    const err = await validateTargetUrl(url);
    if (err) return { error: `fanout_urls: ${err}` };
  }
  return { urls: fanoutUrls };
}

// ─── CUSTOM ACTIONS: register any endpoint, not just curated services ───
fastify.post('/api/v1/custom-actions', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, name, method, target_url, auth_type, auth_header_name, content_type, credential, extra_headers, fanout_urls } = request.body || {};

  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!isValidIdentifier(name)) return reply.status(422).send({ error: 'name must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (name === 'custom') return reply.status(422).send({ error: '"custom" is reserved as the service keyword — pick a different name.' });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  const tenantCap = await checkAgencyTenantCap(request.user.sub, org_id);
  if (!tenantCap.ok) {
    return reply.status(402).send({ error: `Agency plan is limited to ${tenantCap.limit} client tenants. Contact support@agentraas.io to increase this.` });
  }

  const allowedMethods = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE'];
  const upperMethod = (method || 'POST').toUpperCase();
  if (!allowedMethods.includes(upperMethod)) return reply.status(422).send({ error: `method must be one of: ${allowedMethods.join(', ')}` });

  const allowedAuthTypes = ['none', 'bearer', 'basic', 'header'];
  const authTypeValue = auth_type || 'none';
  if (!allowedAuthTypes.includes(authTypeValue)) return reply.status(422).send({ error: `auth_type must be one of: ${allowedAuthTypes.join(', ')}` });
  if (authTypeValue === 'header' && !auth_header_name) return reply.status(422).send({ error: 'auth_header_name is required when auth_type is "header".' });
  if (authTypeValue !== 'none' && (!credential || typeof credential !== 'object' || Object.keys(credential).length === 0)) {
    return reply.status(422).send({ error: 'A credential is required when auth_type is not "none".' });
  }

  const urlError = await validateTargetUrl(target_url || '');
  if (urlError) return reply.status(422).send({ error: urlError });

  const extraHeadersResult = prepareExtraHeaders(extra_headers);
  if (extraHeadersResult.error) return reply.status(422).send({ error: extraHeadersResult.error });
  const fanoutResult = await prepareFanoutUrls(fanout_urls);
  if (fanoutResult.error) return reply.status(422).send({ error: fanoutResult.error });

  const client = await pg.connect();
  try {
    await client.query('BEGIN');
    await client.query(`UPDATE custom_actions SET revoked_at = NOW() WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL`, [org_id, name]);
    await client.query(
      `INSERT INTO custom_actions (user_id, org_id, name, method, target_url, auth_type, auth_header_name, content_type, extra_headers, fanout_urls)
       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)`,
      [request.user.sub, org_id, name, upperMethod, target_url, authTypeValue, auth_header_name || null, content_type || 'application/json',
       JSON.stringify(extraHeadersResult.headers), JSON.stringify(fanoutResult.urls)]
    );
    await client.query('COMMIT');
  } catch (err) {
    await client.query('ROLLBACK');
    throw err;
  } finally {
    client.release();
  }

  if (authTypeValue !== 'none') {
    await saveCredential(request.user.sub, org_id, `custom:${name}`, credential);
  }

  return { saved: true, org_id, name, note: `Agents can now call this via service:"custom", action:"${name}".` };
});

fastify.get('/api/v1/custom-actions', { preHandler: requireAuthRateLimited }, async (request) => {
  const r = await pg.query(
    `SELECT id, org_id, name, method, target_url, auth_type, created_at,
            jsonb_array_length(extra_headers) as extra_header_count,
            jsonb_array_length(fanout_urls) as fanout_url_count
     FROM custom_actions WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC`,
    [request.user.sub]
  );
  return r.rows;
});

fastify.delete('/api/v1/custom-actions/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const r = await pg.query(
    `UPDATE custom_actions SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id`,
    [request.params.id, request.user.sub]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Custom action not found.' });
  return { revoked: true };
});

// ─── VALIDATION RULES (custom validation rule builder) ───
// Lets an org define its own payload validation for any service.action —
// including Custom Actions, which otherwise have no validation at all (see
// the comment in src/core/proxy — the human who registered one owns
// responsibility for its shape, unless they define a rule here). For
// curated services this always overrides the static config/services.json
// rule for that action, rather than merging with it — one rule, one source
// of truth, easier to reason about than a field-by-field merge.
// Curated service actions are dotted (e.g. "charge.create" — see
// config/services.json), so this needs to be a bit more permissive than
// isValidIdentifier (org_id/agent_id/custom-action names, none of which
// contain dots) while still keeping the charset safe for a JSONB/SQL key.
function isValidActionName(value) {
  return typeof value === 'string' && value.length > 0 && value.length <= 100 && /^[a-zA-Z0-9_.-]+$/.test(value);
}

fastify.post('/api/v1/validation-rules', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, service, action, fields } = request.body || {};
  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!isValidIdentifier(service)) return reply.status(422).send({ error: 'service must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!isValidActionName(action)) return reply.status(422).send({ error: 'action must be 1-100 characters, letters/numbers/dots/underscore/hyphen only.' });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  const fieldsError = isValidRuleDefinition(fields);
  if (fieldsError) return reply.status(422).send({ error: fieldsError });

  const r = await pg.query(
    `INSERT INTO custom_validation_rules (org_id, service, action, fields, created_by)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (org_id, service, action) DO UPDATE SET fields = EXCLUDED.fields, updated_at = NOW()
     RETURNING id, org_id, service, action, fields, updated_at`,
    [org_id, service, action, JSON.stringify(fields), request.user.sub]
  );
  return { saved: true, rule: r.rows[0] };
});

fastify.get('/api/v1/validation-rules', { preHandler: requireAuthRateLimited }, async (request) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return [];
  const r = await pg.query(
    `SELECT id, org_id, service, action, fields, created_at, updated_at
     FROM custom_validation_rules WHERE org_id = ANY($1) ORDER BY updated_at DESC`,
    [orgIds]
  );
  return r.rows;
});

fastify.delete('/api/v1/validation-rules/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return reply.status(404).send({ error: 'Validation rule not found.' });
  const r = await pg.query(
    `DELETE FROM custom_validation_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id`,
    [request.params.id, orgIds]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Validation rule not found.' });
  return { deleted: true };
});

// Lets the dashboard's rule builder show a live pass/fail preview against a
// sample payload before saving — pure function, no DB write, so it's safe
// to call on every keystroke without rate-limit concerns beyond the normal
// dashboard limiter.
fastify.post('/api/v1/validation-rules/test', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { fields, payload } = request.body || {};
  const fieldsError = isValidRuleDefinition(fields);
  if (fieldsError) return reply.status(422).send({ error: fieldsError });
  const validationError = validateFields(payload || {}, fields);
  return { valid: !validationError, error: validationError };
});

// ─── PER-FIELD DEDUP RULES ───
// Lets an org dedupe on a chosen subset of a payload's own fields (e.g.
// "email" — same email counts as a duplicate regardless of what else is in
// the payload) instead of the default whole-payload hash, without every
// caller having to supply its own idempotency key. See
// getEffectiveDedupRule and hashFieldValues (src/core/proxy) for how this
// is resolved and applied on the request path; an explicit per-call
// idempotency key still always takes precedence over a configured rule.
fastify.post('/api/v1/dedup-rules', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, service, action, fields } = request.body || {};
  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!isValidIdentifier(service)) return reply.status(422).send({ error: 'service must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!isValidActionName(action)) return reply.status(422).send({ error: 'action must be 1-100 characters, letters/numbers/dots/underscore/hyphen only.' });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  const fieldsError = isValidDedupRuleDefinition(fields);
  if (fieldsError) return reply.status(422).send({ error: fieldsError });

  const r = await pg.query(
    `INSERT INTO custom_dedup_rules (org_id, service, action, fields, created_by)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (org_id, service, action) DO UPDATE SET fields = EXCLUDED.fields, updated_at = NOW()
     RETURNING id, org_id, service, action, fields, updated_at`,
    [org_id, service, action, JSON.stringify(fields), request.user.sub]
  );
  return { saved: true, rule: r.rows[0] };
});

fastify.get('/api/v1/dedup-rules', { preHandler: requireAuthRateLimited }, async (request) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return [];
  const r = await pg.query(
    `SELECT id, org_id, service, action, fields, created_at, updated_at
     FROM custom_dedup_rules WHERE org_id = ANY($1) ORDER BY updated_at DESC`,
    [orgIds]
  );
  return r.rows;
});

fastify.delete('/api/v1/dedup-rules/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return reply.status(404).send({ error: 'Dedup rule not found.' });
  const r = await pg.query(
    `DELETE FROM custom_dedup_rules WHERE id = $1 AND org_id = ANY($2) RETURNING id`,
    [request.params.id, orgIds]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Dedup rule not found.' });
  return { deleted: true };
});

// ─── NOTIFICATION WEBHOOKS (instant outage notifications) ───
fastify.post('/api/v1/notification-webhooks', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, type, target, chat_id } = request.body || {};
  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  if (!['slack', 'discord', 'telegram'].includes(type)) return reply.status(422).send({ error: 'type must be one of: slack, discord, telegram.' });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  if (typeof target !== 'string' || target.length === 0 || target.length > 500) {
    return reply.status(422).send({ error: type === 'telegram' ? 'target (bot token) is required.' : 'target (webhook URL) is required.' });
  }
  if ((type === 'slack' || type === 'discord') && (await validateTargetUrl(target))) {
    return reply.status(422).send({ error: await validateTargetUrl(target) });
  }
  if (type === 'telegram' && (typeof chat_id !== 'string' || chat_id.length === 0)) {
    return reply.status(422).send({ error: 'chat_id is required for type "telegram".' });
  }

  const encryptedTarget = encryptCredential(target);
  const r = await pg.query(
    `INSERT INTO notification_webhooks (org_id, type, encrypted_target, extra, created_by)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (org_id, type) DO UPDATE SET encrypted_target = EXCLUDED.encrypted_target, extra = EXCLUDED.extra
     RETURNING id, org_id, type, created_at`,
    [org_id, type, encryptedTarget, type === 'telegram' ? chat_id : null, request.user.sub]
  );
  return { saved: true, webhook: r.rows[0] };
});

fastify.get('/api/v1/notification-webhooks', { preHandler: requireAuthRateLimited }, async (request) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return [];
  const r = await pg.query(
    `SELECT id, org_id, type, created_at FROM notification_webhooks WHERE org_id = ANY($1) ORDER BY created_at DESC`,
    [orgIds]
  );
  return r.rows;
});

fastify.delete('/api/v1/notification-webhooks/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return reply.status(404).send({ error: 'Notification webhook not found.' });
  const r = await pg.query(
    `DELETE FROM notification_webhooks WHERE id = $1 AND org_id = ANY($2) RETURNING id`,
    [request.params.id, orgIds]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Notification webhook not found.' });
  return { deleted: true };
});

fastify.post('/api/v1/notification-webhooks/:id/test', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  const row = (await pg.query(
    `SELECT org_id FROM notification_webhooks WHERE id = $1 AND org_id = ANY($2)`,
    [request.params.id, orgIds]
  )).rows[0];
  if (!row) return reply.status(404).send({ error: 'Notification webhook not found.' });
  await sendOutageNotification(row.org_id, '🔔 AgentRaaS test notification — if you can see this, outage alerts are wired up correctly.');
  return { sent: true };
});

// ─── ACTIVE HEALTH CHECKS (proactive, opt-in monitoring) ───
fastify.get('/api/v1/health-checks', { preHandler: requireAuthRateLimited }, async (request) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  const supportedServices = Object.keys(HEALTH_CHECK_SPECS);
  if (orgIds.length === 0) return { supported_services: supportedServices, enabled: [] };
  const r = await pg.query(
    `SELECT hs.org_id, hs.service, hs.enabled_at,
            lr.ok AS last_ok, lr.latency_ms AS last_latency_ms, lr.error AS last_error, lr.checked_at AS last_checked_at
     FROM health_check_settings hs
     LEFT JOIN LATERAL (
       SELECT ok, latency_ms, error, checked_at FROM health_check_results
       WHERE org_id = hs.org_id AND service = hs.service ORDER BY checked_at DESC LIMIT 1
     ) lr ON true
     WHERE hs.org_id = ANY($1) ORDER BY hs.enabled_at DESC`,
    [orgIds]
  );
  return { supported_services: supportedServices, enabled: r.rows };
});

fastify.post('/api/v1/health-checks', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, service } = request.body || {};
  if (!isValidIdentifier(org_id)) return reply.status(422).send({ error: 'org_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  const spec = HEALTH_CHECK_SPECS[service];
  if (!spec) return reply.status(422).send({ error: `Active health checks aren't available for "${service}" yet. Supported: ${Object.keys(HEALTH_CHECK_SPECS).join(', ')}.` });
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  // This will make a real request to the live service every 5 minutes using
  // this org's own stored credential — refuse to enable it for a service
  // with nothing configured, which would just fail (and alert) forever.
  if (!spec.internal) {
    const credential = await getCredential(service, org_id);
    if (!credential) return reply.status(422).send({ error: `No ${service} credentials configured for this org yet. Add them from the Credentials panel first.` });
  }
  await pg.query(
    `INSERT INTO health_check_settings (org_id, service, enabled_by) VALUES ($1, $2, $3)
     ON CONFLICT (org_id, service) DO NOTHING`,
    [org_id, service, request.user.sub]
  );
  return { enabled: true, org_id, service, interval_minutes: HEALTH_CHECK_INTERVAL_MS / 60000 };
});

fastify.delete('/api/v1/health-checks/:org_id/:service', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return reply.status(404).send({ error: 'Health check not found.' });
  const r = await pg.query(
    `DELETE FROM health_check_settings WHERE org_id = $1 AND service = $2 AND org_id = ANY($3) RETURNING id`,
    [request.params.org_id, request.params.service, orgIds]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Health check not found.' });
  return { disabled: true };
});

// ─── DEAD LETTER QUEUE (one-click payload replay) ───
fastify.get('/api/v1/dead-letter-queue', { preHandler: requireAuthRateLimited }, async (request) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return [];
  const r = await pg.query(
    `SELECT id, req_id, org_id, agent_id, service, action, encrypted_payload, error_message, created_at
     FROM dead_letter_queue
     WHERE org_id = ANY($1) AND replayed_at IS NULL AND dismissed_at IS NULL
     ORDER BY created_at DESC LIMIT 100`,
    [orgIds]
  );
  // The payload is exactly as sensitive as any stored credential — decrypted
  // here (server-side, over the authenticated dashboard session) so the
  // "Edit & Replay" UI has something to prefill, same trust boundary as
  // GET /api/v1/credentials already crossing for masked previews.
  return r.rows.map((row) => {
    let payload = null;
    try { payload = JSON.parse(decryptCredential(row.encrypted_payload)); } catch { /* leave null if corrupt */ }
    const { encrypted_payload, ...rest } = row;
    return { ...rest, payload };
  });
});

fastify.post('/api/v1/dead-letter-queue/:id/replay', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  const row = (await pg.query(
    `SELECT * FROM dead_letter_queue WHERE id = $1 AND org_id = ANY($2) AND replayed_at IS NULL AND dismissed_at IS NULL`,
    [request.params.id, orgIds]
  )).rows[0];
  if (!row) return reply.status(404).send({ error: 'Dead-letter entry not found (already replayed, dismissed, or not yours).' });
  if (!(await checkOrgWritePermission(request.user.sub, row.org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }

  // The dashboard's "edit parameters" flow — replay the original payload
  // as-is, or an edited one if the caller supplies { payload: {...} }.
  let payload;
  if (request.body?.payload !== undefined) {
    payload = request.body.payload;
  } else {
    try { payload = JSON.parse(decryptCredential(row.encrypted_payload)); }
    catch { return reply.status(500).send({ error: 'Stored payload could not be decrypted.' }); }
  }

  const resolvedRoute = row.service === 'custom'
    ? await resolveCustomRoute(row.org_id, row.action)
    : SERVICE_ROUTES[`${row.service}.${row.action}`];
  if (!resolvedRoute) {
    return reply.status(410).send({ error: 'This action no longer exists (the service or Custom Action was changed or removed since this failure).' });
  }

  const replayReqId = 'req_' + crypto.randomBytes(8).toString('hex');
  try {
    const result = await proxy.forwardAction(resolvedRoute, row.service, row.action, row.org_id, payload, replayReqId);
    await pg.query('UPDATE dead_letter_queue SET replayed_at = NOW() WHERE id = $1', [row.id]);
    await logAudit(replayReqId, `replay:user_${request.user.sub}`, row.org_id, row.agent_id, row.service, row.action, 'success', null, 0, null, payload);
    return { replayed: true, result, reqId: replayReqId };
  } catch (err) {
    const upstreamMessage = extractUpstreamErrorMessage(err.response?.data);
    await logAudit(replayReqId, `replay:user_${request.user.sub}`, row.org_id, row.agent_id, row.service, row.action, 'error', upstreamMessage || err.message, 0, null);
    return reply.status(err.response?.status || 500).send({ replayed: false, error: upstreamMessage || 'Replay failed — the target API returned an error again.', reqId: replayReqId });
  }
});

fastify.delete('/api/v1/dead-letter-queue/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgIds = await getUserOrgIds(request.user.sub);
  if (orgIds.length === 0) return reply.status(404).send({ error: 'Dead-letter entry not found.' });
  const r = await pg.query(
    `UPDATE dead_letter_queue SET dismissed_at = NOW() WHERE id = $1 AND org_id = ANY($2) AND replayed_at IS NULL AND dismissed_at IS NULL RETURNING id`,
    [request.params.id, orgIds]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Dead-letter entry not found.' });
  return { dismissed: true };
});

// Looks up a registered custom action and returns it shaped like a SERVICE_ROUTES
// entry, so the rest of the pipeline (dedup, validation, circuit breaker, forwarding)
// treats it identically to a curated service — no special-casing needed downstream.
async function resolveCustomRoute(orgId, actionName) {
  const r = await pg.query(
    `SELECT method, target_url, auth_type, auth_header_name, content_type, extra_headers, fanout_urls
     FROM custom_actions WHERE org_id=$1 AND name=$2 AND revoked_at IS NULL LIMIT 1`,
    [orgId, actionName]
  );
  if (r.rows.length === 0) return null;
  const row = r.rows[0];
  // Dynamic Header & Secret Injection — decrypt any secret-marked headers
  // now, so the forwarder's existing route.extraHeaders merge (see
  // src/core/proxy's forwardAction) just works, same as a curated
  // service's static config-driven extraHeaders.
  const extraHeaders = {};
  for (const h of row.extra_headers || []) {
    extraHeaders[h.name] = h.secret ? decryptCredential(h.value) : h.value;
  }
  return {
    method: row.method,
    url: row.target_url,
    internal: false,
    authType: row.auth_type === 'header' ? 'custom-header' : row.auth_type,
    authHeader: row.auth_type === 'header' ? row.auth_header_name : (row.auth_type === 'bearer' ? 'Authorization' : null),
    contentType: row.content_type,
    extraHeaders,
    fanoutUrls: row.fanout_urls || [],
    validation: {},
    credentialKey: `custom:${actionName}`, // isolates this action's credential from any same-named built-in service
  };
}

// ─── CONNECT AGENT: generate/list/revoke API keys ───
fastify.post('/api/v1/agents/connect', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const { org_id, agent_id, label } = request.body || {};
  if (!isValidIdentifier(org_id) || !isValidIdentifier(agent_id)) {
    return reply.status(422).send({ error: 'org_id and agent_id must be 1-100 characters, letters/numbers/underscore/hyphen only.' });
  }
  if (!(await checkOrgWritePermission(request.user.sub, org_id))) {
    return reply.status(403).send({ error: 'Auditors have read-only access to this org.' });
  }
  const tenantCap = await checkAgencyTenantCap(request.user.sub, org_id);
  if (!tenantCap.ok) {
    return reply.status(402).send({ error: `Agency plan is limited to ${tenantCap.limit} client tenants. Contact support@agentraas.io to increase this.` });
  }

  const rawKey = 'ar_live_' + crypto.randomBytes(24).toString('hex');
  const keyHash = crypto.createHash('sha256').update(rawKey).digest('hex');
  const keyPrefix = rawKey.slice(0, 16);

  await pg.query(
    `INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)`,
    [request.user.sub, org_id, agent_id, (label || '').slice(0, 255) || null, keyHash, keyPrefix]
  );

  const origin = PUBLIC_URL;
  return {
    api_key: rawKey, // shown once — not retrievable again after this response
    webhook_url: `${origin}/v1/webhook/${org_id}/${agent_id}`,
    mcp_url: `${origin}/mcp`,
    org_id,
    agent_id,
  };
});

fastify.get('/api/v1/agents/keys', { preHandler: requireAuthRateLimited }, async (request) => {
  const r = await pg.query(
    `SELECT id, org_id, agent_id, label, key_prefix, created_at, last_used_at, revoked_at
     FROM api_keys WHERE user_id = $1 ORDER BY created_at DESC`,
    [request.user.sub]
  );
  return r.rows;
});

fastify.delete('/api/v1/agents/keys/:id', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const r = await pg.query(
    `UPDATE api_keys SET revoked_at = NOW() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL RETURNING id`,
    [request.params.id, request.user.sub]
  );
  if (r.rows.length === 0) return reply.status(404).send({ error: 'Key not found.' });
  return { revoked: true };
});

// Revokes the old key and issues a fresh one for the same org/agent/label,
// in one action - the new key is shown once, same as Connect Agent's response.
fastify.post('/api/v1/agents/keys/:id/regenerate', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const existing = await pg.query(
    `SELECT org_id, agent_id, label FROM api_keys WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL`,
    [request.params.id, request.user.sub]
  );
  if (existing.rows.length === 0) return reply.status(404).send({ error: 'Key not found.' });
  const { org_id, agent_id, label } = existing.rows[0];

  await pg.query(`UPDATE api_keys SET revoked_at = NOW() WHERE id = $1`, [request.params.id]);

  const rawKey = 'ar_live_' + crypto.randomBytes(24).toString('hex');
  const keyHash = crypto.createHash('sha256').update(rawKey).digest('hex');
  const keyPrefix = rawKey.slice(0, 16);
  await pg.query(
    `INSERT INTO api_keys (user_id, org_id, agent_id, label, key_hash, key_prefix) VALUES ($1,$2,$3,$4,$5,$6)`,
    [request.user.sub, org_id, agent_id, label, keyHash, keyPrefix]
  );

  const origin = PUBLIC_URL;
  return {
    api_key: rawKey, // shown once - not retrievable again after this response
    webhook_url: `${origin}/v1/webhook/${org_id}/${agent_id}`,
    mcp_url: `${origin}/mcp`,
    org_id,
    agent_id,
  };
});

// ─── WEBHOOK + SDK + MCP (agent-facing, unchanged — protected by API key, not dashboard login) ───
// Payload hashing, Redis dedup, circuit breaker, and the unified forwarder
// now live in src/core/proxy; MCP JSON-RPC handling lives in src/core/mcp
// (see RESTRUCTURE_PLAN.md Phase 3). Constructed here via dependency
// injection since JS function declarations below (resolveCustomRoute,
// verifyApiKey, checkUsageLimit, getCredential, logAudit, etc.) are
// hoisted, so referencing them before their textual definition is safe.
const proxy = createProxy({
  redis, fastify, axios,
  SERVICE_ROUTES, validateFields, getEffectiveValidationRule, getEffectiveDedupRule,
  resolveCustomRoute, verifyApiKey, getEffectiveRateLimit,
  checkUsageLimit, incrementMonthlyUsage, getCredential,
  logAudit, extractUpstreamErrorMessage, AGENT_RATE_LIMIT_PER_MIN, ENTERPRISE_MODE,
  maintenanceQueue, notifyCircuitOpen, pg, encryptCredential,
  PROXY_RETRY_MAX_ATTEMPTS, PROXY_RETRY_BASE_DELAY_MS,
});
const mcp = createMcp({
  fastify, proxy, SERVICE_CONFIG, SERVICE_ROUTES, validateFields, getEffectiveValidationRule, getEffectiveDedupRule,
  resolveCustomRoute, verifyApiKey, getEffectiveRateLimit,
  checkUsageLimit, incrementMonthlyUsage, logAudit, extractUpstreamErrorMessage, notifyCircuitOpen,
  pg, encryptCredential,
});

fastify.post('/v1/webhook/:orgId/:agentId', async (request, reply) => proxy.handleRequest(request,reply,'webhook'));
fastify.post('/v1/sdk/:service/:action', async (request, reply) => proxy.handleRequest(request,reply,'sdk'));
fastify.post('/mcp', async (request, reply) => mcp.handleMCP(request,reply));

registerDashboardRoutes(fastify, {
  pg, redis, getUserOrgIds, getMonthlyUsage, getEffectiveLimit, currentMonthKey,
  getCircuitStatesBatch: proxy.getCircuitStatesBatch,
  DASHBOARD_RANGES, DASHBOARD_BUCKETS, DEPLOYMENT_MODE,
  SERVICE_CONFIG, requireAuthRateLimited, requireAdminRateLimited, usageEvents,
});

// ─── Pause & Buffer (Enterprise maintenance mode) ───
// Deployment-wide, not per-org — an operator concern, gated the same way
// as the other system-wide admin endpoints (requireAdminRateLimited).
fastify.get('/api/v1/admin/maintenance', { preHandler: [...requireAdminRateLimited, requireEnterpriseMode] }, async () => {
  return { paused: await maintenanceQueue.isPaused(), queued: await maintenanceQueue.queueLength() };
});

fastify.post('/api/v1/admin/maintenance/pause', { preHandler: [...requireAdminRateLimited, requireEnterpriseMode] }, async () => {
  await maintenanceQueue.pause();
  return { paused: true };
});

fastify.post('/api/v1/admin/maintenance/resume', { preHandler: [...requireAdminRateLimited, requireEnterpriseMode] }, async () => {
  await maintenanceQueue.resume();
  const { processed, failed } = await proxy.flushMaintenanceQueue();
  return { paused: false, flushed: processed, failed };
});

// ─── Tamper-evident audit log: integrity check + SIEM export ───
// The hash chain itself (prev_hash/row_hash) is computed unconditionally
// by a Postgres trigger for every row (see migration 024) — these two
// endpoints, which make use of it, are the Enterprise-gated part.

// Recomputes each row's hash from its stored fields and compares to what
// was actually persisted, walking the chain in order — any row altered
// after insert (bypassing the DB trigger that blocks UPDATE isn't
// possible for a normal client, but this also catches e.g. a restored-
// from-backup row that skipped the trigger) breaks the chain from that
// point forward. Bounded to the most recent `limit` rows by default — a
// full-table verify on a large deployment could be slow; pass a higher
// limit explicitly to check further back.
fastify.get('/api/v1/admin/audit/verify-integrity', { preHandler: [...requireAdminRateLimited, requireEnterpriseMode] }, async (request) => {
  const limit = Math.min(parseInt(request.query.limit, 10) || 5000, 50000);
  const result = await pg.query(
    `SELECT id, req_id, org_id, service, action, status, created_at::text AS created_at_text, prev_hash, row_hash
     FROM audit_log WHERE row_hash IS NOT NULL ORDER BY id DESC LIMIT $1`,
    [limit]
  );
  const rows = result.rows;
  // Not ordered/walked by id here on purpose: under concurrent writes, a
  // row's id (assigned by the id sequence) and its position in the hash
  // chain (assigned by the FOR UPDATE lock on audit_log_chain_tip — see
  // migration 024) are two independent serialization points that can
  // interleave differently, so a row with a smaller id is not guaranteed
  // to be the other's chain-predecessor. What every row DOES guarantee
  // regardless of ordering: its own row_hash must equal the hash of its
  // own stored fields (including its own prev_hash) — recomputing and
  // comparing that, per row, is what actually detects tampering (any
  // single field changed after insert changes this hash); a byRowHash
  // lookup is enough to confirm a row's claimed prev_hash genuinely
  // exists as another row's hash in the fetched window, without depending
  // on fetch order.
  const byRowHash = new Set(rows.map((r) => r.row_hash));

  const brokenIds = [];
  for (const row of rows) {
    const expectedHash = crypto.createHash('sha256')
      .update(`${row.prev_hash || ''}|${row.req_id}|${row.org_id || ''}|${row.service}|${row.action}|${row.status}|${row.created_at_text}`)
      .digest('hex');
    if (expectedHash !== row.row_hash) brokenIds.push(row.id);
  }
  // Reported separately from brokenIds (which only reflects genuine hash
  // tampering) — a prev_hash absent from this window is expected at the
  // boundary of a limited fetch, not evidence of anything wrong on its own.
  const rowsWithUnresolvedPredecessor = rows.filter((r) => r.prev_hash && !byRowHash.has(r.prev_hash)).length;

  return {
    checked: rows.length,
    intact: brokenIds.length === 0,
    broken_ids: brokenIds,
    rows_with_predecessor_outside_window: rowsWithUnresolvedPredecessor,
    note: rows.length >= limit
      ? `Checked the most recent ${limit} rows only — pass a higher ?limit to check further back.`
      : 'Checked every hash-chained row in the table.',
  };
});

// NDJSON (newline-delimited JSON) — the most broadly ingestible shape for
// Splunk HEC, Datadog, Elastic, and most other SIEM/log-pipeline tools
// without a bespoke parser. Defaults to the last 24h; ?since/?until accept
// any Date-parseable string, for scheduled incremental pulls. Capped at
// 50k rows per request, same "bounded export" precedent as the existing
// CSV export (LIMIT 10000).
fastify.get('/api/v1/admin/audit/siem-export', { preHandler: [...requireAdminRateLimited, requireEnterpriseMode] }, async (request, reply) => {
  const since = request.query.since ? new Date(request.query.since) : new Date(Date.now() - 24 * 60 * 60 * 1000);
  const until = request.query.until ? new Date(request.query.until) : new Date();
  const result = await pg.query(
    `SELECT id, req_id, api_key, org_id, agent_id, service, action, status, error_type, duration_ms, payload_hash, row_hash, prev_hash, created_at
     FROM audit_log WHERE created_at >= $1 AND created_at <= $2 ORDER BY id ASC LIMIT 50000`,
    [since, until]
  );
  const lines = result.rows.map((row) => JSON.stringify({
    source: 'agentraas',
    event_type: 'agent_action',
    event_id: row.id,
    request_id: row.req_id,
    timestamp: row.created_at,
    org_id: row.org_id,
    agent_id: row.agent_id,
    api_key_prefix: row.api_key,
    service: row.service,
    action: row.action,
    outcome: row.status,
    error_type: row.error_type,
    duration_ms: row.duration_ms,
    payload_hash: row.payload_hash,
    integrity: { row_hash: row.row_hash, prev_hash: row.prev_hash },
  }));
  reply.header('Content-Type', 'application/x-ndjson').header('Content-Disposition', 'attachment; filename="agentraas-audit-siem-export.ndjson"');
  return lines.length ? lines.join('\n') + '\n' : '';
});

// ─── Agency tier: white-label branding ───
// GET is public (unauthenticated) — this is what a client-facing branded
// view fetches to theme itself, before that client's own user necessarily
// has a session. PUT requires owning the org (the existing loose
// ownership notion) AND being on the agency plan — white-labeling is
// specifically an agency-tier perk, not something a free/pro org gets.
fastify.get('/api/v1/org-branding/:orgId', async (request) => {
  const result = await pg.query('SELECT display_name, logo_url FROM org_branding WHERE org_id = $1', [request.params.orgId]);
  return result.rows[0] || { display_name: null, logo_url: null };
});

fastify.put('/api/v1/org-branding/:orgId', { preHandler: requireAuthRateLimited }, async (request, reply) => {
  const orgId = request.params.orgId;
  const ownedOrgIds = await getUserOrgIds(request.user.sub);
  if (!ownedOrgIds.includes(orgId)) return reply.status(403).send({ error: 'You do not own this org.' });
  const userResult = await pg.query('SELECT plan FROM users WHERE id = $1', [request.user.sub]);
  if (userResult.rows[0]?.plan !== 'agency') {
    return reply.status(402).send({ error: 'White-label branding requires the Agency plan.' });
  }
  const { display_name, logo_url } = request.body || {};
  if (display_name !== undefined && (typeof display_name !== 'string' || display_name.length > 255)) {
    return reply.status(422).send({ error: 'display_name must be a string up to 255 characters.' });
  }
  if (logo_url !== undefined && logo_url !== null) {
    const urlError = await validateTargetUrl(logo_url);
    if (urlError) return reply.status(422).send({ error: `logo_url: ${urlError}` });
  }
  await pg.query(
    `INSERT INTO org_branding (org_id, display_name, logo_url, updated_at) VALUES ($1, $2, $3, NOW())
     ON CONFLICT (org_id) DO UPDATE SET display_name = EXCLUDED.display_name, logo_url = EXCLUDED.logo_url, updated_at = NOW()`,
    [orgId, display_name || null, logo_url || null]
  );
  return { updated: true };
});

// ─── AUDIT ───
// rawPayload is optional (defaults to null) so every existing call site
// keeps working unchanged - only call sites that explicitly pass a
// payload get a redacted preview stored; everything else behaves exactly
// as before this column existed. Also gated on ENTERPRISE_MODE — DLP
// redaction is an Enterprise-tier feature (see src/ee/dlp), Community-tier
// audit rows keep redacted_payload_preview NULL exactly as before this
// feature existed.
async function logAudit(reqId,apiKey,orgId,agentId,service,action,status,errorType,durationMs,payloadHash,rawPayload=null) {
  try {
    const redactedPreview = (ENTERPRISE_MODE && rawPayload) ? JSON.stringify(redactPII(rawPayload)) : null;
    await pg.query(`INSERT INTO audit_log (req_id,api_key,org_id,agent_id,service,action,status,error_type,duration_ms,payload_hash,redacted_payload_preview,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,NOW())`, [reqId,maskApiKeyForAudit(apiKey),orgId,agentId,service,action,status,errorType,durationMs,payloadHash,redactedPreview]);
  } catch (err) { fastify.log.error({err},'Audit log failed'); }
}

// ─── GRACEFUL SHUTDOWN ───
process.on('SIGTERM', async () => {
  fastify.log.info('SIGTERM received, closing gracefully...');
  await fastify.close();
  await pg.end();
  await redis.quit();
  process.exit(0);
});

process.on('SIGINT', async () => {
  fastify.log.info('SIGINT received, closing gracefully...');
  await fastify.close();
  await pg.end();
  await redis.quit();
  process.exit(0);
});

// ─── AUDIT LOG RETENTION ───
// Free-tier orgs: 30 days. Agency-tier orgs: 1 year (an absolute ceiling —
// no plan retains data forever, mainly for storage/cost reasons). There
// was no cleanup job at all before this — audit_log would otherwise grow
// unbounded indefinitely.
async function cleanupAuditLogRetention() {
  try {
    const freeResult = await pg.query(`
      DELETE FROM audit_log
      WHERE created_at < NOW() - INTERVAL '30 days'
      AND org_id NOT IN (
        SELECT org_id FROM users WHERE plan = 'agency' AND org_id IS NOT NULL
        UNION SELECT a.org_id FROM api_keys a JOIN users u ON a.user_id = u.id WHERE u.plan = 'agency'
        UNION SELECT c.org_id FROM custom_actions c JOIN users u ON c.user_id = u.id WHERE u.plan = 'agency'
        UNION SELECT s.org_id FROM service_credentials s JOIN users u ON s.user_id = u.id WHERE u.plan = 'agency'
      )
    `);
    const ceilingResult = await pg.query(`DELETE FROM audit_log WHERE created_at < NOW() - INTERVAL '365 days'`);
    if (freeResult.rowCount > 0 || ceilingResult.rowCount > 0) {
      fastify.log.info(`Audit log retention cleanup: removed ${freeResult.rowCount} free-tier rows (30d+), ${ceilingResult.rowCount} rows past the 1-year ceiling.`);
    }
  } catch (err) {
    fastify.log.error(`Audit log retention cleanup failed: ${err.message}`);
  }
}

// ─── START ───
async function start() {
  try {
    await fastify.listen({port:PORT,host:'0.0.0.0'});
    fastify.log.info(`AgentRaaS API running on port ${PORT}`);
    fastify.log.info(`Loaded ${Object.keys(SERVICE_ROUTES).length} routes from config`);
    setTimeout(cleanupAuditLogRetention, 60000); // once, a minute after boot
    setInterval(cleanupAuditLogRetention, 6 * 60 * 60 * 1000); // then every 6h
    setTimeout(runHealthChecks, 30000); // once, 30s after boot (give proxy/pg time to settle)
    setInterval(runHealthChecks, HEALTH_CHECK_INTERVAL_MS);
  } catch (err) { fastify.log.error(err); process.exit(1); }
}
start();
