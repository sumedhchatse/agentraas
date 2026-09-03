# Getting started with AgentRaaS

This guide walks through connecting your first agent — on AgentRaaS Cloud,
or on your own self-hosted instance — and where to find everything again
later.

## The short version

1. Register and log in.
2. Click **+ Connect agent**, give it a name.
3. You get three things, shown **once**: an API key, a webhook URL, and an
   MCP URL. Save the API key somewhere safe — it's not shown again.
4. Point your agent at the webhook URL (or MCP URL, if your agent framework
   speaks MCP), with the API key in the `Authorization` header.
5. Done — every call your agent makes now goes through AgentRaaS first.

The rest of this guide explains each piece in more detail.

---

## Cloud vs. self-hosted — which do I want?

| | AgentRaaS Cloud | Self-hosted |
|---|---|---|
| Where it runs | Our servers | Your own server |
| Setup | Register, log in, done | Register, download, run `install.sh` |
| Monthly limit | 500 actions on the free tier, enforced (Agency/Enterprise raise it) | None — unlimited, on any tier |
| Good for | Trying it out, no infra to manage | Your own data residency, unlimited volume |

You can do both — try it on Cloud first, and self-host later if you want
your own instance. Nothing about your Cloud data moves to self-host
automatically; they're separate.

---

## Cloud: step by step

1. **Register** at the dashboard and verify your email (check your inbox —
   or, if you don't receive anything, check whether your deployment has
   email configured; a verification link is always available as a
   fallback in that case).
2. **Log in.**
3. Click **+ Connect agent** in the top toolbar.
4. Fill in:
   - **Org ID** — a short identifier for whoever owns this agent (a
     team, a client, your company). Letters, numbers, underscores, and
     hyphens only. If you're not sure what to put, use something like
     `myteam` or `acme_corp` — you can create more later if you need to
     separate different customers or environments.
   - **Agent ID** — a short identifier for this specific agent, e.g.
     `invoice_bot` or `support_agent`. Same character rules as Org ID.
   - **Label** (optional) — a human-friendly name shown in your agent list,
     e.g. "Production invoice bot".
5. Click **Connect**. You'll see:
   - **API key** — starts with `ar_live_`. **Copy this now** — it is
     shown exactly once and cannot be retrieved again. If you lose it,
     connect a new agent (you can revoke the old one from the Credentials
     panel).
   - **Webhook URL** — where your agent sends requests.
   - **MCP URL** — for agent frameworks that speak MCP instead of plain
     webhooks.

That's it — your agent is connected. Every action it takes through
AgentRaaS will show up on your dashboard within a few seconds.

---

## Self-hosted: step by step

Self-hosting has two logins to keep straight: your AgentRaaS Cloud account
(used only to get the download), and your self-hosted instance's own
account (separate, lives on your own server).

1. **On AgentRaaS Cloud:** register, log in, and connect at least one
   agent first (see above) — the self-host download only unlocks once
   you've actually tried the product.
2. Open your **Account** menu (click your email, top right) → **Self-host
   AgentRaaS**. Fill in the short form (what you'll use it for, and
   optionally your company/team name), then download the `.zip`.
3. On your own server, unzip it and run:
   ```bash
   ./install.sh
   ```
   This generates fresh secrets, runs the database migrations, and starts
   the stack.
4. Once it's running, go to `http://your-server:13000/dashboard` (or
   whatever host/port you configured). Your account is already pre-seeded
   with the email you used on Cloud — click **Forgot password** and enter
   that same email to set a password for *this* instance (see
   `SETUP_INSTRUCTIONS.txt` in the download for the exact link).
5. Log in, then **+ Connect agent**, exactly as in the Cloud steps above —
   this generates a fresh API key/webhook URL/MCP URL specific to your
   self-hosted instance. Cloud and self-hosted API keys are never
   interchangeable.

---

## Where do I find these values again later?

- **API key** — nowhere. It's shown once, by design (AgentRaaS never
  stores it in a form we could show you again — only a hash of it). If
  you've lost it, connect a new agent and revoke the old one.
- **Org ID / Agent ID / Webhook URL** — always visible in the
  **Credentials** panel, under your list of connected agents.
- **MCP URL** — the same for every agent on a given deployment:
  `<your-instance-url>/mcp`.

---

## Understanding the two keys

These are two separate things, easy to mix up at first:

| | Agent Key | Service Credential |
|---|---|---|
| What it is | The key from **+ Connect agent** (`ar_live_...`) | Your own real key for a service (e.g. Stripe's `sk_live_...`), added via the **Credentials** panel |
| What it's for | Proves your agent is allowed to talk to AgentRaaS | Lets AgentRaaS make the real call to that service on your behalf |
| Who sees it | Your agent — puts it in the `Authorization` header | Only AgentRaaS's server — your agent never sees this at all |

The flow: **your agent -> (Agent Key) -> AgentRaaS -> (Service Credential) -> the real API**. There's no separate "incoming" vs "outgoing" agent key — just these two, each handling one leg of the trip.

## Making your first call

Once connected, your agent calls AgentRaaS instead of the real API
directly. A plain webhook call looks like:

```bash
curl -X POST https://your-instance/v1/webhook/<org_id>/<agent_id> \
  -H "Authorization: Bearer <your-agent-key>" \
  -H "Content-Type: application/json" \
  -d '{"service":"mockpay","action":"charge.create","payload":{"amount":500,"currency":"usd","customer":"cus_123"}}'
```

This example uses `mockpay` — a built-in fake service made for testing,
so it works immediately with no setup. To call a real service like
Stripe instead, change `"service"` to `"stripe"` — but that only works
once you've saved your real Stripe key in the **Credentials** panel
first (see below); without it, the call will fail since there's no
credential to use.

- `service` — which connector to use (`stripe`, `twilio`, `slack`, etc. —
  see the full list on the dashboard's Credentials panel).
- `action` — which operation on that service (each connector supports a
  few — check the Credentials panel's dropdown for the exact list).
- `payload` — whatever that action needs, matching the real API's own
  fields.

AgentRaaS handles the actual call to Stripe/Twilio/whichever service,
and guarantees that an identical retry within the dedup window returns
the cached result instead of doing it twice.

---

## Adding your own service credentials

Before your agent can actually call a service like Stripe or Twilio, you
need to give AgentRaaS your API key for that service:

1. Open the **Credentials** panel.
2. Pick the service and enter your key (or username/password, for
   services that use that instead).
3. Save. It's encrypted at rest — nobody, including AgentRaaS staff, can
   read it back out; it's only ever used server-side to make the call on
   your agent's behalf.

## Something not on the list?

If the service you need isn't one of the built-in connectors, use
**Custom Actions** to register any endpoint yourself — same dedup
protection, same SSRF guarding, same audit trail. Open the **Custom
actions** panel and follow the form; it walks you through the same
service/action/payload shape as the built-in connectors.
