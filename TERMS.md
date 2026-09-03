# AgentRaaS Terms of Service

**Last updated: August 31, 2026**

These terms apply to your use of AgentRaaS — whether self-hosted or on a
deployment we operate. Using AgentRaaS means you agree to these terms and
to [LICENSE.md](./LICENSE.md), which governs the usage limits and
commercial licensing that apply alongside these terms.

## 1. What AgentRaaS does

AgentRaaS is a proxy that sits between your AI agents and third-party
services (Stripe, Twilio, etc., or any endpoint you register as a Custom
Action). It aims to guarantee exactly-once execution of the actions your
agents request, along with audit logging, rate limiting, and circuit
breaking.

## 2. Your responsibilities

- **You control what your agents do.** AgentRaaS forwards the actions your
  agents request — it does not evaluate whether an action is one you
  actually want taken. You're responsible for what your agents are
  configured to do.
- **You're responsible for the credentials you store.** Service credentials
  you add are encrypted at rest, but you're responsible for using
  appropriately scoped keys (e.g. restricted API keys rather than
  full-account keys, where the provider supports it) and revoking them
  when no longer needed.
- **You're responsible for your Custom Actions.** Registering a Custom
  Action means you've reviewed and approved that endpoint. AgentRaaS
  applies an SSRF guard against known private/internal address ranges, but
  cannot evaluate whether a public endpoint you register is itself safe or
  trustworthy.
- **Compliance with the license.** Usage beyond the free tiers described in
  [LICENSE.md](./LICENSE.md), or offering AgentRaaS as a competing hosted
  service, requires a separate commercial agreement.

## 3. Service availability

**Self-hosted:** availability is entirely your responsibility — it runs on
infrastructure you control.

**If we host a deployment for you:** we aim for high availability but do
not guarantee any specific uptime figure under these terms. If you need a
contractual uptime commitment, that's part of a commercial agreement, not
these general terms.

## 4. No warranty

AGENTRAAS IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED. We do not warrant that it will be error-free, that it will
prevent every possible duplicate action, or that third-party services it
forwards requests to will behave correctly. See [LICENSE.md](./LICENSE.md)
Section 5 for the full disclaimer.

## 5. Limitation of liability

To the maximum extent permitted by law, the AgentRaaS authors are not
liable for indirect, incidental, or consequential damages arising from
your use of AgentRaaS — including but not limited to financial loss from
a duplicate or failed action, third-party service outages, or credential
exposure resulting from misconfiguration of your own deployment.

## 6. Termination

We may suspend or terminate access to any deployment we host for you if
you violate these terms or [LICENSE.md](./LICENSE.md). For self-hosted
deployments, you can stop using AgentRaaS at any time; the license itself
governs ongoing obligations (like the usage-limit terms) independent of
whether you keep running the software.

## 7. Changes to these terms

We'll update the "Last updated" date above when these terms change
materially. Continued use after a change means you accept the updated
terms.

## 8. Contact

**support@agentraas.io**

---

*This document is a starting draft and has not been reviewed by a lawyer.
Before relying on it — especially the liability limitation in Section 5,
which varies significantly by jurisdiction — have it reviewed by one.*
