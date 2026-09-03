# Security Policy

AgentRaaS sits between AI agents and real-world services — it stores encrypted
credentials (Stripe, Twilio, and similar) on behalf of the people running it.
We take security reports seriously and would rather hear about a problem
privately, with time to fix it, than have it show up as a public GitHub issue.

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, email **support@agentraas.io** with:
- A description of the vulnerability and its potential impact
- Steps to reproduce it (a proof-of-concept, if you have one)
- Which version/commit you tested against

We'll acknowledge your report within 5 business days and aim to have a fix
or mitigation plan within 30 days for confirmed issues, sooner for anything
critical (auth bypass, credential exposure, remote code execution).

## Scope

In scope:
- The AgentRaaS API gateway and dashboard (this repository)
- Authentication, session handling, and the encrypted credential storage
- The dedup/exactly-once execution logic
- SSRF or injection issues in Custom Actions or curated service routing

Out of scope:
- Vulnerabilities in third-party services AgentRaaS forwards requests to
  (Stripe, Twilio, etc.) — report those to the respective provider
- Issues requiring physical access to a self-hosted deployment's server
- Social engineering

## Disclosure

We ask for a reasonable window to fix a confirmed vulnerability before any
public disclosure. We're happy to credit reporters (by name or handle) in
the fix's release notes, if you'd like that.

## Supported versions

AgentRaaS is under active development; only the latest commit on `main` is
supported with security fixes at this stage.
