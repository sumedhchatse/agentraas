# AgentRaaS 🛡️

**The exactly-once execution layer for AI agents.**

Prevent your agents from double-charging customers, double-booking appointments, or sending duplicate emails. AgentRaaS sits between your agent and any API — connect it via webhook, SDK-style headers, or native MCP.

---

## The Problem

Your n8n workflow hits Stripe. It times out. n8n retries.

**Result:** The customer is charged twice.

Your agent calls HubSpot to create a contact. The request succeeds, but the response is lost. The agent retries.

**Result:** Two duplicate contacts in your CRM.

---

## The Solution

AgentRaaS is a proxy that guarantees **exactly-once execution** of agent actions — proven under real concurrent load with an automated test suite (`npm test`).

<svg width="100%" viewBox="0 0 680 220" xmlns="http://www.w3.org/2000/svg" role="img">
<title>How AgentRaaS sits between your agent and the real API</title>
<desc>Agent sends a request to AgentRaaS, which deduplicates, validates, circuit-breaks, rate-limits, and audit-logs it before forwarding to the real API.</desc>
<rect width="680" height="220" fill="#0A0D14"/>
<rect x="40" y="60" width="140" height="100" rx="10" fill="#0F1420" stroke="#232A3A" stroke-width="1"/>
<text x="110" y="95" text-anchor="middle" font-family="sans-serif" font-size="14" font-weight="600" fill="#F5F6F9">Agent</text>
<text x="110" y="118" text-anchor="middle" font-family="sans-serif" font-size="12" fill="#8B93A6">(n8n, Python,</text>
<text x="110" y="134" text-anchor="middle" font-family="sans-serif" font-size="12" fill="#8B93A6">MCP)</text>
<line x1="182" y1="110" x2="246" y2="110" stroke="#00E0A8" stroke-width="1.5"/>
<polygon points="246,105 254,110 246,115" fill="#00E0A8"/>
<rect x="250" y="30" width="220" height="160" rx="10" fill="#0F1420" stroke="#00E0A8" stroke-width="1"/>
<text x="360" y="55" text-anchor="middle" font-family="sans-serif" font-size="14" font-weight="600" fill="#F5F6F9">AgentRaaS (Proxy)</text>
<text x="272" y="80" font-family="sans-serif" font-size="12" fill="#8B93A6">- Deduplicate</text>
<text x="272" y="102" font-family="sans-serif" font-size="12" fill="#8B93A6">- Validate</text>
<text x="272" y="124" font-family="sans-serif" font-size="12" fill="#8B93A6">- Circuit breaker</text>
<text x="272" y="146" font-family="sans-serif" font-size="12" fill="#8B93A6">- Rate limit</text>
<text x="272" y="168" font-family="sans-serif" font-size="12" fill="#8B93A6">- Audit log</text>
<line x1="472" y1="110" x2="536" y2="110" stroke="#00E0A8" stroke-width="1.5"/>
<polygon points="536,105 544,110 536,115" fill="#00E0A8"/>
<rect x="540" y="60" width="100" height="100" rx="10" fill="#0F1420" stroke="#232A3A" stroke-width="1"/>
<text x="590" y="95" text-anchor="middle" font-family="sans-serif" font-size="14" font-weight="600" fill="#F5F6F9">API</text>
<text x="590" y="118" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">Stripe, Twilio,</text>
<text x="590" y="134" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">HubSpot, or any</text>
</svg>

**How it works:**
1. Your agent sends a request to AgentRaaS instead of the API directly
2. AgentRaaS atomically claims a dedup slot in Redis for that exact request
3. **First call:** forwarded to the real API, result cached
4. **A concurrent or later retry:** returns the cached result, or a 409 if the original is still in flight — never a second real execution

---

## Dashboard

Open `http://localhost:13000/dashboard` — requires an account (register/login, encrypted password storage). New accounts get their own org automatically — nothing technical required just to see a working dashboard.

- **+ Connect Agent** — generates a real API key scoped to one org/agent, with curl and n8n examples
- **Credentials** — add your own Stripe/Twilio/etc. keys yourself, encrypted at rest, no server access needed
- **Custom Actions** — register any endpoint (not just the curated list below), SSRF-guarded, so your agents can call it too
- Total actions, success/deduplicated/blocked/error breakdown, request volume and outcome charts, over 24h/7d/30d/90d — scoped to your own data only, never other users'
- Active agents, service health (circuit breaker state), searchable/filterable/sortable recent activity log
- Auto-refreshing, CSV export, account settings (change password)
- A full step-by-step walkthrough lives at [`/guide`](./GETTING_STARTED.md) — worth reading if any of the above is unfamiliar

---

## Getting started

AgentRaaS isn't distributed via a public repo clone — register for an
account at **agentraas.io** (or wherever this deployment lives), connect
your first agent, then download your self-host package from the
dashboard's Account menu (this unlocks once you've connected an agent —
a short form asking what you'll use it for is part of that flow). That
package includes everything, including this README.

Once you have it:

```bash
unzip agentraas-self-host.zip
cd agentraas
./install.sh
```

