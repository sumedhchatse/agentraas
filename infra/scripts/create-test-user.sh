#!/bin/bash
# create-test-user.sh — create a regular (non-admin) test account, either in
# the local range (id 1-9 — exempt from usage limits, like a personal
# account) or the external range (id 100+ — a normal registered customer,
# no special treatment, subject to the standard limit).
#
# Registers through the real API (same path a real signup takes), then
# auto-verifies via SQL so you don't need to click an email link for a test
# account. For "internal", also reassigns the id into the 1-9 range — same
# pattern as bootstrap-admin.sh, safe because of the ON UPDATE CASCADE fix
# (migration 011).
#
# Usage:
#   ./create-test-user.sh someone@example.com internal
#   ./create-test-user.sh someone@example.com external
set -euo pipefail

if [ -z "${1:-}" ] || [ -z "${2:-}" ]; then
  echo "Usage: ./create-test-user.sh <email> internal|external"
  exit 1
fi

TEST_EMAIL="$1"
KIND="$2"

if [ "$KIND" != "internal" ] && [ "$KIND" != "external" ]; then
  echo "✗ Second argument must be 'internal' or 'external', not '${KIND}'."
  exit 1
fi

TEST_PASSWORD=$(openssl rand -base64 18)

echo "→ Registering ${KIND} test account for ${TEST_EMAIL}..."
REGISTER_RESPONSE=$(curl -s -X POST http://localhost:13000/api/v1/auth/register \
  -H "Content-Type: application/json" \
  -d "{\"email\":\"${TEST_EMAIL}\",\"password\":\"${TEST_PASSWORD}\"}")

if ! echo "$REGISTER_RESPONSE" | grep -q '"registered":true'; then
  echo "✗ Registration failed: $REGISTER_RESPONSE"
  exit 1
fi

# Auto-verify — this is a test account, not worth the click-the-email-link
# round trip real users go through.
podman exec -i ar-postgres psql -U agentraas -d agentraas -c \
  "UPDATE users SET email_verified = true WHERE email = '${TEST_EMAIL}';" > /dev/null

if [ "$KIND" = "internal" ]; then
  # Move into the local range (1-9) — exempt from usage limits, same
  # ownership-preserving reassignment bootstrap-admin.sh uses.
  podman exec -i ar-postgres psql -U agentraas -d agentraas -c "
  DO \$\$
  DECLARE
    current_id INTEGER;
    next_local_id INTEGER;
  BEGIN
    SELECT id INTO current_id FROM users WHERE email = '${TEST_EMAIL}';
    SELECT MIN(candidate) INTO next_local_id
    FROM generate_series(1, 9) AS candidate
    WHERE candidate NOT IN (SELECT id FROM users WHERE id BETWEEN 1 AND 9);
    IF next_local_id IS NOT NULL AND current_id != next_local_id THEN
      UPDATE users SET id = next_local_id WHERE id = current_id;
    END IF;
  END \$\$;
  " > /dev/null
fi
# External needs no id change — normal registrations already land at 100+
# (see migration 012's sequence adjustment).

FINAL_ID=$(podman exec -i ar-postgres psql -U agentraas -d agentraas -t -c \
  "SELECT id FROM users WHERE email = '${TEST_EMAIL}';" | tr -d '[:space:]')

echo ""
echo "✅ ${KIND} test account created."
echo "   Email:    ${TEST_EMAIL}"
echo "   Password: ${TEST_PASSWORD}"
echo "   User id:  ${FINAL_ID}"
