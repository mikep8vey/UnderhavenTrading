#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
set -a; [ -f .env ] && . ./.env; set +a
export WEATHER_BOT_CONFIG_DIR="${WEATHER_BOT_CONFIG_DIR:-$PWD/config}"
exec cargo run --release -- dry-run "$@"
