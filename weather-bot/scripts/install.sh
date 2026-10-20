#!/usr/bin/env bash
# Install or upgrade weather-bot on Ubuntu 22.04/24.04.  Run as root from the repo root:
#   sudo bash scripts/install.sh
# Non-interactive: ADMIN_USER=.. ADMIN_PASS=.. DOMAIN=example.com EMAIL=me@x.com sudo -E bash scripts/install.sh
# Skip the dashboard/HTTPS part: SKIP_DASHBOARD=1
set -euo pipefail
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo bash scripts/install.sh"; exit 1; }
cd "$(dirname "$0")/.."
CFG=/etc/weather-bot/config
ADMIN_FILE=/etc/weather-bot/admin.json

echo "== 1/5 system packages =="
apt-get update -y
DEBIAN_FRONTEND=noninteractive apt-get install -y build-essential pkg-config libssl-dev sqlite3 curl openssl

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
R="$CFG/risk.toml"
# migrate an existing risk.toml: Coinbase crypto guard (Binance blocks many VPS regions) and the newer settings
sed -i -E 's|^crypto_url = .*|crypto_url = "https://api.exchange.coinbase.com/products"|; s|^crypto_symbols = .*|crypto_symbols = ["BTC-USD", "ETH-USD", "XRP-USD"]|' "$R"
grep -q '^daily_loss_cap_pct' "$R" || sed -i '1i daily_loss_cap_pct = 0.05\nmax_attempts_per_market = 3\nretry_cooldown_secs = 900\nlive_bankroll_usdc = 0.0' "$R"
grep -q '^shrink_weight' "$R" || sed -i '/^\[forecast\]/a shrink_weight = 0.3\ncalibration_min_samples = 100\ncalibration_prior = 20.0\nno_max_yes_mid = 0.30\nmin_entry_price = 0.05' "$R"
grep -q '^\[dashboard\]' "$R" || printf '\n[dashboard]\nenabled = true\nbind = "127.0.0.1:8787"\n' >> "$R"
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

HOST=""
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

  echo "== 5/5 HTTPS: Caddy gets and renews a browser-trusted certificate automatically =="
  DOMAIN="${DOMAIN-__ask__}"
  EMAIL="${EMAIL:-}"
  if [ "$DOMAIN" = "__ask__" ]; then
    read -r -p "Your domain (A record must point at this server). Blank = free automatic hostname from your IP (sslip.io): " DOMAIN
  fi
  IP="$(curl -4 -fsS --max-time 8 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')"
  if [ -n "$DOMAIN" ]; then HOST="$DOMAIN"; else HOST="${IP//./-}.sslip.io"; fi

  DEBIAN_FRONTEND=noninteractive apt-get install -y caddy
  # retire nginx / the old standalone python dashboard so ports 80 and 443 are free for Caddy
  systemctl disable --now nginx 2>/dev/null || true
  rm -f /etc/nginx/sites-enabled/weather-dashboard
  systemctl disable --now underhaven-dashboard 2>/dev/null || true
  rm -f /etc/systemd/system/underhaven-dashboard.service
  systemctl daemon-reload

  {
    if [ -n "$EMAIL" ]; then printf '{\n\temail %s\n}\n\n' "$EMAIL"; fi
    printf '%s {\n\tencode gzip\n\theader Strict-Transport-Security "max-age=31536000"\n\treverse_proxy 127.0.0.1:8787\n}\n' "$HOST"
  } > /etc/caddy/Caddyfile
  if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then ufw allow 80,443/tcp; fi
  systemctl enable caddy
  systemctl restart caddy
fi

systemctl enable underhaven-weather
systemctl restart underhaven-weather
sleep 4
systemctl --no-pager status underhaven-weather | head -5 || true

if [ -n "$HOST" ]; then
  echo "waiting for the HTTPS certificate (up to ~2 minutes)..."
  ok=0
  for _ in $(seq 1 30); do
    code="$(curl -sS -o /dev/null -w '%{http_code}' --max-time 8 "https://$HOST/" 2>/dev/null || true)"
    if [ "$code" = "200" ]; then ok=1; break; fi
    sleep 4
  done
  echo
  if [ "$ok" -eq 1 ]; then
    echo "dashboard ready (trusted HTTPS, no browser warning): https://$HOST/"
  else
    echo "The certificate is not ready yet. Check, in this order:"
    echo "  1. ports 80 and 443 are open in your VPS provider's firewall panel (Let's Encrypt needs port 80)"
    [ -n "${DOMAIN:-}" ] && echo "  2. DNS: $HOST must have an A record pointing to $IP"
    echo "  3. journalctl -u caddy -n 40 --no-pager"
    echo "then open https://$HOST/ (Caddy keeps retrying by itself)."
  fi
fi
echo
echo "weather-bot is running: PAPER account only. To also trade live, see BUILD.md (section 2)."
echo "logs: journalctl -u underhaven-weather -f"
