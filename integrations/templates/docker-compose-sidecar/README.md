# AgentRaaS as a Docker Compose sidecar

Paste [`agentraas-sidecar.yaml`](./agentraas-sidecar.yaml)'s service
blocks into your existing `docker-compose.yml`, set the two required env
vars (`AGENTRAAS_JWT_SECRET`, `AGENTRAAS_CRED_KEY` — `openssl rand -hex 32`
and `openssl rand -base64 32` respectively), and `docker compose up -d`.
AgentRaaS is then reachable at `http://agentraas:13000` from any other
service on the same compose network — self-hosted, free, unlimited on
every tier (see [LICENSE.md](https://github.com/sumedhchatse/agentraas/blob/main/LICENSE.md)).

The container image is published from this repo's own CI on every push
to `main` — `ghcr.io/sumedhchatse/agentraas:latest`.

## Flowise

Use the [Custom Tool](../../flowise-custom-tool) — its `function.js`
already targets `AGENTRAAS_BASE_URL` from Flowise Variables; set that to
`http://agentraas:13000` once both are on the same compose network.

## Langflow

Use the [custom component](../../langflow-custom-component) —
`base_url` defaults to `http://localhost:13000`; override it to
`http://agentraas:13000` in the component's input field once both are on
the same compose network.

## Dify

No native plugin yet (see the top-level [templates README](../README.md)).
Point Dify's HTTP Request tool/node at
`http://agentraas:13000/v1/sdk/:service/:action` with the
`X-AgentRaaS-Key` header, same as any other client — Dify's own
docker-compose setup already runs multiple services on one network, so
`agentraas` just becomes one more hostname on it.

## First-time setup, once the container's running

1. Open `http://localhost:13000/dashboard` from the host running Compose
   and register the first account (self-hosted is single-tenant — no
   special admin bootstrap needed, see the main repo's
   [Getting Started](https://github.com/sumedhchatse/agentraas/blob/main/GETTING_STARTED.md)).
2. **+ Connect agent** to get an API key, org id, and agent id.
3. Add credentials for whatever real service you're calling (Stripe,
   Twilio, ...) from the **Credentials** panel — AgentRaaS forwards using
   those; your low-code tool never sees the real upstream key.
