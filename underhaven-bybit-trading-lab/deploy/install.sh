#!/usr/bin/env bash
# Idempotent Ubuntu 22.04/24.04 installer for UNDERHAVEN BYBIT TRADING LAB.
#
# This script installs a prebuilt release binary. Build it with
# `cargo build --release` on a compatible host first.
#
# Usage:
#   sudo ./deploy/install.sh --ip 203.0.113.10 [--bin ./target/release/underhaven]
#   sudo ./deploy/install.sh --domain lab.example.com --bin /tmp/underhaven
#
# It never enables a live trading mode; the application has no such mode.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IP="${UNDERHAVEN_IP:-}"
DOMAIN=""
BIN="${UNDERHAVEN_BIN:-$SCRIPT_DIR/../target/release/underhaven}"
ADMIN_USER="admin"
OPEN_FIREWALL=0
SKIP_TLS=0

usage() {
  sed -n '2,14p' "$0"
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ip) IP="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --bin) BIN="$2"; shift 2 ;;
    --admin-user) ADMIN_USER="$2"; shift 2 ;;
    --open-firewall) OPEN_FIREWALL=1; shift ;;
    --no-tls) SKIP_TLS=1; shift ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if [[ "$(id -u)" -ne 0 ]]; then
  echo "run as root (sudo)" >&2
  exit 1
fi
if ! command -v systemctl >/dev/null 2>&1; then
  echo "systemd is required" >&2
  exit 1
