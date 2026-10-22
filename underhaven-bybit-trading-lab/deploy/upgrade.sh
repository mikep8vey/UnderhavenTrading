#!/usr/bin/env bash
# Atomic binary upgrade with a pre-upgrade database backup and health check.
# Usage: sudo ./deploy/upgrade.sh /path/to/new/underhaven
set -euo pipefail

BIN="${1:-${UNDERHAVEN_BIN:-}}"
if [[ -z "$BIN" || ! -f "$BIN" ]]; then
  echo "usage: $0 /path/to/underhaven-binary" >&2
  exit 2
fi
if [[ "$(id -u)" -ne 0 ]]; then
  echo "run as root (sudo)" >&2
  exit 1
fi

BACKUP_DIR=/var/lib/underhaven/backups
install -d -o underhaven -g underhaven -m 0700 "$BACKUP_DIR"
STAMP="$(date -u +%Y%m%d-%H%M%S)"
echo "==> pre-upgrade backup"
runuser -u underhaven -- env \
  UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
  UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
  UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
  /opt/underhaven/bin/underhaven backup --output "$BACKUP_DIR/pre-upgrade-$STAMP.db"

echo "==> installing new binary"
install -o root -g root -m 0755 "$BIN" /opt/underhaven/bin/underhaven.new
mv -f /opt/underhaven/bin/underhaven.new /opt/underhaven/bin/underhaven

systemctl restart underhaven
for _ in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
    echo "==> upgrade healthy"
    systemctl --no-pager --lines=5 status underhaven || true
    exit 0
  fi
  sleep 1
done

echo "ERROR: new binary did not become healthy." >&2
echo "Roll back the binary and inspect: journalctl -u underhaven -n 150" >&2
echo "The pre-upgrade database backup is $BACKUP_DIR/pre-upgrade-$STAMP.db" >&2
exit 1
