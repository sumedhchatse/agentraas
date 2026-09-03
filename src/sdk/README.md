# agentraas (Python SDK)

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
