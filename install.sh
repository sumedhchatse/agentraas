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
ENV_FILE="src/api-gateway/.env"
if [ ! -f "$ENV_FILE" ]; then
  echo "→ Generating secrets..."
  JWT_SECRET=$(openssl rand -base64 48)
  CRED_KEY=$(openssl rand -base64 32)
  PG_PASSWORD=$(openssl rand -hex 24)

  cat > "$ENV_FILE" << ENVEOF
NODE_ENV=development
PORT=3000
REDIS_URL=redis://ar-redis:6379
DATABASE_URL=postgres://agentraas:${PG_PASSWORD}@ar-postgres:5432/agentraas
JWT_SECRET=${JWT_SECRET}
CREDENTIALS_ENCRYPTION_KEY=${CRED_KEY}
PUBLIC_URL=http://localhost:13000
DEPLOYMENT_MODE=self-hosted
SELF_HOST_MONTHLY_LIMIT=100000
ENVEOF

  # Keep compose.yaml's Postgres password in sync with the one just generated.
  sed -i.bak "s/POSTGRES_PASSWORD: .*/POSTGRES_PASSWORD: ${PG_PASSWORD}/" compose.yaml
  sed -i.bak "s#postgres://agentraas:[^@]*@#postgres://agentraas:${PG_PASSWORD}@#g" compose.yaml
  rm -f compose.yaml.bak

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

# ─── 4. Start the stack ───
echo "→ Starting containers..."
podman-compose up -d > /dev/null 2>&1

# ─── 4b. Fix Redis data directory ownership ───
# Rootless Podman's UID namespace mapping doesn't always land where Redis's
# own process expects on a freshly created volume — this shows up as
# "MISCONF Errors writing to the AOF file: Permission denied" on the very
# first write. Fixing it here means nobody has to debug it manually.
echo "→ Fixing Redis data directory ownership..."
podman unshare chown -R 999:999 "$(pwd)/data/redis" 2>/dev/null || true
podman restart ar-redis > /dev/null 2>&1 || true
sleep 2

# ─── 4c. Fix Postgres data directory ownership ───
# Same rootless Podman UID-mapping issue as Redis above, just showing up as
# a different symptom: "could not open file global/pg_filenode.map:
# Permission denied" the first time Postgres tries to read its own data
# directory after a container recreate.
echo "→ Fixing Postgres data directory ownership..."
podman unshare chown -R 999:999 "$(pwd)/data/postgres" 2>/dev/null || true
podman restart ar-postgres > /dev/null 2>&1 || true
sleep 2

# ─── 5. Wait for Postgres to actually be ready before migrating ───
echo "→ Waiting for the database..."
for i in $(seq 1 30); do
  if podman exec ar-postgres pg_isready -U agentraas > /dev/null 2>&1; then
    break
  fi
  sleep 1
done

# ─── 6. Run every migration, in order ───
echo "→ Running database migrations..."
for f in infra/migrations/*.sql; do
  podman exec -i ar-postgres psql -U agentraas -d agentraas < "$f" > /dev/null
done
echo "✓ Migrations complete"

# ─── 7. Install Node dependencies INSIDE the container ───
# Must happen inside the container, not on the host — native modules need to
# build for the container's actual OS, not your host's (this bit us hard
# during development: a bcrypt binary built on the host silently crashed
# every login once run inside an Alpine container).
echo "→ Installing dependencies (inside the container)..."
podman exec ar-api npm install > /dev/null 2>&1 || {
  echo "⚠ Initial install failed — retrying after a moment (container may still be starting)..."
  sleep 5
  podman exec ar-api npm install > /dev/null
}
echo "✓ Dependencies installed"

# ─── 8. Recreate cleanly so everything (SELinux labels, fresh install) applies ───
echo "→ Finalizing..."
podman-compose down > /dev/null 2>&1
podman-compose up -d > /dev/null 2>&1
sleep 3

echo ""
echo "✅ AgentRaaS is running."
echo "   Dashboard: http://localhost:13000/dashboard"
echo "   Register an account there to get started."
echo ""
echo "   Run the test suite any time with:"
echo "     podman exec -it ar-api npm test"
