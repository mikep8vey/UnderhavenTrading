#!/usr/bin/env bash
# Install or upgrade weather-bot on Ubuntu 22.04/24.04.  Run as root from the repo root:
#   sudo bash scripts/install.sh
# Non-interactive: ADMIN_USER=... ADMIN_PASS=... DOMAIN=example.com EMAIL=me@x.com sudo -E bash scripts/install.sh
# Skip the dashboard/HTTPS part: SKIP_DASHBOARD=1
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo bash scripts/install.sh"; exit 1; }
cd "$(dirname "$0")/.."
CFG=/etc/weather-bot/config
ADMIN_FILE=/etc/weather-bot/admin.json

echo "== 1/5 system packages =="
apt-get update -y
DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential pkg-config libssl-dev sqlite3 curl openssl nginx

echo "== 2/5 build =="
if ! command -v cargo >/dev/null 2>&1 && [ ! -f "$HOME/.cargo/env" ]; then
  curl https://sh.rustup.rs -sSf | sh -s -- -y
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
cargo build --release
systemctl stop underhaven-weather 2>/dev/null || true
install -m 0755 target/release/weather-bot /usr/local/bin/weather-bot

echo "== 3/5 config =="
mkdir -p /var/lib/weather-bot/exports "$CFG"
for f in config/*.toml; do
  b="$(basename "$f")"
  [ -f "$CFG/$b" ] || cp "$f" "$CFG/$b"
done
# migrate an existing risk.toml: Coinbase for the crypto guard (Binance blocks many VPS regions), dashboard section
sed -i -E 's|^crypto_url = .*|crypto_url = "https://api.exchange.coinbase.com/products"|; s|^crypto_symbols = .*|crypto_symbols = ["BTC-USD", "ETH-USD", "XRP-USD"]|' "$CFG/risk.toml"
grep -q '^\[dashboard\]' "$CFG/risk.toml" || printf '\n[dashboard]\nenabled = true\nbind = "127.0.0.1:8787"\n' >> "$CFG/risk.toml"
if [ ! -f /etc/weather-bot/env ]; then
  cat > /etc/weather-bot/env <<'EOF'
POLYMARKET_PRIVATE_KEY=
POLYMARKET_FUNDER_ADDRESS=
POLYMARKET_SIGNATURE_TYPE=0
WEATHER_BOT_CONFIG_DIR=/etc/weather-bot/config
WEATHER_BOT_DB=/var/lib/weather-bot/weather-bot.db
WEATHER_BOT_EXPORT_DIR=/var/lib/weather-bot/exports
RUST_LOG=info
EOF
  chmod 600 /etc/weather-bot/env
fi

install -m 0644 scripts/underhaven-weather.service /etc/systemd/system/underhaven-weather.service
systemctl daemon-reload

URL=""
if [ "${SKIP_DASHBOARD:-0}" != "1" ]; then
  echo "== 4/5 dashboard login =="
  ADMIN_USER="${ADMIN_USER:-}"
  ADMIN_PASS="${ADMIN_PASS:-}"
  KEEP=0
  if [ -f "$ADMIN_FILE" ] && [ -z "$ADMIN_PASS" ]; then
    read -r -p "An admin login already exists. Keep it? [Y/n] " ans
    case "${ans:-Y}" in n|N) KEEP=0 ;; *) KEEP=1 ;; esac
  fi
  if [ "$KEEP" -eq 0 ]; then
    [ -n "$ADMIN_USER" ] || read -r -p "Choose an admin username: " ADMIN_USER
    if [ -z "$ADMIN_PASS" ]; then
      while true; do
        read -r -s -p "Choose an admin password (min 12 chars): " ADMIN_PASS; echo
        read -r -s -p "Repeat password: " P2; echo
        [ "$ADMIN_PASS" = "$P2" ] || { echo "passwords differ"; continue; }
        [ "${#ADMIN_PASS}" -ge 12 ] || { echo "too short"; continue; }
        break
      done
    fi
    printf '%s\n' "$ADMIN_PASS" | WEATHER_BOT_CONFIG_DIR="$CFG" /usr/local/bin/weather-bot set-admin --user "$ADMIN_USER" --password-stdin
    unset ADMIN_PASS P2
  fi

  echo "== 5/5 HTTPS (nginx) =="
  DOMAIN="${DOMAIN-__ask__}"; EMAIL="${EMAIL:-}"
  if [ "$DOMAIN" = "__ask__" ]; then
    read -r -p "Domain name pointing at this server (blank = self-signed certificate on the IP): " DOMAIN
  fi
  if [ -n "$DOMAIN" ] && [ -z "$EMAIL" ]; then read -r -p "Email for Let's Encrypt notices: " EMAIL; fi

  # retire the old standalone python dashboard if it was installed
  systemctl disable --now underhaven-dashboard 2>/dev/null || true
  rm -f /etc/systemd/system/underhaven-dashboard.service

  if [ -n "$DOMAIN" ]; then
    DEBIAN_FRONTEND=noninteractive apt-get install -y certbot
    SERVER_NAME="$DOMAIN"
    systemctl stop nginx || true
    certbot certonly --standalone -d "$DOMAIN" -m "$EMAIL" --agree-tos --non-interactive \
      --pre-hook "systemctl stop nginx" --post-hook "systemctl start nginx"
    CERT="/etc/letsencrypt/live/$DOMAIN/fullchain.pem"
    KEY="/etc/letsencrypt/live/$DOMAIN/privkey.pem"
  else
    IP="$(curl -fsS --max-time 5 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')"
    SERVER_NAME="$IP"
    CERT="/etc/ssl/certs/weather-dash.crt"
    KEY="/etc/ssl/private/weather-dash.key"
    openssl req -x509 -nodes -newkey rsa:2048 -days 825 -keyout "$KEY" -out "$CERT" \
      -subj "/CN=$IP" -addext "subjectAltName=IP:$IP" 2>/dev/null
    chmod 600 "$KEY"
  fi

  cat > /etc/nginx/sites-available/weather-dashboard <<'EOF'
limit_req_zone $binary_remote_addr zone=wb_login:10m rate=10r/m;
limit_req_zone $binary_remote_addr zone=wb_api:10m rate=120r/m;

server {
    listen 80 default_server;
    server_name _;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl default_server;
    server_name __SERVER_NAME__;
    ssl_certificate     __CERT__;
    ssl_certificate_key __KEY__;
    ssl_protocols TLSv1.2 TLSv1.3;
    add_header Strict-Transport-Security "max-age=31536000" always;

    location = /api/login {
        limit_req zone=wb_login burst=5 nodelay;
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
    }
    location / {
        limit_req zone=wb_api burst=60 nodelay;
        proxy_pass http://127.0.0.1:8787;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
    }
}
EOF
  sed -i "s|__SERVER_NAME__|$SERVER_NAME|; s|__CERT__|$CERT|; s|__KEY__|$KEY|" /etc/nginx/sites-available/weather-dashboard
  ln -sf /etc/nginx/sites-available/weather-dashboard /etc/nginx/sites-enabled/weather-dashboard
  rm -f /etc/nginx/sites-enabled/default
  nginx -t
  systemctl enable nginx
  systemctl restart nginx
  if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then ufw allow 80,443/tcp; fi
  URL="https://$SERVER_NAME/"
fi

systemctl enable underhaven-weather
systemctl restart underhaven-weather
sleep 4
systemctl --no-pager status underhaven-weather | head -5 || true
echo
echo "weather-bot is running in PAPER mode (dry-run)."
echo "logs:      journalctl -u underhaven-weather -f"
if [ -n "$URL" ]; then
  echo "dashboard: $URL"
  case "$URL" in https://[0-9]*) echo "           (self-signed certificate: the browser warns once; use a domain for a trusted cert)";; esac
  echo "           if it does not load, open ports 80 and 443 in your VPS provider's firewall."
fi
