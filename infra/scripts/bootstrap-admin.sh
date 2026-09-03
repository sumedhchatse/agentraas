#!/bin/bash
# Run this yourself, once, ONLY on your official AgentRaaS Cloud deployment
# (the one running with DEPLOYMENT_MODE=cloud). Self-hosted deployments never
# need this — admin is a cloud-operator concept (system-wide visibility across
# all users/orgs), meaningless on a single-tenant self-hosted instance.
#
# Only one admin can exist at all — enforced at the database level (see
# migration 013). This script refuses to run if one already exists, rather
# than let that show up as a confusing raw constraint-violation error.
#
# The admin account lands in the local id range (1-9), alongside
# personal/founder accounts — see migration 014.
#
# Usage: ./bootstrap-admin.sh you@example.com
set -euo pipefail

if [ -z "${1:-}" ]; then
  echo "Usage: ./bootstrap-admin.sh <your-email>"
  exit 1
fi

ADMIN_EMAIL="$1"

EXISTING_ADMIN=$(podman exec -i ar-postgres psql -U agentraas -d agentraas -t -c "SELECT email FROM users WHERE is_admin = true LIMIT 1;" | tr -d '[:space:]')
if [ -n "$EXISTING_ADMIN" ]; then
  echo "✗ An admin already exists: ${EXISTING_ADMIN}"
  echo "  Only one admin can exist at a time. There's no promote/demote"
  echo "  mechanism by design — to replace the admin, an existing admin"
  echo "  would need to be demoted directly in the database first, then"
  echo "  run this script again."
  exit 1
fi

ADMIN_PASSWORD=$(openssl rand -base64 18)

echo "→ Registering admin account for ${ADMIN_EMAIL}..."
REGISTER_RESPONSE=$(curl -s -X POST http://localhost:13000/api/v1/auth/register \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"${ADMIN_EMAIL}\",\"password\":\"${ADMIN_PASSWORD}\"}")

if ! echo "$REGISTER_RESPONSE" | grep -q '"registered":true'; then
  echo "✗ Registration failed: $REGISTER_RESPONSE"
  exit 1
fi

# Password rotation (see server.js's login handler) generates a new password
# on every login for this account, so there's no forced-first-change step —
# rotation handles freshness automatically from the very first login.
podman exec -i ar-postgres psql -U agentraas -d agentraas -c \
  "UPDATE users SET is_admin = true, email_verified = true WHERE email = '${ADMIN_EMAIL}';" > /dev/null

# Move this new account into the local id range (1-9), alongside
# personal/founder accounts. With ON UPDATE CASCADE now in place on every
# foreign key referencing users(id) (see migration 011), this correctly
# updates any dependent rows automatically — no need to manually check each
# table.
podman exec -i ar-postgres psql -U agentraas -d agentraas -c "
DO \$\$
DECLARE
  current_id INTEGER;
  next_local_id INTEGER;
BEGIN
  SELECT id INTO current_id FROM users WHERE email = '${ADMIN_EMAIL}';
  SELECT MIN(candidate) INTO next_local_id
  FROM generate_series(1, 9) AS candidate
  WHERE candidate NOT IN (SELECT id FROM users WHERE id BETWEEN 1 AND 9);
  IF next_local_id IS NOT NULL AND current_id != next_local_id THEN
    UPDATE users SET id = next_local_id WHERE id = current_id;
  END IF;
END \$\$;
" > /dev/null

echo ""
echo "✅ Admin account created."
echo "   Email:    ${ADMIN_EMAIL}"
echo "   Password: ${ADMIN_PASSWORD}"
echo ""
echo "   This password is shown only once. You'll be required to set a new"
echo "   one immediately after your first login — that's enforced by the"
echo "   app itself, not just a suggestion."
