#!/usr/bin/env bash
set -Eeuo pipefail
trap 'echo; echo "ERROR: installation stopped at line $LINENO. Check: journalctl -u underhaven-trading -n 100 --no-pager" >&2' ERR

if [ "$(id -u)" -ne 0 ]; then
  echo 'Run as root: sudo ./install.sh'
  exit 1
fi

SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
INSTALL_DIR="/opt/underhaven-trading"
SERVICE_USER="underhaven"
DATA_DIR="/var/lib/underhaven-trading"
ETC_DIR="/etc/underhaven-trading"
ACME_DIR="/var/www/underhaven-acme"
NGINX_SITE="/etc/nginx/sites-available/underhaven-trading"
CERT_NAME="underhaven-ip"

export DEBIAN_FRONTEND=noninteractive

echo "==> Installing system packages"
apt-get update
apt-get install -y python3 python3-venv python3-pip nginx curl ca-certificates openssl snapd rsync

# Make sure snapd is actually ready before installing the current Certbot.
systemctl enable --now snapd.socket >/dev/null 2>&1 || true
for i in {1..30}; do
  if snap version >/dev/null 2>&1; then break; fi
  sleep 2
done
snap install core >/dev/null 2>&1 || true
snap refresh core >/dev/null 2>&1 || true
if ! snap list certbot >/dev/null 2>&1; then
  snap install --classic certbot
else
  snap refresh certbot
fi
ln -sf /snap/bin/certbot /usr/local/bin/certbot
CERTBOT_VERSION="$(certbot --version 2>&1)"
echo "==> $CERTBOT_VERSION"

# IP-address certificates require Certbot 5.3+; current Let’s Encrypt guidance
# recommends 5.4+ for webroot IP certificates.
CERTBOT_NUM="$(certbot --version 2>&1 | sed -E 's/.*([0-9]+\.[0-9]+).*/\1/' | head -1)"
CERTBOT_OK="$($SRC_DIR/.venv/bin/python - <<'PY' 2>/dev/null || true
PY
)"
python3 - <<PY
import re,sys
s='''$CERTBOT_VERSION'''
m=re.search(r'(\d+)\.(\d+)',s)
if not m or (int(m.group(1)),int(m.group(2))) < (5,4):
    print('Certbot 5.4+ is required for the automated IP-address certificate flow.', file=sys.stderr)
    sys.exit(1)
PY

id -u "$SERVICE_USER" >/dev/null 2>&1 || useradd --system --home "$DATA_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
mkdir -p "$INSTALL_DIR" "$DATA_DIR" "$ETC_DIR" "$ACME_DIR/.well-known/acme-challenge"

# Never run the service from /root. Copy the release to /opt so the systemd user can execute it.
echo "==> Installing UnderHaven into $INSTALL_DIR"
rsync -a --delete --exclude='.venv' --exclude='.env' --exclude='*.db' --exclude='__pycache__' "$SRC_DIR/" "$INSTALL_DIR/"
chown -R "$SERVICE_USER":"$SERVICE_USER" "$INSTALL_DIR" "$DATA_DIR"
chmod 755 "$INSTALL_DIR"
chmod 700 "$DATA_DIR"
chmod 750 "$ETC_DIR"
chown "$SERVICE_USER":"$SERVICE_USER" "$ETC_DIR"
chmod 755 "$ACME_DIR" "$ACME_DIR/.well-known" "$ACME_DIR/.well-known/acme-challenge"

# Ask for the administrator credentials during installation and confirm the password twice.
read -rp 'UnderHaven admin username: ' ADMIN_USER
while [ -z "$ADMIN_USER" ]; do read -rp 'Admin username cannot be empty: ' ADMIN_USER; done
while true; do
  read -rsp 'UnderHaven admin password (min 10 chars): ' ADMIN_PASS; echo
  if [ "${#ADMIN_PASS}" -lt 10 ]; then echo 'Password must be at least 10 characters.'; continue; fi
  read -rsp 'Confirm UnderHaven admin password: ' ADMIN_PASS_CONFIRM; echo
  if [ "$ADMIN_PASS" != "$ADMIN_PASS_CONFIRM" ]; then echo 'Passwords do not match. Try again.'; unset ADMIN_PASS ADMIN_PASS_CONFIRM; continue; fi
  break
