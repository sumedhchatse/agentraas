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

## Library mode: no server needed

Wrap any function that has a side effect so retries and concurrent
duplicates run it **exactly once**, with state in your own Redis.

```ts
import { exactlyOnce, RedisStore } from "agentraas";
import { createClient } from "redis";

const redis = await createClient().connect();
const charge = exactlyOnce(
  async (customer: string, amount: number) =>
    (await stripe.charges.create({ customer, amount })).id,
  { store: new RedisStore(redis), name: "charge", ttlSeconds: 86400 },
);

await charge("cus_1", 4200); // runs for real
await charge("cus_1", 4200); // retry: cached id, no second charge
```

- Throwing releases the slot, so a retry runs the function again. That
  includes the case where the provider did the work but the response was
  lost, so pass `passKey: true` and send the key you receive to the
  provider; it stays the same on every retry of the same call:

  ```ts
  const refund = exactlyOnce(
    async (idempotencyKey: string, charge: string) =>
      (await stripe.refunds.create({ charge }, { idempotencyKey })).id,
    { store: new RedisStore(redis), name: "refund", passKey: true },
  );
  await refund("ch_1"); // callers never pass the key
  ```
- Pass `key: (...args) => string` to choose what counts as a duplicate.
- `MemoryStore` works for tests and single-process scripts; for anything
  else, use `RedisStore` (node-redis v4+) or implement `DedupStore`.
- Results must be JSON-serializable.

## Vercel AI SDK: exactly-once tool calls

Wrap the `tools` object you pass to `generateText` or `streamText`:

```ts
import { protectTools, RedisStore } from "agentraas";

const tools = protectTools({ charge, sendEmail, search }, { store: new RedisStore(redis) });
const result = await generateText({ model, tools, prompt });
```

When the model calls a tool again with the same input, or a request is
retried, the first result comes back instead of a second charge. The key is
the tool name plus its input (`toolCallId` and the other per-call options
are passed through, not keyed); pass `key: (toolName, input) => ...` to
choose your own. Tools without `execute` are left as they are.

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

To decide yourself what counts as "the same action", pass your own key
instead of relying on the payload hash:

```typescript
await client.call("stripe", "charge.create", { amount: 5000, currency: "usd" },
  { idempotencyKey: `order-${orderId}` });
```

A new key makes it a new action, for example to resend after a 504
"outcome unknown" once you're sure the first attempt didn't run.

## Record and replay

Run an agent once against the real providers, then replay it (in a test, in
CI, after a prompt change) without calling them again:

```typescript
const recorder = new Client({ agentraasKey, orgId, agentId, record: "refund-flow" });
// ... run the agent with recorder ...

const replayer = new Client({ agentraasKey, orgId, agentId, replay: "refund-flow", runId: "ci-42" });
// each call is answered from the recording, in order; nothing reaches the provider
```

Each replayed answer carries `replay: { recording, position, payload_matches }`;
a call the recording doesn't have throws a 404 `AgentRaaSError`. A new
`runId` starts from the top. Details: [agentraas.io/docs#record-replay](https://agentraas.io/docs#record-replay).

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

MIT — see [LICENSE](./LICENSE). (The AgentRaaS server is AGPL-3.0, its
Team/Enterprise features FSL, all free to self-host; see the
[main repo](https://github.com/sumedhchatse/agentraas).)
