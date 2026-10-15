#!/usr/bin/env bash
# LIVE MODE. Real money. Run dry-run for days first.
set -euo pipefail
cd "$(dirname "$0")/.."
set -a; [ -f .env ] && . ./.env; set +a
export WEATHER_BOT_CONFIG_DIR="${WEATHER_BOT_CONFIG_DIR:-$PWD/config}"
exec cargo run --release -- live --enable "$@"
