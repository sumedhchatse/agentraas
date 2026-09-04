# Community templates

Ready-to-use starting points for wiring AgentRaaS into the low-code / agent
tools it's built to sit alongside.

| Tool | What's here | Install |
|---|---|---|
| **Python** | [`src/sdk`](../../src/sdk) — `Client`, tested end-to-end against a live instance | `pip install agentraas` (once published — see its README) |
| **TypeScript/JS** | [`src/sdk-js`](../../src/sdk-js) — `Client`, dependency-free (global `fetch`), tested end-to-end | `npm install agentraas` (once published — see its README) |
| **n8n** | [`n8n-nodes-agentraas`](../n8n-nodes-agentraas) — a real community node (TypeScript, compiles against `n8n-workflow`) | `npm install n8n-nodes-agentraas` in your n8n instance, or **Settings → Community Nodes** in the UI |
| **Flowise** | [`flowise-custom-tool`](../flowise-custom-tool) — schema + function to paste into a Custom Tool | Copy-paste, no install (see its README) |
| **Langflow** | [`langflow-custom-component`](../langflow-custom-component) — a Python `Component` file | Drop into your Langflow custom components directory |
| **Dify** | No native plugin yet — Dify's HTTP Request node/tool works today: point it at `POST /v1/sdk/:service/:action` with the `X-AgentRaaS-Key` header, same as any other client. | n/a |
| **n8n workflow** | [`agentraas-stripe-retry-safe.json`](./agentraas-stripe-retry-safe.json) — importable example workflow | n8n → **Import from File** |
| **Docker Compose sidecar** | [`docker-compose-sidecar/`](./docker-compose-sidecar) — drop AgentRaaS into an existing Flowise/Langflow/Dify compose stack as one more service, plus per-tool wiring notes | Paste into your `docker-compose.yml` |

## Verification status

The **n8n node** compiles cleanly against the real, current `n8n-workflow`
package (verified in this repo's dev environment — this caught a real
breaking API change, `NodeConnectionType` → `NodeConnectionTypes`, between
older and current n8n versions) and loads without error via plain
`require()`. Full end-to-end registration in a live n8n instance (node
appearing in the palette, executing a real workflow) was not confirmed in
this repo's environment — n8n 2.x's custom-node loading didn't surface the
package the way documented, and we ran out of time chasing why. If you hit
an issue installing it, please open an issue with your n8n version.

The **Flowise** and **Langflow** integrations are written to each tool's
documented extension API but not load-tested against a running instance of
either — same ask if something's off.

## Why wrap these tools' API calls in AgentRaaS

Every one of these tools retries on failure by design — that's the right
default. But a retry after a timeout doesn't know whether the first
attempt's call actually landed upstream before the connection dropped.
AgentRaaS claims an atomic dedup slot before forwarding anything, so the
retry becomes provably safe instead of a guess.
