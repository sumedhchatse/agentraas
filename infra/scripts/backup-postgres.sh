#!/bin/bash
# backup-postgres.sh — dumps the AgentRaaS database and prunes backups older than 14 days.
# Run manually, or add to cron (see setup instructions below).

set -euo pipefail

BACKUP_DIR="${HOME}/agentraas-backups"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
FILENAME="agentraas_${TIMESTAMP}.sql.gz"
RETENTION_DAYS=14

mkdir -p "${BACKUP_DIR}"

echo "Backing up agentraas database..."
podman exec ar-postgres pg_dump -U agentraas -d agentraas | gzip > "${BACKUP_DIR}/${FILENAME}"

if [ -s "${BACKUP_DIR}/${FILENAME}" ]; then
  echo "Backup saved: ${BACKUP_DIR}/${FILENAME} ($(du -h "${BACKUP_DIR}/${FILENAME}" | cut -f1))"
else
  echo "ERROR: backup file is empty — something went wrong." >&2
  rm -f "${BACKUP_DIR}/${FILENAME}"
  exit 1
fi

echo "Pruning backups older than ${RETENTION_DAYS} days..."
find "${BACKUP_DIR}" -name "agentraas_*.sql.gz" -mtime +${RETENTION_DAYS} -delete

echo "Done. Current backups:"
ls -lh "${BACKUP_DIR}"

# ─── One-time setup: run daily at 3am via cron ───
# crontab -e
# Add this line:
# 0 3 * * * /home/sumedh/agentraas/infra/scripts/backup-postgres.sh >> /home/sumedh/agentraas-backups/backup.log 2>&1
#
# To restore from a backup:
# gunzip -c /path/to/agentraas_TIMESTAMP.sql.gz | podman exec -i ar-postgres psql -U agentraas -d agentraas
