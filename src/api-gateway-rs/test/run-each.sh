#!/usr/bin/env bash
# Runs every integration test file on its own against a running server,
# resetting rate limits and breakers in between (test/reset-limits.js).
# Needs TEST_BASE_URL, DATABASE_URL and REDIS_URL, and the server started with
# PROXY_TIMEOUT_SECONDS=2 (outcome-unknown.test.js outlasts it). Local stack:
#   TEST_BASE_URL=http://localhost:13001 REDIS_URL=redis://localhost:16379 \
#   DATABASE_URL=postgres://agentraas:<POSTGRES_PASSWORD>@localhost:15432/agentraas test/run-each.sh
# Exits non-zero if any file fails, printing that file's output.
set -uo pipefail
cd "$(dirname "$0")/.."
out=$(mktemp); trap 'rm -f "$out"' EXIT
failed=0
for f in test/*.test.js; do
  node test/reset-limits.js
  if node --test "$f" > "$out" 2>&1; then
    printf 'ok    %-34s %s\n' "$f" "$(grep -E '^# (pass|skipped)' "$out" | tr '\n' ' ')"
  else
    printf 'FAIL  %s\n' "$f"; grep -vE '^\s*$' "$out" | tail -40; failed=1
  fi
done
exit $failed
