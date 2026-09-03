#!/bin/bash
# set-org-limit.sh — set (or remove) a custom monthly call limit for one
# specific user or org, overriding the global CLOUD_MONTHLY_LIMIT default.
# Only takes effect in cloud mode — self-hosted deployments don't enforce a
# limit at all (see checkUsageLimit in server.js).
#
# Accepts EITHER an email address (resolved to that user's default org) OR
# a raw org_id directly — useful if someone has connected multiple agents
# under different org_ids and you need to target a specific one rather than
# their default.
#
# Usage:
#   ./set-org-limit.sh someone@example.com <new_limit>
#   ./set-org-limit.sh <org_id> <new_limit>
#   ./set-org-limit.sh someone@example.com remove   — remove the override, back to the global default
set -euo pipefail

if [ -z "${1:-}" ] || [ -z "${2:-}" ]; then
  echo "Usage: ./set-org-limit.sh <email-or-org_id> <new_limit>"
  echo "       ./set-org-limit.sh <email-or-org_id> remove"
  exit 1
fi

TARGET="$1"
LIMIT="$2"

if [[ "$TARGET" == *"@"* ]]; then
  ORG_ID=$(podman exec -i ar-postgres psql -U agentraas -d agentraas -t -c \
    "SELECT org_id FROM users WHERE email = '${TARGET}';" | tr -d '[:space:]')
  if [ -z "$ORG_ID" ]; then
    echo "✗ No user found with email ${TARGET} (or they have no org_id — shouldn't happen for any account created after the default-org-on-signup change)."
    exit 1
  fi
  echo "→ Resolved ${TARGET} to org ${ORG_ID}"
else
  ORG_ID="$TARGET"
fi

if [ "$LIMIT" = "remove" ]; then
  podman exec -i ar-postgres psql -U agentraas -d agentraas -c \
    "DELETE FROM org_limit_overrides WHERE org_id = '${ORG_ID}';"
  echo "✅ Removed custom limit for ${ORG_ID} — back to the global default."
  exit 0
fi

if ! [[ "$LIMIT" =~ ^[0-9]+$ ]]; then
  echo "✗ Limit must be a positive whole number (or the word 'remove')."
  exit 1
fi

podman exec -i ar-postgres psql -U agentraas -d agentraas -c \
  "INSERT INTO org_limit_overrides (org_id, monthly_limit) VALUES ('${ORG_ID}', ${LIMIT})
   ON CONFLICT (org_id) DO UPDATE SET monthly_limit = ${LIMIT}, updated_at = NOW();"

echo "✅ ${ORG_ID} now has a custom monthly limit of ${LIMIT}."
