#!/usr/bin/env bash
set -Eeuo pipefail
if [ "$(id -u)" -ne 0 ]; then echo 'Run as root: sudo ./uninstall.sh'; exit 1; fi
systemctl disable --now underhaven-trading.service 2>/dev/null || true
systemctl disable --now underhaven-certbot.timer 2>/dev/null || true
rm -f /etc/systemd/system/underhaven-trading.service /etc/systemd/system/underhaven-certbot.service /etc/systemd/system/underhaven-certbot.timer
rm -f /etc/nginx/sites-enabled/underhaven-trading /etc/nginx/sites-available/underhaven-trading
systemctl daemon-reload
nginx -t && systemctl reload nginx || true
rm -rf /opt/underhaven-trading
if id -u underhaven >/dev/null 2>&1; then userdel underhaven 2>/dev/null || true; fi
echo 'UnderHaven services, application files, and nginx configuration removed.'
echo 'Data remains under /var/lib/underhaven-trading.'
echo 'The Let’s Encrypt certificate remains under /etc/letsencrypt until you remove it manually.'
