# agentraas (TypeScript / JavaScript SDK)

Exactly-once execution for AI agents — a thin, dependency-free wrapper
around [AgentRaaS](https://github.com/sumedhchatse/agentraas)'s SDK-style
REST gateway. If your agent code calls Stripe, Twilio, HubSpot, or any
other API directly (from Node, a serverless function, or a browser),
wrap it with this client so a retry — yours, your framework's, or a
flaky network — never becomes a duplicate charge, a duplicate contact,
or a duplicate message.

Works against any AgentRaaS deployment: self-hosted (`./install.sh`,
free and unlimited on every tier) or AgentRaaS Cloud. Uses the global
`fetch`/`AbortController` (Node 18+, or any modern browser) — no
runtime dependencies.

## Install

```bash
npm install agentraas
```

## Quickstart

1. Connect an agent from your AgentRaaS dashboard (**+ Connect Agent**) —
   this gives you an `agentraasKey`, plus your `orgId` and `agentId`.
2. Add credentials for the service you're calling (**Credentials** panel)
   — AgentRaaS forwards to the real API using those, you never pass a
   raw upstream API key through this SDK.

```typescript
import { Client } from "agentraas";

const client = new Client({
  agentraasKey: "ar_live_...",
  orgId: "acme-corp",
  agentId: "billing-bot",
  baseUrl: "http://localhost:13000", // or your Cloud/self-hosted URL
});

const result = await client.call("stripe", "charge.create", { amount: 5000, currency: "usd" });
```

Or with the service-scoped shorthand:

```typescript
const stripe = client.service("stripe");
const result = await stripe.call("charge.create", { amount: 5000, currency: "usd" });
```

Calling a [Custom Action](https://github.com/sumedhchatse/agentraas#supported-services)
you've registered:

```typescript
const result = await client.custom("my-internal-webhook", { foo: "bar" });
```

## Why `orgId` / `agentId` matter

Omit them and every untagged SDK caller shares one unenforced identity
server-side — your per-agent rate limit and audit trail won't tell your
traffic apart from anyone else's. Set them once you've connected an
agent from the dashboard; it's two extra constructor fields.

## Retries are safe

AgentRaaS claims an atomic dedup slot server-side *before* forwarding
anything. If `client.call(...)` rejects because of a timeout or a
dropped connection, calling it again with the same payload is safe — it
either completes normally or returns the cached result from the call
that actually went through. This SDK doesn't retry automatically; your
own retry logic (or your agent framework's) can be as aggressive as you
want.

## Error handling

```typescript
import { AgentRaaSError } from "agentraas";

try {
  await client.call("stripe", "charge.create", { amount: 5000, currency: "usd" });
} catch (err) {
  if (err instanceof AgentRaaSError) {
    console.log(err.statusCode, err.reqId, err.message);
  }
}
```

## License

MIT — see [LICENSE](./LICENSE). (The AgentRaaS server itself is
open-core; see the [main repo](https://github.com/sumedhchatse/agentraas)
for its licensing.)
