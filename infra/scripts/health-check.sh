#!/bin/bash
# health-check.sh — a daily morning digest of production, emailed every
# run (not just on failure — see setup instructions at the bottom for the
# 7am IST cron line). Three layers, each answering a different question:
#   - Server:      is the host itself okay (disk, containers, panics)?
#   - Application: is real traffic flowing and healthy (24h audit_log
#                  summary, dead-letter-queue backlog)?
#   - Services:    are the curated integrations (Stripe, Twilio, ...)
#                  currently circuit-broken (real outage/bad credential)?
#
# Why disk space specifically: a real incident (2026-09-09) had
# `podman-compose build` report success while the underlying compile had
# actually failed from disk exhaustion — this check exists so that gets
# caught by a daily email, not by someone noticing a feature isn't live.

set -euo pipefail

REPO_DIR="${HOME}/agentraas"
ALERT_EMAIL="${ALERT_EMAIL:-sumedhchatse11@gmail.com}"
DISK_THRESHOLD_PCT="${DISK_THRESHOLD_PCT:-85}"
REQUIRED_CONTAINERS=(ar-api-rs ar-postgres ar-redis ar-minio)

# Deliberately NOT `source .env` — this repo's .env has a multi-line PEM
# value (LICENSE_SIGNING_PRIVATE_KEY) that isn't valid bash syntax to
# source (real bug hit while writing this script). Pull out only the
# single-line SMTP_* keys actually needed, quotes stripped if present.
env_get() {
  local line
  line=$(grep -E "^$1=" "${REPO_DIR}/.env" | tail -1)
  line="${line#*=}"
  line="${line%\"}"; line="${line#\"}"
  printf '%s' "${line}"
}
SMTP_HOST=$(env_get SMTP_HOST)
SMTP_PORT=$(env_get SMTP_PORT)
SMTP_USER=$(env_get SMTP_USER)
SMTP_PASS=$(env_get SMTP_PASS)
SMTP_FROM=$(env_get SMTP_FROM)
# curl's --mail-from is the raw SMTP envelope address — Resend (and most
# providers) reject the "Display Name <addr>" form there with a 501
# syntax error (found while testing this script). Strip it down to the
# bare address for the envelope; the header below keeps the display name.
SMTP_FROM_ENVELOPE="${SMTP_FROM}"
if [[ "${SMTP_FROM}" == *"<"*">"* ]]; then
  SMTP_FROM_ENVELOPE=$(printf '%s' "${SMTP_FROM}" | sed -E 's/.*<([^>]+)>.*/\1/')
fi

issues=()
report=()

# ─── Server status ───────────────────────────────────────────────────
report+=("=== SERVER ===")

check_disk() {
  local mount="$1"
  local pct
  pct=$(df -P "${mount}" | awk 'NR==2 {gsub("%","",$5); print $5}')
  report+=("Disk ${mount}: ${pct}% used")
  if [ "${pct}" -ge "${DISK_THRESHOLD_PCT}" ]; then
    issues+=("Disk at ${mount} is ${pct}% full (threshold ${DISK_THRESHOLD_PCT}%) — podman image prune -f, see docs/kb/07-error-sop.md")
  fi
}
check_disk "/"
graph_root=$(podman info --format '{{.Store.GraphRoot}}' 2>/dev/null || echo "")
if [ -n "${graph_root}" ]; then
  check_disk "${graph_root}"
fi

for c in "${REQUIRED_CONTAINERS[@]}"; do
  status=$(podman inspect "${c}" --format '{{.State.Status}}' 2>/dev/null || echo "missing")
  report+=("Container ${c}: ${status}")
  if [ "${status}" != "running" ]; then
    issues+=("Container ${c} is ${status}, not running")
  fi
done
# ar-api (Node) is the deliberate rollback slot, not required to be up —
# reported informationally only, never counted as an issue.
ar_api_status=$(podman inspect ar-api --format '{{.State.Status}}' 2>/dev/null || echo "missing")
report+=("Container ar-api (rollback slot, not required): ${ar_api_status}")

