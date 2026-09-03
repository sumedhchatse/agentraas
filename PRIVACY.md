# AgentRaaS Privacy Policy

**Last updated: August 31, 2026**

This policy explains what AgentRaaS collects, stores, and does with your
data — whether you're using a self-hosted deployment or one operated by us.

> **Note on self-hosted deployments:** if you're self-hosting AgentRaaS, you
> (the operator) are the data controller for your own instance — this policy
> describes what the *software itself* collects and how, which applies
> regardless of who's hosting it, but the legal responsibility for that data
> is yours on a self-hosted deployment.

## What we collect

**Account information:** email address and a bcrypt-hashed password. We
never store your plaintext password.

**Service credentials:** API keys/tokens you add for third-party services
(Stripe, Twilio, etc.) via the Credentials panel. These are encrypted at
rest (AES-256-GCM) using a key separate from the database itself, and are
never displayed again after you save them — only a short masked preview
(e.g. `sk_l••••_456`) is shown.

**Agent API keys:** when you use "Connect Agent," we generate and store a
hash of the resulting key (not the key itself) so it can be verified on
future requests. The raw key is shown to you exactly once.

**Audit logs:** every request your agents make through AgentRaaS is logged —
timestamp, service/action, status, duration, and a masked reference to
which key was used. Full request/response payloads are not stored in the
audit log by default.

**Custom Action endpoints:** URLs you register are stored so agents can
invoke them by name. We do not store the *content* sent to or received
from those endpoints beyond what's needed for exactly-once deduplication
(a hash of the request, and the response, held temporarily to serve
duplicate/retry requests — not kept indefinitely).

## What we don't do

- We do not sell your data.
- We do not use your stored credentials for anything other than forwarding
  the specific requests your agents make.
- We do not share data with third parties except the upstream services
  your agent explicitly asked us to call (e.g. Stripe, when your agent
  requests a charge).

## Data retention

Audit log entries and cached dedup results age out over time; deduplication
cache entries expire automatically (currently 24 hours). Account and
credential data is retained until you delete it or close your account.

## Your rights

You can revoke any credential or API key at any time from the dashboard.
For account deletion or data export requests, contact
**support@agentraas.io**.

## Security

See [SECURITY.md](./SECURITY.md) for how to report a vulnerability, and
our approach to handling one if you find it.

## Changes to this policy

We'll update the "Last updated" date above when this policy changes
materially.

## Contact

**support@agentraas.io**

---

*This document is a starting draft and has not been reviewed by a lawyer.
Before relying on it for a real product handling real user data —
especially third-party financial/API credentials — have it reviewed by
one, particularly for compliance with regulations like GDPR or CCPA if
you have users in those jurisdictions.*
