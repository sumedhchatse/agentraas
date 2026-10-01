# AgentRaaS Helm chart

Runs the AgentRaaS API (Community edition image) on Kubernetes, with a
bundled single-instance Postgres and Redis, or your own.

```bash
helm install agentraas ./infra/helm/agentraas \
  --set publicUrl=https://agentraas.example.com \
  --set ingress.enabled=true --set ingress.host=agentraas.example.com
```

With a managed database and Redis:

```bash
helm install agentraas ./infra/helm/agentraas \
  --set postgresql.enabled=false --set externalDatabase.url=postgres://user:pass@host:5432/agentraas \
  --set redis.enabled=false --set externalRedis.url=redis://host:6379
```

What it does:

- Generates `JWT_SECRET`, `CREDENTIALS_ENCRYPTION_KEY` and the database
  password on first install and keeps them on upgrade. Or bring your own
  Secret with `existingSecret`. **Back up `CREDENTIALS_ENCRYPTION_KEY`**:
  stored third-party credentials can't be decrypted without it.
- Runs every file in `infra/migrations` before the API starts (an init
  container, under a Postgres advisory lock, so replicas don't race). All
  migrations are safe to re-run.
- Mounts `config/services.json` (the built-in service catalog) as a
  ConfigMap. Both are symlinks into the repo, so the chart never drifts
  from them.
- Runs the API with a read-only root filesystem and probes on `/health`.
- `/metrics` (Prometheus) is on when `metricsToken` is set; scrape it with
  that token as a bearer token.

See `values.yaml` for every option. Set `clientIpHeader` to the header your
ingress writes with the client IP (e.g. `x-real-ip` for ingress-nginx),
otherwise every client shares one rate-limit bucket.

Verified: `helm lint`, `kubeconform -strict` on both value sets, the
migration step against an empty Postgres (twice concurrently, then again),
and the API image running read-only with only the ConfigMap mounted. Not
yet run on a real cluster.
