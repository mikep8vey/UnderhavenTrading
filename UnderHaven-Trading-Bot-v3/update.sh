#!/usr/bin/env bash
set -Eeuo pipefail
if [ "$(id -u)" -ne 0 ]; then echo 'Run as root: sudo ./update.sh'; exit 1; fi
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="/opt/underhaven-trading"
SERVICE_USER=underhaven
if [ ! -d "$INSTALL_DIR" ]; then echo 'UnderHaven is not installed at /opt/underhaven-trading. Run install.sh first.'; exit 1; fi
systemctl stop underhaven-trading.service || true
rsync -a --delete --exclude='.venv' --exclude='.env' --exclude='*.db' --exclude='__pycache__' "$SRC_DIR/" "$INSTALL_DIR/"
"$INSTALL_DIR/.venv/bin/pip" install -r "$INSTALL_DIR/requirements.txt"
chown -R "$SERVICE_USER":"$SERVICE_USER" "$INSTALL_DIR"
chmod 600 "$INSTALL_DIR/.env"
systemctl daemon-reload
systemctl restart underhaven-trading.service
systemctl reload nginx
systemctl --no-pager status underhaven-trading.service
