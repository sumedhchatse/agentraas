#!/bin/bash
# Sets a new random password for the single admin account and prints it.
#
#   ssh agentraas@192.168.1.8 "bash ~/agentraas/infra/scripts/admin-password.sh"
#
# Why: the admin password rotates on every login and the new one is only ever
# written to the API container's log, which a restart deletes (--rm). This is
# the recovery path that doesn't depend on that log or on email.
#
# The old password stops working at once. Logging in with the new one rotates
# it again (read `podman logs ar-api-rs | grep "ADMIN PASSWORD ROTATED"`
# right after, or just run this script again). Nothing is stored in plain text.
set -euo pipefail

# Same hash the app writes (bcrypt, 12 rounds; auth/mod.rs SALT_ROUNDS).
read -r password hash < <(python3 -c '
import secrets, bcrypt
p = secrets.token_urlsafe(18)
print(p, bcrypt.hashpw(p.encode(), bcrypt.gensalt(12)).decode())')

email=$(podman exec -i ar-postgres psql -U agentraas -d agentraas -qtA -v ON_ERROR_STOP=1 -v h="$hash" <<'SQL'
UPDATE users SET password_hash = :'h', must_change_password = false WHERE is_admin RETURNING email;
SQL
)

if [ -z "$email" ]; then
  echo "No admin account exists; create one with bootstrap-admin.sh." >&2
  exit 1
fi
echo "Admin:    $email"
echo "Password: $password"
