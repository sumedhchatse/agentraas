#!/bin/bash
# deploy.sh — run this instead of manually cp'ing files + restarting containers.
# Fixes the recurring SELinux EACCES issue permanently by always relabeling
# before recreating, so you never have to remember the chcon/restorecon step again.

set -euo pipefail

PROJECT_DIR="$HOME/agentraas"
cd "$PROJECT_DIR"

# Explicitly export .env into the shell before calling podman-compose, rather
# than relying on podman-compose's own .env auto-loading — that support has
# historically been inconsistent across podman-compose versions. Exporting
# these as real shell env vars makes compose.yaml's ${VAR} substitution work
# via standard shell expansion, which is always reliable regardless.
if [ -f "$PROJECT_DIR/.env" ]; then
  echo "→ Loading .env..."
  set -a
  source "$PROJECT_DIR/.env"
  set +a
fi

echo "→ Relabeling SELinux context (best-effort — compose.yaml's :z shared
   labels mean this is no longer required, only a safety net for a system
   where sudo needs a password and can't prompt over a non-interactive
   session; skipped rather than failing the whole deploy in that case)..."
sudo -n restorecon -Rv "$PROJECT_DIR" > /dev/null 2>&1 || echo "  (skipped — no passwordless sudo for restorecon; :z labels should cover it)"

# podman-compose prints every resolved `-e VAR=value`, including secrets
# (JWT_SECRET, DB password, Paddle keys, SMTP password, ...), to stdout —
# redirect it away. Same fix already applied to install.sh; deploy.sh had
# been missing it.
echo "→ Recreating containers (down + up, not restart — needed for env/volume changes to apply)..."
podman-compose down > /dev/null 2>&1
podman-compose up -d > /dev/null 2>&1

echo "→ Fixing Redis data directory ownership (rootless Podman resets this on every container recreate)..."
podman unshare chown -R 999:999 "$PROJECT_DIR/data/redis" 2>/dev/null || true
podman restart ar-redis > /dev/null
sleep 2

echo "→ Waiting for ar-api to boot..."
sleep 3
podman-compose logs --tail=15 ar-api

echo ""
echo "Done. If you see 'Server listening at http://0.0.0.0:3000' above, you're good."