done

python3 -m venv "$INSTALL_DIR/.venv"
"$INSTALL_DIR/.venv/bin/pip" install --upgrade pip wheel
"$INSTALL_DIR/.venv/bin/pip" install -r "$INSTALL_DIR/requirements.txt"

if [ ! -f "$ETC_DIR/vault.key" ]; then
  "$INSTALL_DIR/.venv/bin/python" -c "from cryptography.fernet import Fernet; open('$ETC_DIR/vault.key','wb').write(Fernet.generate_key())"
fi
chmod 600 "$ETC_DIR/vault.key"
chown "$SERVICE_USER":"$SERVICE_USER" "$ETC_DIR/vault.key"

SESSION_SECRET="$(openssl rand -hex 32)"
cat > "$INSTALL_DIR/.env" <<ENV
UNDERHAVEN_DATA_DIR=$DATA_DIR
UNDERHAVEN_VAULT_KEY_FILE=$ETC_DIR/vault.key
POLYMARKET_CLOB_URL=https://clob.polymarket.com
POLYMARKET_GAMMA_URL=https://gamma-api.polymarket.com
POLYMARKET_CHAIN_ID=137
UNDERHAVEN_HOST=127.0.0.1
UNDERHAVEN_PORT=5000
UNDERHAVEN_SESSION_SECRET=$SESSION_SECRET
ENV
chmod 600 "$INSTALL_DIR/.env"
chown "$SERVICE_USER":"$SERVICE_USER" "$INSTALL_DIR/.env"

# Initialize the database AS THE SERVICE USER.
# Important: SQLite needs write permission on both the database file and its parent
# directory (especially for WAL/-shm/-wal files). Initializing it as root would
# make the later systemd process fail with "attempt to write a readonly database".
chown -R "$SERVICE_USER":"$SERVICE_USER" "$DATA_DIR" "$ETC_DIR"
chmod 700 "$DATA_DIR"

UH_ADMIN_USER="$ADMIN_USER" UH_ADMIN_PASS="$ADMIN_PASS" runuser -u "$SERVICE_USER" -- env \
  UH_ADMIN_USER="$ADMIN_USER" UH_ADMIN_PASS="$ADMIN_PASS" \
  "$INSTALL_DIR/.venv/bin/python" - <<PY
import os, sys
sys.path.insert(0, '$INSTALL_DIR')
from werkzeug.security import generate_password_hash
from app import db
db.set_setting('admin_user', os.environ['UH_ADMIN_USER'])
db.set_setting('admin_hash', generate_password_hash(os.environ['UH_ADMIN_PASS']))
db.set_setting('min_net_edge','0.01')
db.set_setting('paper_size','10')
db.set_setting('live_size','10')
db.set_setting('max_daily_loss','5')
db.set_setting('paper_cooldown','30')
db.set_setting('auto_merge','1')
db.set_setting('live_enabled','0')
PY

# The installer itself runs as root, but the application must own its writable
# state. Re-apply ownership after initialization so upgrades/reinstalls remain safe.
chown -R "$SERVICE_USER":"$SERVICE_USER" "$DATA_DIR" "$ETC_DIR"
chmod 700 "$DATA_DIR"
unset ADMIN_PASS ADMIN_PASS_CONFIRM

# Detect public IPv4.
PUBLIC_IP="$(curl -4 -fsS --max-time 15 https://api.ipify.org || true)"
if [[ ! "$PUBLIC_IP" =~ ^[0-9]+(\.[0-9]+){3}$ ]]; then
  read -rp 'Could not detect public IPv4. Enter the VPS public IPv4: ' PUBLIC_IP
fi
if [[ ! "$PUBLIC_IP" =~ ^[0-9]+(\.[0-9]+){3}$ ]]; then echo 'Invalid IPv4 address.'; exit 1; fi
echo "Detected public IPv4: $PUBLIC_IP"

# If UFW is active, allow only the web ports required by this app.
if command -v ufw >/dev/null 2>&1 && ufw status | grep -q 'Status: active'; then
  ufw allow 80/tcp >/dev/null
  ufw allow 443/tcp >/dev/null
fi

