#!/usr/bin/env bash
# Remove UNDERHAVEN services and configuration.
# Data is preserved unless --purge is passed.
set -euo pipefail

PURGE=0
[[ "${1:-}" == "--purge" ]] && PURGE=1

if [[ "$(id -u)" -ne 0 ]]; then
  echo "run as root (sudo)" >&2
  exit 1
fi

echo "==> stopping services"
systemctl disable --now underhaven 2>/dev/null || true
rm -f /etc/systemd/system/underhaven.service
systemctl daemon-reload

echo "==> removing nginx vhost"
rm -f /etc/nginx/sites-enabled/underhaven
rm -f /etc/nginx/sites-available/underhaven /etc/nginx/sites-available/underhaven.bak
if [[ -f /etc/nginx/sites-enabled/default.underhaven-disabled ]]; then
  mv /etc/nginx/sites-enabled/default.underhaven-disabled /etc/nginx/sites-enabled/default
fi
nginx -t && systemctl reload nginx || true

if [[ "$PURGE" -eq 1 ]]; then
  echo "==> purging data, keys and service account"
  rm -rf /var/lib/underhaven /etc/underhaven /opt/underhaven
  rm -rf /root/.acme.sh/"${UNDERHAVEN_IP:-}"* 2>/dev/null || true
  userdel underhaven 2>/dev/null || true
  echo "PURGED. Let's Encrypt certificates and the database are gone."
else
  echo "Data preserved in /var/lib/underhaven and /etc/underhaven."
  echo "Re-run install.sh to attach a new binary to the existing ledger."
fi
