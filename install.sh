#!/bin/bash
# AgentRaaS one-command installer.
# Usage: unzip the downloaded package, cd into it, then:
#   ./install.sh
set -euo pipefail

echo "🛡️  AgentRaaS Installer"
echo ""

# ─── 1. Check prerequisites ───
command -v podman >/dev/null 2>&1 || { echo "✗ podman is required. Install it first: https://podman.io/docs/installation"; exit 1; }
command -v podman-compose >/dev/null 2>&1 || { echo "✗ podman-compose is required: pip install podman-compose"; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "✗ openssl is required."; exit 1; }
echo "✓ Prerequisites found"

# ─── 2. Generate secrets (only if not already configured) ───
# Everything below is read by compose.yaml's `env_file: - .env` — every
# service reads this one file, so the secrets never appear in a
# podman-compose command-echo the way `-e KEY=value` mappings would.
# POSTGRES_PASSWORD and MINIO_ROOT_* are required by ar-postgres/ar-minio
# themselves; DATABASE_URL is required by ar-api-rs (sqlx reads it
# directly and fails fast if it's missing).
ENV_FILE=".env"
if [ ! -f "$ENV_FILE" ]; then
  echo "→ Generating secrets..."
  JWT_SECRET=$(openssl rand -base64 48)
  CRED_KEY=$(openssl rand -base64 32)
  PG_PASSWORD=$(openssl rand -hex 24)
  MINIO_PASSWORD=$(openssl rand -hex 24)

  cat > "$ENV_FILE" << ENVEOF
POSTGRES_PASSWORD=${PG_PASSWORD}
DATABASE_URL=postgres://agentraas:${PG_PASSWORD}@ar-postgres:5432/agentraas
REDIS_URL=redis://ar-redis:6379
JWT_SECRET=${JWT_SECRET}
CREDENTIALS_ENCRYPTION_KEY=${CRED_KEY}
MINIO_ROOT_USER=agentraas
MINIO_ROOT_PASSWORD=${MINIO_PASSWORD}
PUBLIC_URL=http://localhost:13001
DEPLOYMENT_MODE=self-hosted
SELF_HOST_MONTHLY_LIMIT=100000
ENVEOF
  chmod 600 "$ENV_FILE"

  echo "✓ Secrets generated (saved to $ENV_FILE — back this up, it won't be shown again)"
else
  echo "✓ $ENV_FILE already exists — skipping secret generation"
fi

# ─── 3. SELinux relabeling (Fedora/RHEL-family hosts only) ───
if command -v getenforce >/dev/null 2>&1 && [ "$(getenforce)" != "Disabled" ]; then
  echo "→ SELinux detected — relabeling project files..."
  sudo semanage fcontext -a -t container_file_t "$(pwd)(/.*)?" 2>/dev/null || true
  sudo restorecon -Rv "$(pwd)" > /dev/null
  echo "✓ SELinux context set"
fi

# ─── 4. Build the API image ───
# A from-source release build (Rust, not a quick npm install) — first run
# genuinely takes a few minutes, not a hang. Build explicitly (rather than
# letting it happen silently inside `up -d`) so that wait is visible.
#
# A paid-tier self-host package (from agentraas.io, not this public
# clone) ships a compose.yaml referencing a prebuilt `image:` instead of
# a `build:` block - no source in that package at all to build from.
# `podman-compose up` pulls/loads that instead, so skip this step
# entirely rather than fail on a missing Containerfile.
if grep -q "^\s*image: agentraas-enterprise" compose.yaml 2>/dev/null; then
  echo "→ Prebuilt Enterprise image detected in compose.yaml — skipping build."
  echo "  Make sure you've already run: podman load -i agentraas-enterprise.tar"
else
  echo "→ Building the API image (first run compiles from source — a few minutes, not a hang)..."
  podman-compose build ar-api-rs
fi

# ─── 5. Start the stack ───
echo "→ Starting containers..."
podman-compose up -d > /dev/null 2>&1

# ─── 5b. Fix Redis data directory ownership ───
# Rootless Podman's UID namespace mapping doesn't always land where Redis's
# own process expects on a freshly created volume — this shows up as
# "MISCONF Errors writing to the AOF file: Permission denied" on the very
# first write. Fixing it here means nobody has to debug it manually.
echo "→ Fixing Redis data directory ownership..."
podman unshare chown -R 999:999 "$(pwd)/data/redis" 2>/dev/null || true
podman restart ar-redis > /dev/null 2>&1 || true
sleep 2

# ─── 5c. Fix Postgres data directory ownership ───
# Same rootless Podman UID-mapping issue as Redis above, just showing up as
# a different symptom: "could not open file global/pg_filenode.map:
# Permission denied" the first time Postgres tries to read its own data
# directory after a container recreate.
echo "→ Fixing Postgres data directory ownership..."
podman unshare chown -R 999:999 "$(pwd)/data/postgres" 2>/dev/null || true
podman restart ar-postgres > /dev/null 2>&1 || true
sleep 2

# ─── 6. Wait for Postgres to actually be ready before migrating ───
echo "→ Waiting for the database..."
for i in $(seq 1 30); do
  if podman exec ar-postgres pg_isready -U agentraas > /dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ─── 7. Run every migration, in order ───
echo "→ Running database migrations..."
for f in infra/migrations/*.sql; do
  podman exec -i ar-postgres psql -U agentraas -d agentraas < "$f" > /dev/null
done
echo "✓ Migrations complete"

# ─── 8. Recreate cleanly so SELinux labels/fresh env apply ───
echo "→ Finalizing..."
podman-compose down > /dev/null 2>&1
podman-compose up -d > /dev/null 2>&1
sleep 3

echo ""
echo "✅ AgentRaaS is running."
echo "   Dashboard: http://localhost:13001/dashboard"
echo "   Register an account there to get started."
echo ""
echo "   Run the test suite any time with:"
echo "     cd src/api-gateway-rs && npm install && TEST_BASE_URL=http://localhost:13001 npm test"