`install.sh` handles everything that used to be a manual multi-step
process: generating `JWT_SECRET` and `CREDENTIALS_ENCRYPTION_KEY`, starting
the stack, running every migration in order, installing dependencies
*inside* the container (native modules need to build for the container's
own OS, not your host's — this bit us hard during development, see below),
handling SELinux relabeling if applicable, and a clean recreate at the end.

Then open `http://localhost:13000/dashboard` — your account (the same
email you used to download the package) is already pre-registered; use
"Forgot password" to set a password for this instance.

**A real gotcha worth knowing, if you ever touch the setup manually:** on
SELinux (Fedora/RHEL-family hosts), bind-mounted files can end up with the
wrong context and the container fails with `EACCES` errors. `install.sh`
handles this automatically; if you're troubleshooting by hand, fix it with:
```bash
sudo semanage fcontext -a -t container_file_t "$(pwd)(/.*)?"
sudo restorecon -Rv "$(pwd)"
```
and from then on, always use `podman-compose down && up -d` (full recreate)
after changing files on the host — never plain `restart`, which doesn't
re-apply the SELinux label.

**Services:**
- API Gateway + dashboard: `http://localhost:13000`
- Postgres: `localhost:15432`
- Redis: `localhost:16379`

---

## Testing

```bash
podman exec -it ar-api npm test
```

Runs real integration tests against the running server — concurrent duplicate
requests, sequential replay, distinct-payload isolation, and failure/retry
recovery — not mocked unit tests. Uses the built-in `mockpay` service, so no
real API keys are needed to run them.

---

## MCP (Model Context Protocol)

AgentRaaS exposes an MCP gateway for Claude Desktop, Cursor, and other MCP clients:

```json
{
  "mcpServers": {
    "agentraas": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://localhost:13000/mcp"]
    }
  }
}
```

All tool calls through this gateway are deduplicated, validated, rate-limited, and logged.

---

## Supported services

Curated, pre-configured integrations (no code required — see `config/services.json`):

Stripe, Twilio, HubSpot, Calendly, Shopify, Zoho, Razorpay, WhatsApp,
Zapier, Make, Adyen, Mollie, Airwallex, Xendit, PayPal, Salesforce, Slack,
Klarna, Paystack, GoCardless, Opn Payments, plus the built-in `mockpay`
for safe testing.

**Need something not on this list?** Register it as a **Custom Action** from
the dashboard — any URL, any auth type, protected by the same dedup/audit/
rate-limit pipeline, with an SSRF guard so it can't be pointed at internal
infrastructure.

To add a new *curated* integration permanently, add an entry to
`config/services.json` following the existing pattern — no code changes needed.

---

## Architecture

<svg width="100%" viewBox="0 0 680 260" xmlns="http://www.w3.org/2000/svg" role="img">
<title>AgentRaaS stack architecture</title>
<desc>API Gateway on port 13000, Redis for dedup on 16379, PostgreSQL for audit/users/credentials on 15432. Requests arrive via Webhook, SDK-style headers, or MCP, routed through the MCP Gateway.</desc>
<rect width="680" height="260" fill="#0A0D14"/>
<text x="40" y="24" font-family="sans-serif" font-size="13" font-weight="600" fill="#8B93A6">AGENTRAAS STACK</text>
<rect x="40" y="40" width="150" height="90" rx="10" fill="#0F1420" stroke="#00E0A8" stroke-width="1"/>
<text x="115" y="70" text-anchor="middle" font-family="sans-serif" font-size="13" font-weight="600" fill="#F5F6F9">API Gateway</text>
<text x="115" y="92" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">:13000</text>
<rect x="220" y="40" width="130" height="90" rx="10" fill="#0F1420" stroke="#232A3A" stroke-width="1"/>
<text x="285" y="70" text-anchor="middle" font-family="sans-serif" font-size="13" font-weight="600" fill="#F5F6F9">Redis</text>
<text x="285" y="92" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">(Dedup)</text>
<text x="285" y="108" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">:16379</text>
<rect x="380" y="40" width="190" height="90" rx="10" fill="#0F1420" stroke="#232A3A" stroke-width="1"/>
<text x="475" y="70" text-anchor="middle" font-family="sans-serif" font-size="13" font-weight="600" fill="#F5F6F9">PostgreSQL</text>
<text x="475" y="92" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">(Audit, users, creds)</text>
<text x="475" y="108" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">:15432</text>
<line x1="115" y1="132" x2="115" y2="158" stroke="#00E0A8" stroke-width="1.5"/>
<polygon points="110,156 115,164 120,156" fill="#00E0A8"/>
<rect x="40" y="160" width="220" height="80" rx="10" fill="#0F1420" stroke="#232A3A" stroke-width="1"/>
<text x="150" y="192" text-anchor="middle" font-family="sans-serif" font-size="13" font-weight="600" fill="#F5F6F9">Webhook / SDK / MCP</text>
<text x="150" y="214" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">Incoming agent requests</text>
<rect x="300" y="160" width="270" height="80" rx="10" fill="#0F1420" stroke="#00E0A8" stroke-width="1"/>
<text x="435" y="192" text-anchor="middle" font-family="sans-serif" font-size="13" font-weight="600" fill="#F5F6F9">MCP Gateway</text>
<text x="435" y="214" text-anchor="middle" font-family="sans-serif" font-size="11" fill="#8B93A6">/v1/sdk/:service/:action, /mcp</text>
</svg>

---

## Why AgentRaaS vs. DIY idempotency keys?

| | DIY Idempotency Keys | AgentRaaS |
|---|---|---|
| **Code changes** | Modify every API call | Change the URL |
| **No-code support** | ❌ Impossible | ✅ Paste webhook URL (n8n, Make, Zapier) |
| **Multiple services** | Different logic per API | One proxy, all services — plus any custom endpoint |
| **Credential management** | Build yourself | Self-serve, encrypted at rest |
| **Validation, circuit breaker, rate limiting** | Build yourself | Built-in |
| **Audit trail, dashboard** | Build yourself | Included |

---

## Pricing

Open-core, three tiers. Community is self-hosted only, with a limited
feature set — it's the free on-ramp. Agency and Enterprise both run
either cloud-hosted (on AgentRaaS Cloud) or self-hosted, and unlock
everything above Community. `src/ee/*` (SSO, RBAC, HMAC, DLP, HA, SIEM
export) is source-available under a separate commercial license — see
[LICENSE.md](./LICENSE.md).

| | Community | Agency | Enterprise |
|---|---|---|---|
| **Price** | $0/mo | $149/mo | Custom, from $499/mo |
| **Deployment** | Self-hosted only | Cloud-hosted or self-hosted | Cloud-hosted or self-hosted/on-prem |
| **Actions/month, self-hosted** | Unlimited | Unlimited | Unlimited |
| **Actions/month, cloud-hosted** | n/a (not offered) | 50,000 | Unlimited |
| **Payload dedup, MCP gateway, dashboard** | ✅ | ✅ | ✅ |
| **Audit log retention** | Local Postgres (30d) | 1 year | SOC2-ready + SIEM export |
| **Client tenants / white-label** | — | ✅ up to 10 | ✅ unlimited |
| **Outbound rate smoothing (token bucket)** | Static cap only | ✅ | ✅ |
| **Inbound HMAC verification, PII/DLP redaction** | — | — | ✅ |
| **SSO (OIDC/SAML), RBAC** | — | — | ✅ |
| **HA clustering** | — | — | ✅ |
| **Support** | GitHub & Discord | Priority email | 24/7 SLA |

Community also has a free-to-try flavor on AgentRaaS Cloud (no install,
capped at 500 actions/month — the only tier/deployment combination with
any cap at all; self-hosting removes it entirely, on any tier). Get
started or self-host from `/dashboard`, or contact
**support@agentraas.io** for Agency/Enterprise sales.

---

## Roadmap

- [x] Exactly-once proxy engine — automated-test-verified under real concurrency
- [x] Validation rules, circuit breaker, rate limiting
- [x] Audit logging + real-time dashboard
- [x] MCP gateway
- [x] Self-serve encrypted credentials — real forwarding to Stripe/Twilio/etc. with your own keys
- [x] Custom Actions — call any endpoint, not just curated services
- [x] Dashboard auth (register/login, session management, change password)
- [x] Hosted AgentRaaS Cloud offering
- [x] Enterprise SSO (OIDC) + per-org RBAC (`src/ee/auth`)
- [x] Inbound webhook HMAC verification — 10+ providers (`src/ee/hmac`)
- [x] PII/DLP redaction engine (`src/ee/dlp`)
- [x] Distributed token-bucket rate limiter (`src/ee/rate_limiter`)
- [x] Tamper-evident audit logs + SIEM export
- [x] Agency tier — multi-tenant, white-label dashboard branding
- [ ] Official Python SDK package
- [ ] Custom validation rule builder (UI)
- [ ] n8n/Flowise/Langflow native node packaging

---

## License

AgentRaaS uses a custom **fair-code / source-available license** — see
[LICENSE.md](./LICENSE.md) for full terms. In short:

- **Self-hosted:** free, unlimited forwarded actions/month, on any tier
- **On an AgentRaaS-hosted deployment:** free for up to 500/month
- To offer AgentRaaS as a competing hosted service, or for a Cloud plan
  beyond the free tier, contact **support@agentraas.io**

This is not an OSI-approved open-source license — it's modeled on n8n's
Sustainable Use License.

Everything in this repo (`src/core`, `src/api-gateway`, `src/sdk`) is
MIT/Apache-2.0 — genuinely open. Enterprise features (SSO, RBAC, HMAC
verification, DLP, distributed rate limiting, HA) live in a separate
`ee/` module under a source-available commercial license, required for
production use beyond a trial — see the [Pricing](#pricing) section
above or contact **support@agentraas.io**.

---

## Contributing & Security

See [CONTRIBUTING.md](./CONTRIBUTING.md) for the contribution process and
[CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md) for community expectations.

**Found a security vulnerability?** Do not open a public issue — see
[SECURITY.md](./SECURITY.md) for the private disclosure process.

---

**Built with:** Fastify, Redis, PostgreSQL, Podman, and the fear of double-charging a customer at 2 AM.
