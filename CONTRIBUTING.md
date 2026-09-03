# Contributing to AgentRaaS

Thanks for your interest in AgentRaaS. A few things to know before you dive in.

## License

AgentRaaS is source-available under a custom fair-code license (see
[LICENSE.md](./LICENSE.md)) — not a traditional open-source license. By
submitting a contribution, you agree that it may be distributed under the
same license terms.

Notably, the license restricts offering AgentRaaS (or a derivative of it)
as a competing hosted service. If you're contributing as part of building
something like that, reach out to support@agentraas.io first.

## Reporting bugs

Open a GitHub issue with:
- What you expected to happen vs. what actually happened
- Steps to reproduce
- Your environment (self-hosted via Podman/Docker, Node version, etc.)

**Security vulnerabilities are the one exception** — see
[SECURITY.md](./SECURITY.md) instead of opening a public issue.

## Suggesting features

Open an issue describing the use case, not just the feature — especially
useful for Custom Actions or new curated service integrations, where
knowing *why* helps evaluate the request.

## Pull requests

1. Fork the repo and create a branch from `main`
2. Keep changes focused — one logical change per PR is easier to review
3. If you touch `server.js`, run the test suite: `npm test` (from
   `src/api-gateway/`, with the dev stack running)
4. Describe what changed and why in the PR description

## Adding a new curated service integration

Most new integrations don't need code changes — just an entry in
`config/services.json` following the existing pattern (see the file for
examples). Only add code-level changes if the service needs something the
generic proxy pattern can't express.

## Development setup

See the README for the full local setup (Podman, migrations, environment
variables). Short version:

```bash
podman-compose up -d
podman exec -i ar-postgres psql -U agentraas -d agentraas < infra/migrations/<latest>.sql   # for any new migrations
cd src/api-gateway && npm install && npm run dev
```
