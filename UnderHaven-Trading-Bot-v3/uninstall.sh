#!/usr/bin/env bash
# UnderHaven full uninstall: removes the service, app, database, encryption key,
# nginx site, HTTPS certificate, service user and (optionally) the git clone, so you
# can `git clone` the latest version and run install.sh from a clean slate.
#
#   sudo bash uninstall.sh              interactive (asks you to type DELETE)
#   sudo bash uninstall.sh --yes        no prompt
#   sudo bash uninstall.sh --dry-run    only print what would be removed
#   sudo bash uninstall.sh --keep-data  keep the database + vault key (trade history, saved wallet)
#   sudo bash uninstall.sh --keep-source  do not delete the folder/git clone this script is in
#
# Works from anywhere after install:  sudo bash /opt/underhaven-trading/uninstall.sh
set -Eeuo pipefail

usage(){ sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; }
if [ "$(id -u)" -ne 0 ]; then echo 'Run as root: sudo bash uninstall.sh'; exit 1; fi

INSTALL_DIR="/opt/underhaven-trading"
DATA_DIR="/var/lib/underhaven-trading"
ETC_DIR="/etc/underhaven-trading"
ACME_DIR="/var/www/underhaven-acme"
SERVICE_USER="underhaven"
CERT_NAME="underhaven-ip"
SRC_DIR="$(cd "$(dirname "$0")" && pwd)"

ASSUME_YES=0; KEEP_DATA=0; KEEP_SOURCE=0; DRY=0
for a in "$@"; do
  case "$a" in
    --yes|-y) ASSUME_YES=1;;
    --keep-data) KEEP_DATA=1;;
    --keep-source) KEEP_SOURCE=1;;
    --dry-run) DRY=1;;
    -h|--help) usage; exit 0;;
    *) echo "Unknown option: $a"; usage; exit 1;;
  esac
done

run(){ if [ "$DRY" -eq 1 ]; then echo "  [dry-run] $*"; else "$@"; fi; }

# Work out which source folder (if any) to delete. If this script lives inside a git
# clone, the whole clone is removed so a fresh `git clone` has nothing to collide with.
SRC_PURGE=""
if [ "$KEEP_SOURCE" -eq 0 ] && [ "$SRC_DIR" != "$INSTALL_DIR" ]; then
  SRC_PURGE="$SRC_DIR"
  if TOP="$(git -C "$SRC_DIR" rev-parse --show-toplevel 2>/dev/null)" && [ -n "$TOP" ]; then SRC_PURGE="$TOP"; fi
  SRC_PURGE="$(cd "$SRC_PURGE" && pwd -P)"
  HOME_DIRS="$HOME /root /home $(getent passwd "${SUDO_USER:-root}" | cut -d: -f6)"
  DEPTH="$(printf '%s' "$SRC_PURGE" | awk -F/ '{print NF-1}')"
  UNSAFE=0
  [ "$DEPTH" -lt 2 ] && UNSAFE=1
  for h in $HOME_DIRS; do [ "$SRC_PURGE" = "$h" ] && UNSAFE=1; done
  case "$SRC_PURGE" in /|/opt|/opt/*|/usr|/usr/*|/var|/var/*|/etc|/etc/*|/bin|/sbin|/lib|/lib/*|/boot|/boot/*|/proc|/sys|/dev) UNSAFE=1;; esac
  if [ "$UNSAFE" -eq 1 ]; then
    echo "NOTE: not deleting source folder '$SRC_PURGE' (looks like a system or home directory)."
    SRC_PURGE=""
  fi
fi

echo "UnderHaven uninstall"
echo "===================="
echo "This will PERMANENTLY remove:"
echo "  - systemd services:  underhaven-trading, underhaven-certbot (+ timer)"
echo "  - application:       $INSTALL_DIR"
echo "  - nginx site:        underhaven-trading"
echo "  - HTTPS certificate: $CERT_NAME (Let's Encrypt)"
echo "  - service user:      $SERVICE_USER"
if [ "$KEEP_DATA" -eq 1 ]; then
  echo "  - KEEPING data:      $DATA_DIR and $ETC_DIR (database, trade history, vault key)"
else
  echo "  - ALL DATA:          $DATA_DIR (database, scan history, trades, saved wallet credentials)"
  echo "                       $ETC_DIR (vault encryption key)"
  echo
  echo "  WARNING: if a wallet is saved in the bot, back up its private key / seed phrase"
  echo "  somewhere safe BEFORE continuing. Once the vault key is deleted, the encrypted"
  echo "  copy in the database cannot be recovered. Use --keep-data to preserve it."
fi
[ -n "$SRC_PURGE" ] && echo "  - source folder:     $SRC_PURGE (the folder/git clone this script is in)"
echo
if [ "$DRY" -eq 1 ]; then echo "(dry run: nothing will actually be changed)"; echo
elif [ "$ASSUME_YES" -ne 1 ]; then
  read -rp 'Type DELETE to continue (anything else cancels): ' ANSWER
  [ "$ANSWER" = "DELETE" ] || { echo 'Cancelled. Nothing was changed.'; exit 1; }
fi

echo "==> Stopping and removing services"
run systemctl disable --now underhaven-trading.service 2>/dev/null || true
run systemctl disable --now underhaven-certbot.timer 2>/dev/null || true
run pkill -u "$SERVICE_USER" 2>/dev/null || true
run rm -f /etc/systemd/system/underhaven-trading.service /etc/systemd/system/underhaven-certbot.service /etc/systemd/system/underhaven-certbot.timer
run systemctl daemon-reload || true

echo "==> Removing nginx site"
run rm -f /etc/nginx/sites-enabled/underhaven-trading /etc/nginx/sites-available/underhaven-trading
if [ "$DRY" -eq 1 ]; then echo "  [dry-run] nginx -t && systemctl reload nginx"
else nginx -t 2>/dev/null && systemctl reload nginx 2>/dev/null || true; fi

echo "==> Removing HTTPS certificate"
if command -v certbot >/dev/null 2>&1; then
  run certbot delete --non-interactive --cert-name "$CERT_NAME" 2>/dev/null || true
  run certbot delete --non-interactive --cert-name "$CERT_NAME-staging" 2>/dev/null || true
fi
run rm -rf "$ACME_DIR"

echo "==> Removing application files"
run rm -rf "$INSTALL_DIR"
run rm -f /tmp/underhaven-health.json
if [ "$KEEP_DATA" -eq 0 ]; then
  run rm -rf "$DATA_DIR" "$ETC_DIR"
fi
if id -u "$SERVICE_USER" >/dev/null 2>&1; then run userdel "$SERVICE_USER" 2>/dev/null || true; fi

echo
echo "UnderHaven has been removed."
echo "Left in place on purpose: nginx, Certbot/snap and firewall rules for ports 80/443 (other things may use them)."
echo "To reinstall the latest version:"
echo "  cd ~ && git clone <your-repo-url> && cd <repo>/UnderHaven-Trading-Bot-v3 && sudo bash install.sh"

# Delete the source folder last, after all output, so nothing needs the script file again.
if [ -n "$SRC_PURGE" ]; then
  cd /
  run rm -rf -- "$SRC_PURGE"
  [ "$DRY" -eq 0 ] && echo "Deleted $SRC_PURGE"
fi
exit 0