health_body=$(curl -s -m 5 http://localhost:13001/health || echo "")
if echo "${health_body}" | grep -q '"ok":true'; then
  report+=("ar-api-rs /health: ok")
else
  report+=("ar-api-rs /health: NOT ok — ${health_body:-<no response>}")
  issues+=("http://localhost:13001/health did not return ok:true — got: ${health_body:-<no response>}")
fi

panic_count=$(podman logs ar-api-rs --since 15m 2>&1 | grep -ic panic || true)
report+=("Panics in ar-api-rs logs (last 15m): ${panic_count}")
if [ "${panic_count}" -gt 0 ]; then
  issues+=("${panic_count} panic(s) in ar-api-rs logs in the last 15 minutes")
fi

# ─── Application status (real traffic, last 24h, system-wide) ─────────
report+=("")
report+=("=== APPLICATION (last 24h) ===")
psql() { podman exec ar-postgres psql -U agentraas -d agentraas -t -A "$@"; }

audit_line=$(psql -c "SELECT COUNT(*), COUNT(*) FILTER (WHERE status='success'), COUNT(*) FILTER (WHERE status='deduplicated'), COUNT(*) FILTER (WHERE status='blocked'), COUNT(*) FILTER (WHERE status='error') FROM audit_log WHERE created_at >= NOW() - INTERVAL '24 hours';" 2>/dev/null || echo "|||||")
IFS='|' read -r a_total a_success a_dedup a_blocked a_error <<< "${audit_line}"
report+=("Actions: ${a_total:-0} total — ${a_success:-0} success, ${a_dedup:-0} deduplicated, ${a_blocked:-0} blocked, ${a_error:-0} errors")

dlq_open=$(psql -c "SELECT COUNT(*) FROM dead_letter_queue WHERE replayed_at IS NULL AND dismissed_at IS NULL;" 2>/dev/null || echo "0")
report+=("Dead-letter queue backlog (unreplayed, undismissed): ${dlq_open:-0}")
if [ -n "${dlq_open:-}" ] && [ "${dlq_open}" -gt 20 ]; then
  issues+=("Dead-letter queue backlog is ${dlq_open} — real upstream failures piling up unreviewed")
fi

# ─── Service status (curated integrations' circuit-breaker state) ─────
report+=("")
report+=("=== SERVICES ===")
services=$(grep -E '^  "[a-zA-Z_]+": \{$' "${REPO_DIR}/config/services.json" | sed -E 's/^ *"([a-zA-Z_]+)".*/\1/')
degraded=()
while IFS= read -r svc; do
  [ -z "${svc}" ] && continue
  raw=$(podman exec ar-redis redis-cli GET "circuit:${svc}" 2>/dev/null || echo "")
  state="closed"
  if [ -n "${raw}" ]; then
    state=$(printf '%s' "${raw}" | sed -E 's/.*"state":"([a-z-]+)".*/\1/')
  fi
  report+=("${svc}: ${state}")
  if [ "${state}" != "closed" ]; then
    degraded+=("${svc} (${state})")
  fi
done <<< "${services}"
if [ "${#degraded[@]}" -gt 0 ]; then
  issues+=("Circuit not closed for: $(IFS=', '; echo "${degraded[*]}") — check whether it's a real outage or one org's bad credential (see docs/kb/07-error-sop.md)")
fi

# ─── Send the daily digest ─────────────────────────────────────────────
if [ "${#issues[@]}" -gt 0 ]; then
  subject="AgentRaaS daily report — ${#issues[@]} issue(s)"
  summary="ISSUES:\n$(printf '%s\n' "${issues[@]}")\n"
else
  subject="AgentRaaS daily report — all clear"
  summary="No issues.\n"
fi
body="$(printf '%b\n%s' "${summary}" "$(printf '%s\n' "${report[@]}")")"

curl -s --url "smtp://${SMTP_HOST}:${SMTP_PORT}" --ssl-reqd \
  --mail-from "${SMTP_FROM_ENVELOPE}" --mail-rcpt "${ALERT_EMAIL}" \
  --user "${SMTP_USER}:${SMTP_PASS}" --upload-file - <<EOF
From: ${SMTP_FROM}
To: ${ALERT_EMAIL}
Subject: ${subject}

${body}
EOF

printf '%s\n' "${report[@]}"
if [ "${#issues[@]}" -gt 0 ]; then
  printf 'ISSUE: %s\n' "${issues[@]}" >&2
  exit 1
fi

# ─── One-time setup: run every morning at 7:00 AM IST ───
# Production's system timezone is UTC (`timedatectl`), so 7:00 IST
# (UTC+5:30) is scheduled as 01:30 UTC directly, rather than relying on
# cron's own (inconsistently supported) CRON_TZ= feature.
# crontab -e
# Add this line:
# 30 1 * * * /home/agentraas/agentraas/infra/scripts/health-check.sh >> /home/agentraas/health-check.log 2>&1
#
# ponytail: email only, no WhatsApp — checked, and the only WhatsApp
# Business credential ever stored (org "my_team") was test data created
# and revoked 7 minutes later on 2026-09-02, not a live integration.
# Wiring this up for real needs an actual Meta WhatsApp Business API
# credential (phone number ID + access token) registered via the
# Credentials panel, plus confirming the target number is within the 24h
# messaging window or a template is approved — add a send_whatsapp() call
# here via AgentRaaS's own `/v1/sdk/whatsapp/message.send` once that
# exists.
