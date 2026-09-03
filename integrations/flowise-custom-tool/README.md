# AgentRaaS Custom Tool for Flowise

[Flowise](https://flowiseai.com) doesn't install community nodes via npm for
Custom Tools — you paste the schema and function directly into the UI. This
is that tool, ready to paste.

**Verification note:** written to Flowise's documented Custom Tool
conventions (as of Flowise 3.x), but not load-tested against a running
Flowise instance in this repo — please open an issue if something doesn't
match your version.

## Setup

1. In Flowise, go to **Tools → Add Custom Tool**.
2. **Tool Name:** `agentraas`
3. **Tool Description:** `Exactly-once execution for any API call via AgentRaaS — use this instead of a raw HTTP call whenever the action has a real-world side effect (charges, bookings, messages) that must never happen twice.`
4. **Input Schema:** paste the contents of [`schema.json`](./schema.json).
5. **JavaScript Function:** paste the contents of [`function.js`](./function.js).
6. Go to **Settings → Variables** and add:
   - `agentraasBaseUrl` — your AgentRaaS deployment URL (e.g. `http://localhost:13000`)
   - `agentraasKey` — your agent's API key (dashboard's **+ Connect Agent** panel)
   - `agentraasOrgId`, `agentraasAgentId` — recommended, same identity you connected the agent under
7. Attach the tool to any Agent/Chain node that should be able to take real-world actions.

## Why wrap tool calls in AgentRaaS

An LLM agent re-invoking a tool after a timeout, a retried chain step, or a
user re-running a flow all carry the same risk as a network retry: the
first call might have already succeeded upstream. AgentRaaS claims an
atomic dedup slot before forwarding anything, so a repeated call is safe —
it either runs once or returns the cached result from the run that
actually went through.

## License

MIT — same terms as the [Python SDK](../../src/sdk/README.md).
