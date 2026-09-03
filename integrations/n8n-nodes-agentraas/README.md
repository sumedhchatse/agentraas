# n8n-nodes-agentraas

An [n8n](https://n8n.io) community node for [AgentRaaS](https://github.com/sumedhchatse/agentraas) — wrap any node in your workflow's API calls with AgentRaaS's atomic exactly-once dedup guarantee, so an n8n retry (or manual re-run) never double-charges, double-books, or double-creates anything.

## Install

In n8n: **Settings → Community Nodes → Install**, package name `n8n-nodes-agentraas`.

Or self-hosted, in your n8n install:

```bash
npm install n8n-nodes-agentraas
```

## Setup

1. In AgentRaaS, connect an agent (dashboard's **+ Connect Agent**) — this gives you an agent API key, plus your org ID and agent ID.
2. In n8n, create an **AgentRaaS API** credential: paste the key, org ID, agent ID, and your AgentRaaS base URL (`http://localhost:13000` for a local self-hosted instance, or your Cloud/deployment URL).
3. Add the **AgentRaaS** node to your workflow. Set **Service** (a curated service name, or `custom` for a registered Custom Action) and **Action** (e.g. `charge.create`), and **Payload** (the request body).

## Why this matters for n8n specifically

n8n retries failed executions — by design, and that's usually right. But a retry after a timeout doesn't know whether the first attempt's HTTP call actually landed upstream before the connection dropped. Point the same call through AgentRaaS instead of the raw HTTP Request node, and a retry becomes provably safe: AgentRaaS claims an atomic dedup slot before forwarding anything, so the retry either completes normally or gets back the cached result from the call that actually went through.

## Development

```bash
npm install
npm run build
```

To test locally against a running n8n instance, link this package into n8n's custom nodes directory (`~/.n8n/custom` by default) and restart n8n — see n8n's [community node development docs](https://docs.n8n.io/integrations/creating-nodes/).

## License

MIT — see [LICENSE](../../src/sdk/LICENSE) (same terms as the Python SDK; the AgentRaaS server itself is open-core, see the [main repo](https://github.com/sumedhchatse/agentraas)).