fi
if [[ ! -f "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  echo "build with: cargo build --release" >&2
  exit 1
fi
if [[ -z "$IP" && -z "$DOMAIN" ]]; then
  IP="$(curl -4 -fsS --max-time 8 https://api.ipify.org || true)"
fi
PUBLIC_NAME="${DOMAIN:-$IP}"
if [[ -z "$PUBLIC_NAME" ]]; then
  echo "provide --ip or --domain" >&2
  exit 2
fi
PUBLIC_URL="https://$PUBLIC_NAME"

echo "==> installing dependencies"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq nginx openssl curl ca-certificates cron

echo "==> creating service account and directories"
if ! id underhaven >/dev/null 2>&1; then
  useradd --system --home-dir /opt/underhaven --shell /usr/sbin/nologin underhaven
fi
install -d -o underhaven -g underhaven -m 0750 /opt/underhaven /opt/underhaven/bin
install -d -o underhaven -g underhaven -m 0700 /var/lib/underhaven
# The directory is traversable so nginx can reach its TLS material, but it is
# not listable by unprivileged users. Secrets inside are group-restricted.
install -d -o root -g root -m 0755 /etc/underhaven
# nginx reads the certificate and private key from here. `www-data` is the
# Ubuntu nginx worker group; override NGINX_GROUP on a non-standard image.
NGINX_GROUP="${NGINX_GROUP:-www-data}"
install -d -o root -g "$NGINX_GROUP" -m 0750 /etc/underhaven/tls
install -d -m 0755 /var/www/letsencrypt/.well-known/acme-challenge

echo "==> installing binary atomically"
install -o root -g root -m 0755 "$BIN" /opt/underhaven/bin/underhaven.new
mv -f /opt/underhaven/bin/underhaven.new /opt/underhaven/bin/underhaven

if [[ ! -f /etc/underhaven/master.key ]]; then
  echo "==> generating AES master key outside SQLite"
  openssl rand -out /etc/underhaven/master.key 32
  chown root:underhaven /etc/underhaven/master.key
  chmod 0640 /etc/underhaven/master.key
fi

if [[ ! -f /etc/underhaven/underhaven.env ]]; then
  echo "==> writing /etc/underhaven/underhaven.env"
  sed -e "s#UNDERHAVEN_PUBLIC_URL=.*#UNDERHAVEN_PUBLIC_URL=$PUBLIC_URL#" \
      -e "s#UNDERHAVEN_TLS_MODE=.*#UNDERHAVEN_TLS_MODE=unknown#" \
      "$SCRIPT_DIR/underhaven.env.example" > /etc/underhaven/underhaven.env
  chown root:underhaven /etc/underhaven/underhaven.env
  chmod 0640 /etc/underhaven/underhaven.env
fi

echo "==> installing systemd unit"
install -o root -g root -m 0644 "$SCRIPT_DIR/underhaven.service" /etc/systemd/system/underhaven.service
systemctl daemon-reload
systemctl enable underhaven >/dev/null

echo "==> configuring nginx bootstrap vhost"
if [[ -f /etc/nginx/sites-enabled/default ]]; then
  mv /etc/nginx/sites-enabled/default /etc/nginx/sites-enabled/default.underhaven-disabled
fi
install -o root -g root -m 0644 "$SCRIPT_DIR/nginx-http-bootstrap.conf" /etc/nginx/sites-available/underhaven
ln -sfn /etc/nginx/sites-available/underhaven /etc/nginx/sites-enabled/underhaven
nginx -t
systemctl enable nginx >/dev/null
systemctl restart nginx

TLS_MODE="none"
if [[ "$SKIP_TLS" -eq 0 ]]; then
  echo "==> obtaining Let's Encrypt short-lived certificate for $PUBLIC_NAME"
  ACME="/root/.acme.sh/acme.sh"
  if [[ ! -x "$ACME" ]]; then
    curl -fsS https://get.acme.sh | sh -s email="admin@$PUBLIC_NAME" >/dev/null || true
  fi
  if [[ -x "$ACME" ]] && "$SCRIPT_DIR/acme-shortlived.sh" "$PUBLIC_NAME"; then
    TLS_MODE="letsencrypt"
    install -o root -g root -m 0644 "$SCRIPT_DIR/nginx-underhaven.conf" /etc/nginx/sites-available/underhaven
  else
    echo "WARNING: Let's Encrypt issuance failed; falling back to self-signed TLS."
    TLS_MODE="self_signed"
    "$SCRIPT_DIR/selfsigned.sh" "$PUBLIC_NAME"
    install -o root -g root -m 0644 "$SCRIPT_DIR/nginx-underhaven-selfsigned.conf" /etc/nginx/sites-available/underhaven
  fi
else
  echo "==> --no-tls selected; only the HTTP bootstrap vhost is configured"
  TLS_MODE="none"
fi

sed -i "s/^UNDERHAVEN_TLS_MODE=.*/UNDERHAVEN_TLS_MODE=$TLS_MODE/" /etc/underhaven/underhaven.env
nginx -t
systemctl reload nginx

echo "==> starting the paper engine"
systemctl restart underhaven
for _ in $(seq 1 30); do
  if curl -fsS http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! curl -fsS http://127.0.0.1:8080/healthz >/dev/null 2>&1; then
  echo "service did not become healthy; inspect: journalctl -u underhaven -n 100" >&2
  exit 1
fi

if [[ "$OPEN_FIREWALL" -eq 1 ]] && command -v ufw >/dev/null 2>&1; then
  ufw allow 80/tcp >/dev/null || true
  ufw allow 443/tcp >/dev/null || true
fi

echo "==> creating the initial administrator"
echo "The password is read from a terminal or UNDERHAVEN_ADMIN_PASSWORD."
if [[ -t 0 || -n "${UNDERHAVEN_ADMIN_PASSWORD:-}" ]]; then
  set +e
  runuser -u underhaven -- env \
    UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
    UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
    UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
    /opt/underhaven/bin/underhaven init-admin --username "$ADMIN_USER"
  INIT_STATUS=$?
  set -e
  if [[ $INIT_STATUS -ne 0 ]]; then
    echo "admin was not created (it may already exist). To reset deliberately:"
    echo "  sudo runuser -u underhaven -- /opt/underhaven/bin/underhaven init-admin --username $ADMIN_USER --force"
  fi
else
  echo "non-interactive install: create the admin later with"
  echo "  sudo -u underhaven env UNDERHAVEN_DATA_DIR=/var/lib/underhaven \\"
  echo "    UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \\"
  echo "    UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \\"
  echo "    /opt/underhaven/bin/underhaven init-admin --username $ADMIN_USER"
fi

cat <<EOF

UNDERHAVEN is installed and running in PAPER-ONLY mode.

  URL:        $PUBLIC_URL
  TLS mode:   $TLS_MODE
  Bind:       127.0.0.1:8080 behind nginx
  Database:   /var/lib/underhaven/underhaven.db
  Master key: /etc/underhaven/master.key (outside SQLite, root:underhaven 0640)

Service commands:
  systemctl status underhaven
  journalctl -u underhaven -f
  systemctl restart underhaven
  systemctl stop underhaven

Certificate check:
  sudo -u underhaven env UNDERHAVEN_PUBLIC_URL=$PUBLIC_URL \\
    /opt/underhaven/bin/underhaven tls-check

If TLS mode is self_signed, the browser warning is expected: the fallback
certificate is not publicly trusted. See docs/DEPLOY.md.
EOF
