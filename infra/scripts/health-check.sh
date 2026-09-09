#!/bin/bash
# health-check.sh — checks the AgentRaaS production host itself (disk,
# required containers, the live service's /health, recent panics) and
# emails an alert only on a state CHANGE (healthy -> unhealthy, or back).
# Run via cron every 10-15 minutes (see setup instructions at the bottom).
#
# Why disk space specifically: a real incident (2026-09-09) had
# `podman-compose build` report success while the underlying compile had
# actually failed from disk exhaustion — this check exists so that gets
# caught by a page, not by someone noticing a feature silently isn't live.

set -euo pipefail

REPO_DIR="${HOME}/agentraas"
STATE_FILE="${HOME}/.health-check-state"
ALERT_EMAIL="${ALERT_EMAIL:-sumedhchatse11@gmail.com}"
DISK_THRESHOLD_PCT=85
REQUIRED_CONTAINERS=(ar-api-rs ar-postgres ar-redis ar-minio)

# shellcheck disable=SC1090
set -a; source "${REPO_DIR}/.env"; set +a

failures=()

# ─── Disk space — root filesystem and wherever podman's storage actually lives ───
check_disk() {
  local mount="$1"
  local pct
  pct=$(df -P "${mount}" | awk 'NR==2 {gsub("%","",$5); print $5}')
  if [ "${pct}" -ge "${DISK_THRESHOLD_PCT}" ]; then
    failures+=("Disk at ${mount} is ${pct}% full (threshold ${DISK_THRESHOLD_PCT}%) — podman image prune -f, see docs/kb/07-error-sop.md")
  fi
}
check_disk "/"
graph_root=$(podman info --format '{{.Store.GraphRoot}}' 2>/dev/null || echo "")
if [ -n "${graph_root}" ]; then
  check_disk "${graph_root}"
fi

# ─── Required containers actually running ───
for c in "${REQUIRED_CONTAINERS[@]}"; do
  status=$(podman inspect "${c}" --format '{{.State.Status}}' 2>/dev/null || echo "missing")
  if [ "${status}" != "running" ]; then
    failures+=("Container ${c} is ${status}, not running")
  fi
done
# ar-api (Node) is the deliberate rollback slot, not required to be up —
# reported informationally only, never alerted on.
ar_api_status=$(podman inspect ar-api --format '{{.State.Status}}' 2>/dev/null || echo "missing")

# ─── The live service actually responding ───
health_body=$(curl -s -m 5 http://localhost:13001/health || echo "")
if ! echo "${health_body}" | grep -q '"ok":true'; then
  failures+=("http://localhost:13001/health did not return ok:true — got: ${health_body:-<no response>}")
fi

# ─── Recent panics ───
panic_count=$(podman logs ar-api-rs --since 15m 2>&1 | grep -ic panic || true)
if [ "${panic_count}" -gt 0 ]; then
  failures+=("${panic_count} panic(s) in ar-api-rs logs in the last 15 minutes")
fi

# ─── Decide whether to email, based on the state transition ───
prev_state="ok"
[ -f "${STATE_FILE}" ] && prev_state=$(cat "${STATE_FILE}")

send_mail() {
  local subject="$1" body="$2"
  curl -s --url "smtp://${SMTP_HOST}:${SMTP_PORT}" --ssl-reqd \
    --mail-from "${SMTP_FROM}" --mail-rcpt "${ALERT_EMAIL}" \
    --user "${SMTP_USER}:${SMTP_PASS}" --upload-file - <<EOF
From: ${SMTP_FROM}
To: ${ALERT_EMAIL}
Subject: ${subject}

${body}
(ar-api rollback container: ${ar_api_status})
EOF
}

if [ "${#failures[@]}" -gt 0 ]; then
  if [ "${prev_state}" != "alerting" ]; then
    send_mail "AgentRaaS production — health check FAILED" "$(printf '%s\n' "${failures[@]}")"
  fi
  echo "alerting" > "${STATE_FILE}"
  printf '%s\n' "${failures[@]}" >&2
  exit 1
else
  if [ "${prev_state}" = "alerting" ]; then
    send_mail "AgentRaaS production — recovered" "All checks passing again."
  fi
  echo "ok" > "${STATE_FILE}"
fi

# ─── One-time setup: run every 15 minutes via cron ───
# crontab -e
# Add this line:
# */15 * * * * /home/agentraas/agentraas/infra/scripts/health-check.sh >> /home/agentraas/health-check.log 2>&1
#
# ponytail: email only, no WhatsApp — production already has a real
# WhatsApp Business credential (org "my_team"), but sending an unprompted
# freeform alert through it risks silently failing WhatsApp's 24h
# customer-service-window/template-approval rules, which needs a human
# decision (which number, is it template-approved), not a guess. Add a
# second send_whatsapp() call here via AgentRaaS's own
# `/v1/sdk/whatsapp/message.send` if that's confirmed usable.