# HTTP-only Nginx configuration. The ACME location is explicit and uses ^~ so it
# can never be swallowed by the application proxy.
cat > "$NGINX_SITE" <<NGINX
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;

    # Use alias for ACME files. This maps the challenge URL directly to the
    # challenge directory and avoids root/try_files path-resolution surprises.
    location ^~ /.well-known/acme-challenge/ {
        alias $ACME_DIR/.well-known/acme-challenge/;
        default_type text/plain;
        add_header Cache-Control "no-store" always;
    }

    location / {
        proxy_pass http://127.0.0.1:5000;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
    }
}
NGINX
ln -sf "$NGINX_SITE" /etc/nginx/sites-enabled/underhaven-trading
rm -f /etc/nginx/sites-enabled/default
nginx -t
systemctl enable --now nginx
systemctl reload nginx

# Preflight the exact HTTP-01 path before asking Let's Encrypt for a certificate.
PROBE_TOKEN="underhaven-$(openssl rand -hex 8)"
mkdir -p "$ACME_DIR/.well-known/acme-challenge"
printf '%s' "$PROBE_TOKEN" > "$ACME_DIR/.well-known/acme-challenge/$PROBE_TOKEN"
chmod 644 "$ACME_DIR/.well-known/acme-challenge/$PROBE_TOKEN"
sleep 1
# Test locally with the public Host header first. This distinguishes an Nginx
# location problem from an upstream provider/firewall problem.
LOCAL_PROBE="$(curl -4 -fsS --max-time 10 -H "Host: $PUBLIC_IP" "http://127.0.0.1/.well-known/acme-challenge/$PROBE_TOKEN" || true)"
PROBE="$(curl -4 -fsS --max-time 10 "http://$PUBLIC_IP/.well-known/acme-challenge/$PROBE_TOKEN" || true)"
if [ "$LOCAL_PROBE" != "$PROBE_TOKEN" ]; then
  echo
  echo 'ERROR: ACME local Nginx preflight failed.'
  echo 'The VPS itself cannot serve the challenge file through Nginx.'
  nginx -T 2>&1 | tail -n 120 || true
  rm -f "$ACME_DIR/.well-known/acme-challenge/$PROBE_TOKEN"
  exit 1
fi
if [ "$PROBE" != "$PROBE_TOKEN" ]; then
  echo
  echo 'ERROR: ACME public HTTP preflight failed.'
  echo 'Nginx serves the challenge locally, but the public IPv4 path does not.'
  echo 'This normally means TCP/80 is blocked by the VPS provider/network firewall or the public IP is not mapped to this VPS.'
  echo "Local test:  curl -4 -H 'Host: $PUBLIC_IP' http://127.0.0.1/.well-known/acme-challenge/$PROBE_TOKEN"
  echo "Public test: curl -4 http://$PUBLIC_IP/.well-known/acme-challenge/$PROBE_TOKEN"
  rm -f "$ACME_DIR/.well-known/acme-challenge/$PROBE_TOKEN"
  exit 1
fi
rm -f "$ACME_DIR/.well-known/acme-challenge/$PROBE_TOKEN"
echo '==> ACME HTTP challenge preflight passed locally and publicly.'

# Ask for the IP certificate. The staging pass catches webroot/network problems
# without consuming the production certificate issuance path.
echo '==> Testing Let’s Encrypt IP certificate issuance (staging)'
certbot certonly --staging --non-interactive --agree-tos --register-unsafely-without-email \
  --webroot -w "$ACME_DIR" --preferred-profile shortlived --preferred-challenges http \
  --cert-name "$CERT_NAME-staging" --ip-address "$PUBLIC_IP"
rm -rf "/etc/letsencrypt/live/$CERT_NAME-staging" "/etc/letsencrypt/archive/$CERT_NAME-staging" "/etc/letsencrypt/renewal/$CERT_NAME-staging.conf" || true

echo '==> Requesting trusted Let’s Encrypt IP certificate'
certbot certonly --non-interactive --agree-tos --register-unsafely-without-email \
  --webroot -w "$ACME_DIR" --preferred-profile shortlived --preferred-challenges http \
  --cert-name "$CERT_NAME" --ip-address "$PUBLIC_IP"

