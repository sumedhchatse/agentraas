# SPEC-ACTION-POLICIES: what each agent is allowed to do

Status: implemented 2026-09-30. Private-repo-only, same handling as the
other `SPEC-*.md` files.

## 1. Problem

Agents now spend money and send mail on people's behalf (OpenAI Dots and
Meta Muse, 2026-09). The question a team asks before letting one loose is
"what is this agent allowed to do?", e.g. *the support agent may refund
up to 50, may never delete a customer, may only email our customers'
domains*. Today AgentRaaS has pieces of that but no answer to it:

| Exists | Gap |
|---|---|
| Validation rules: field limits per org + service + action | not per agent; can't forbid an action outright |
| Spend caps: call counts per agent | counts only, not what's in the call |
| HITL rules: numeric threshold → approval (Team+) | not per agent, numbers only |

## 2. Design

One new table, `action_policies` (migration 049), reusing the validation
rule field language so there is one way to describe a payload limit.

```
org_id, agent_id (NULL = every agent), service, action ('*' = any action
of that service), effect ('deny' | 'require'), fields (JSONB, validation
rule syntax, only for 'require'), on_violation ('block' | 'hitl')
```

- **deny**: the agent may not call this service.action at all.
- **require**: the payload must pass `fields` (min/max/enum/format/...,
  plus the new `domains` rule below), otherwise `on_violation`.
- **Every matching policy applies** (not most-specific-wins): policies are
  restrictions, so an org-wide rule and an agent rule both hold. Safer and
  simpler to reason about than overrides.
- New field rule **`domains: ["acme.com", ...]`** for strings: an email's
  domain or a URL's host must equal a listed domain or be a subdomain of
  it. Also works in plain validation rules, since it lives in the shared
  validator.
- **Enforcement point**: in `agent::handle_request`, straight after
  validation rules and before circuit breaker / usage / spend caps, so a
  refused call doesn't count against caps. Blocked → dedup slot released,
  audit row `blocked` with reason `policy_denied` or `policy_violation`,
  HTTP 403 naming the policy id and the failed field. `hitl` → the same
  `freeze_and_notify` path spend caps use (Team+ and the enterprise
  build, checked at creation time exactly like spend caps).
- **Tier**: every tier, including Community (it's a safety control, not a
  paid feature; see free-first in `conventions.md`). Only `hitl` needs Team.
- Applies to every source (webhook, SDK, MCP-through-gateway), since all
  go through `handle_request`.

## 3. API

`POST /api/v1/action-policies`, `GET /api/v1/action-policies`,
`DELETE /api/v1/action-policies/:id`. Same auth, auditor read-only check
and org scoping as `/api/v1/spend-cap-rules`.

## 4. Not in v1

- Policies in library mode / `protect_tool` (no server to hold them; users
  can write the check in their own function).
- Regex/pattern rules (ReDoS risk on a shared gateway; `domains` and
  `enum` cover the asked-for cases).
- Time-of-day rules, dollar amounts across calls (spend caps' territory).

## 5. Verification

- Unit tests: `domains` matching (email, URL, subdomain, look-alike
  `evilacme.com` rejected), policy evaluation (deny wins, wildcard action,
  agent scoping).
- `test/action-policies.test.js` against a local stack: deny blocks with
  403 and an audit row, require passes a good payload and blocks a bad
  one, a per-agent policy doesn't affect another agent.
