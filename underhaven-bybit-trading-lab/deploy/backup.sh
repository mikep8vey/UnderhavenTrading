#!/usr/bin/env bash
# Consistent SQLite backup via `underhaven backup` (VACUUM INTO) + retention.
# Usage: sudo ./deploy/backup.sh [retention-days]
set -euo pipefail

RETAIN_DAYS="${1:-14}"
BACKUP_DIR=/var/lib/underhaven/backups
STAMP="$(date -u +%Y%m%d-%H%M%S)"

if [[ "$(id -u)" -ne 0 ]]; then
  echo "run as root (sudo) or install a systemd timer running as root" >&2
  exit 1
fi

install -d -o underhaven -g underhaven -m 0700 "$BACKUP_DIR"
runuser -u underhaven -- env \
  UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
  UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
  UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
  /opt/underhaven/bin/underhaven backup --output "$BACKUP_DIR/underhaven-$STAMP.db"

find "$BACKUP_DIR" -type f -name 'underhaven-*.db' -mtime "+$RETAIN_DAYS" -delete
echo "backup complete: $BACKUP_DIR/underhaven-$STAMP.db (retention ${RETAIN_DAYS}d)"