# HTTPS config. Keep the ACME path on port 80 for every renewal.
cat > "$NGINX_SITE" <<NGINX
server {
    listen 80 default_server;
    listen [::]:80 default_server;
    server_name _;
    location ^~ /.well-known/acme-challenge/ {
        alias $ACME_DIR/.well-known/acme-challenge/;
        default_type text/plain;
        add_header Cache-Control "no-store" always;
    }
    location / { return 301 https://\$host\$request_uri; }
}
server {
    listen 443 ssl default_server;
    listen [::]:443 ssl default_server;
    server_name $PUBLIC_IP;
    ssl_certificate /etc/letsencrypt/live/$CERT_NAME/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/$CERT_NAME/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;
    client_max_body_size 2m;
    location / {
        proxy_pass http://127.0.0.1:5000;
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_read_timeout 60s;
    }
}
NGINX
nginx -t
systemctl reload nginx

# Long-running application service. The dashboard is NOT the process that keeps
# the bot alive; this systemd service is. Closing the browser does not stop it.
cat > /etc/systemd/system/underhaven-trading.service <<SERVICE
[Unit]
Description=UnderHaven Trading Bot
After=network-online.target nginx.service
Wants=network-online.target

[Service]
Type=simple
User=$SERVICE_USER
Group=$SERVICE_USER
WorkingDirectory=$INSTALL_DIR
EnvironmentFile=$INSTALL_DIR/.env
ExecStart=$INSTALL_DIR/.venv/bin/python $INSTALL_DIR/run.py
Restart=always
RestartSec=5
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
SERVICE

# Renewal hook is explicit because IP certificates are short-lived and Nginx must
# reload the renewed certificate. The official Let’s Encrypt docs call out this need.
cat > /etc/systemd/system/underhaven-certbot.service <<SERVICE
[Unit]
Description=Renew UnderHaven IP TLS certificate
After=network-online.target nginx.service
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/certbot renew --quiet --deploy-hook /usr/bin/systemctl\ reload\ nginx
SERVICE
cat > /etc/systemd/system/underhaven-certbot.timer <<SERVICE
[Unit]
Description=Twice daily UnderHaven IP certificate renewal

[Timer]
OnBootSec=10min
OnUnitActiveSec=12h
Persistent=true

[Install]
WantedBy=timers.target
SERVICE

systemctl daemon-reload
systemctl enable --now underhaven-trading.service
systemctl enable --now underhaven-certbot.timer

# Do not report a successful installation while the dashboard backend is crashing.
# Wait briefly for Flask to become reachable through the local Nginx upstream.
READY=0
for i in $(seq 1 20); do
  if curl -fsS --max-time 2 http://127.0.0.1:5000/health >/tmp/underhaven-health.json 2>/dev/null; then
    READY=1
    break
  fi
  sleep 1
done

if [ "$READY" -ne 1 ]; then
  echo
  echo 'ERROR: UnderHaven service did not become ready.'
  echo 'The SSL certificate is installed, but the application backend is not healthy.'
  echo
  systemctl --no-pager --full status underhaven-trading.service || true
  echo
  echo 'Recent application log:'
  journalctl -u underhaven-trading.service -n 80 --no-pager || true
  echo
  echo 'Database permissions:'
  ls -lad "$DATA_DIR" || true
  ls -la "$DATA_DIR" || true
  exit 1
fi

echo "==> UnderHaven backend health check passed: $(cat /tmp/underhaven-health.json)"

printf '\n===============================================\n'
printf ' UNDERHAVEN TRADING BOT INSTALLED\n'
printf '===============================================\n'
printf 'HTTP :  http://%s\n' "$PUBLIC_IP"
printf 'HTTPS: https://%s\n' "$PUBLIC_IP"
printf '\nPAPER mode is running continuously on the VPS.\n'
printf 'LIVE trading is OFF until you enable it in the dashboard.\n'
printf 'Let’s Encrypt IP certificate: installed + automatic renewal enabled.\n'
printf 'Admin user: %s\n' "$ADMIN_USER"
printf '\nService: systemctl status underhaven-trading\n'
printf 'Logs:    journalctl -u underhaven-trading -f\n'
printf 'Cert:    systemctl status underhaven-certbot.timer\n'
printf 'Data:    %s\n' "$DATA_DIR"
printf '===============================================\n'
