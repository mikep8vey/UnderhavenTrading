#!/usr/bin/env bash
# Run as root from the repository root:  sudo ./scripts/install.sh
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then echo "run as root (sudo)"; exit 1; fi
cd "$(dirname "$0")/.."

apt update && apt install -y build-essential pkg-config libssl-dev sqlite3 curl

if ! command -v cargo >/dev/null 2>&1; then
  curl https://sh.rustup.rs -sSf | sh -s -- -y
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"

cargo build --release
install -m 0755 target/release/weather-bot /usr/local/bin/weather-bot

mkdir -p /var/lib/weather-bot/exports /etc/weather-bot/config
cp -n config/*.toml /etc/weather-bot/config/
if [ ! -f /etc/weather-bot/env ]; then
  cp .env.example /etc/weather-bot/env
  chmod 600 /etc/weather-bot/env
fi

install -m 0644 scripts/underhaven-weather.service /etc/systemd/system/underhaven-weather.service
systemctl daemon-reload
systemctl enable --now underhaven-weather
echo "installed. status: systemctl status underhaven-weather   logs: journalctl -u underhaven-weather -f"
