#!/bin/bash
set -e
cd ~/agentraas

CONFLICT=0
for PORT in 13000 15432 16379 19000 19001; do
  if ss -tlnp 2>/dev/null | grep -q ":$PORT "; then
    echo "ERROR: Port $PORT is already in use."
    ss -tlnp | grep ":$PORT " || true
    CONFLICT=1
  fi
done

if [ "$CONFLICT" -eq 1 ]; then
  echo "Fix port conflicts before starting."
  exit 1
fi

mkdir -p data/postgres data/redis data/minio

podman-compose -f compose.yaml up -d --build

echo ""
echo "=========================================="
echo "AgentRaaS dev stack running:"
echo "  API:       http://localhost:13000"
echo "  Postgres:  localhost:15432"
echo "  Redis:     localhost:16379"
echo "  MinIO:     http://localhost:19001"
echo ""
echo "Test:    curl http://localhost:13000/health"
echo "Logs:    podman-compose -f compose.yaml logs -f ar-api"
echo "Stop:    podman-compose -f compose.yaml down"
echo "=========================================="
