# agentraas (Python SDK)

<!-- mcp-name: io.github.sumedhchatse/agentraas -->

Exactly-once execution for AI agents — a thin, dependency-light wrapper
around [AgentRaaS](https://github.com/sumedhchatse/agentraas)'s SDK-style
REST gateway. If your agent code calls Stripe, Twilio, HubSpot, or any
other API directly, wrap it with this client so a retry — yours, your
framework's, or a flaky network — never becomes a duplicate charge, a
duplicate contact, or a duplicate message.

Works against any AgentRaaS deployment: self-hosted (`./install.sh`,
free and unlimited on every tier) or AgentRaaS Cloud.

## Install

```bash
pip install agentraas
```

## Library mode: no server needed

Wrap any function that has a side effect (charging a card, sending an
email, creating a CRM record) so retries and concurrent duplicates run it
**exactly once**. Nothing to deploy: state lives in a local SQLite file by
default, or in your own Redis.

```python
from agentraas.local import exactly_once, RedisStore

@exactly_once()                                   # SQLite file, one machine
def charge(customer, amount):
    return stripe.Charge.create(customer=customer, amount=amount).id

charge("cus_1", 4200)   # runs for real
charge("cus_1", 4200)   # retry: returns the cached id, no second charge

# Across machines, with your own idempotency key and window:
@exactly_once(store=RedisStore("redis://localhost:6379/0"),
              key=lambda order: order["id"], ttl=3600)
async def ship(order):
    ...
```

- If the function raises, nothing is cached and a retry runs it again. That
  includes the case where the provider did the work but the response was
  lost, so give the function an `idempotency_key` parameter and send it to
  the provider (e.g. Stripe's `Idempotency-Key` header). The decorator
  fills it with the same value on every retry:

  ```python
  @exactly_once()
  def charge(customer, amount, idempotency_key=None):
      return stripe.Charge.create(customer=customer, amount=amount,
                                  idempotency_key=idempotency_key).id
  ```
- A duplicate that arrives while the first call is still running waits for
  its result (up to `wait=30` seconds, then `InFlightError`).
- Results must be JSON-serializable (return ids or dicts, not SDK objects).
- `RedisStore` needs `pip install redis`.

When you want a dashboard, audit trail, human approval or circuit
breaking on top, point the same calls at an AgentRaaS server with the
`Client` below.

## MCP wrap: exactly-once tool calls for any MCP server

Put `agentraas wrap --` in front of an MCP server's command. Everything
passes through unchanged, except that an identical call to a **write**
tool (same tool, same arguments) inside the dedup window gets the first
call's result back instead of running again. An agent that retries or
loops can't send the same email or open the same issue twice. Every tool
call is appended to an audit log.

Claude Desktop / Cursor config:

```json
{
  "mcpServers": {
    "github": {
      "command": "uvx",
      "args": ["agentraas", "wrap", "--", "npx", "-y", "@modelcontextprotocol/server-github"]
    }
  }
}
```

- Read-only tools are never deduplicated, so lookups stay fresh. The
  server's `readOnlyHint` annotation decides, falling back to names like
  `get_*`, `list_*`, `search_*`. Override with `--dedupe TOOL` /
  `--no-dedupe TOOL`.
- A failed call (JSON-RPC error or `isError: true`) is not cached, so a
  retry runs for real.
- Default window: 10 minutes (`--ttl 600`). State lives in
  `~/.agentraas/mcp-dedup.db`, the log in `~/.agentraas/mcp-audit.jsonl`
  (`--db`, `--log`).
- A deduplicated result carries `_meta: {"agentraas/deduplicated": true}`.

## Chaos tester: find the calls your agent would run twice

`agentraas-chaos` sits between your code and an API. The first time each
distinct write request (POST/PUT/PATCH/DELETE) arrives, it lets the
request execute and then drops the response, the network failure that
turns a retry into a double charge. Then it reports every action that
executed more than once.

```bash
pip install agentraas

# Safe: a built-in mock answers every call, nothing real is touched
agentraas-chaos --mock -- python my_agent.py

# Against a real API: use TEST-mode keys
agentraas-chaos --upstream https://api.stripe.com -- pytest
```

Your command gets `AGENTRAAS_CHAOS_URL` (e.g. `http://127.0.0.1:8787`);
use it as the API base URL. Without a command it runs until Ctrl-C, so
you can point an n8n workflow or any other tool at it by hand.

```
agentraas-chaos: 6 write requests, 3 responses dropped after execution

  DUPLICATES: 1 action(s) executed more than once

    2x  POST /v1/charges  (body sha256 623f14d1b00d)
  ...
  Protected by an idempotency key: 2 retries
```

It exits with code 1 when it finds a duplicate, so it can fail CI.
`--fault error-after-commit` returns a 502 instead of dropping the
connection. A retry that repeats an `Idempotency-Key` it has already seen
is counted as protected, not executed again.

In GitHub Actions:

```yaml
- uses: actions/setup-python@v5
  with: { python-version: "3.12" }
- uses: sumedhchatse/agentraas/src/chaos-action@main
  with:
    run: python my_agent.py
```

## Quickstart

1. Connect an agent from your AgentRaaS dashboard (**+ Connect Agent**) —
   this gives you an `agentraas_key`, plus your `org_id` and `agent_id`.
2. Add credentials for the service you're calling (**Credentials** panel)
   — AgentRaaS forwards to the real API using those, you never pass a
   raw upstream API key through this SDK.

```python
import agentraas

client = agentraas.Client(
    agentraas_key="ar_live_...",
    org_id="acme-corp",
    agent_id="billing-bot",
    base_url="http://localhost:13000",  # or your Cloud/self-hosted URL
)

result = client.call("stripe", "charge.create", {"amount": 5000, "currency": "usd"})
```

Or with dot-notation sugar:

```python
stripe = client.service("stripe")
result = stripe.charge.create({"amount": 5000, "currency": "usd"})
```

Calling a [Custom Action](https://github.com/sumedhchatse/agentraas#supported-services)
you've registered:

```python
result = client.custom("my-internal-webhook", {"foo": "bar"})
```

## Why `org_id` / `agent_id` matter

Omit them and every untagged SDK caller shares one unenforced identity
server-side — your per-agent rate limit and audit trail won't tell your
traffic apart from anyone else's. Set them once you've connected an
agent from the dashboard; it takes two extra kwargs.

## Retries are safe

AgentRaaS claims an atomic dedup slot server-side *before* forwarding
anything. If `client.call(...)` raises because of a timeout or a dropped
connection, calling it again with the same payload is safe — it either
completes normally or returns the cached result from the call that
actually went through. This SDK doesn't retry automatically; your own
retry logic (or your agent framework's) can be as aggressive as you want.

## Error handling

```python
from agentraas import AgentRaaSError

try:
    client.call("stripe", "charge.create", {"amount": 5000, "currency": "usd"})
except AgentRaaSError as err:
    print(err.status_code, err.req_id, str(err))
```

## License

MIT — see [LICENSE](./LICENSE). (The AgentRaaS server itself is
open-core; see the [main repo](https://github.com/sumedhchatse/agentraas)
for its licensing.)
