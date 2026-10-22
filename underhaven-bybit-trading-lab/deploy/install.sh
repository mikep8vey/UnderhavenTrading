#!/usr/bin/env bash
# Idempotent Ubuntu 22.04/24.04 installer for UNDERHAVEN BYBIT TRADING LAB.
#
# Installs a prebuilt release binary when it exists. If it is missing, the
# installer bootstraps the build toolchain: it installs Rust and build
# dependencies, adds a 2 GB swap file on low-memory hosts, and builds with a
# reduced-memory release profile. One command is enough:
#
#   bash deploy/install.sh --ip 203.0.113.10
#
# It never enables a live trading mode; the application has no such mode.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IP="${UNDERHAVEN_IP:-}"
DOMAIN=""
BIN="${UNDERHAVEN_BIN:-$SCRIPT_DIR/../target/release/underhaven}"
BIN_EXPLICIT=0
ADMIN_USER="admin"
OPEN_FIREWALL=0
SKIP_TLS=0
APT_REFRESHED=0

usage() {
  cat <<'EOF'
Usage:
  bash deploy/install.sh --ip 203.0.113.10 [--bin ./target/release/underhaven]
  bash deploy/install.sh --domain lab.example.com [--bin /tmp/underhaven]

Options:
  --ip <address>        public IPv4 address for the certificate and URL
  --domain <name>       DNS name instead of an IP
  --bin <path>          use an existing prebuilt binary (skips the build)
  --admin-user <name>   initial administrator name (default: admin)
  --open-firewall       add UFW rules for TCP 80/443
  --no-tls              HTTP only; use behind an existing TLS proxy
  -h, --help            show this help

When --bin is omitted and target/release/underhaven is missing, the installer
installs Rust and build dependencies and builds the release binary itself.
EOF
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ip) IP="$2"; shift 2 ;;
    --domain) DOMAIN="$2"; shift 2 ;;
    --bin) BIN="$2"; BIN_EXPLICIT=1; shift 2 ;;
    --admin-user) ADMIN_USER="$2"; shift 2 ;;
    --open-firewall) OPEN_FIREWALL=1; shift ;;
    --no-tls) SKIP_TLS=1; shift ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

apt_refresh() {
  if [[ "$APT_REFRESHED" -eq 0 ]]; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    APT_REFRESHED=1
  fi
}

total_memory_mb() {
  awk '/^MemTotal:/{mem=$2} /^SwapTotal:/{swap=$2} END{printf "%d", (mem+swap)/1024}' /proc/meminfo
}

ensure_swap() {
  local total_mb free_mb
  total_mb="$(total_memory_mb)"
  if (( total_mb >= 3000 )); then
    return 0
  fi
  if grep -q '^/swapfile ' /proc/swaps 2>/dev/null; then
    return 0
  fi
  free_mb="$(df -Pm / | awk 'NR==2 {print $4}')"
  if (( free_mb < 2500 )); then
    echo "WARNING: low memory and only ${free_mb} MB free disk; not adding swap" >&2
    return 0
  fi

  echo "==> low memory (${total_mb} MB): adding a 2 GB swap file for the build"
  if [[ ! -f /swapfile ]]; then
    if ! fallocate -l 2G /swapfile 2>/dev/null; then
      dd if=/dev/zero of=/swapfile bs=1M count=2048 status=none
    fi
    chmod 600 /swapfile
  fi
  if ! mkswap /swapfile >/dev/null 2>&1; then
    echo "WARNING: could not format /swapfile; continuing without new swap" >&2
    rm -f /swapfile
    return 0
  fi
  if ! swapon /swapfile 2>/dev/null; then
    echo "WARNING: could not enable swap (container or restricted host)" >&2
    return 0
  fi
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >> /etc/fstab
}

build_release() {
  echo "==> release binary missing; installing the Rust build toolchain"
  ensure_swap
  apt_refresh
  apt-get install -y -qq build-essential pkg-config libssl-dev curl ca-certificates

  if ! command -v cargo >/dev/null 2>&1; then
    if [[ -x "${HOME:-/root}/.cargo/bin/cargo" ]]; then
      export PATH="${HOME:-/root}/.cargo/bin:$PATH"
    else
      echo "==> installing Rust with rustup"
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
      export PATH="${HOME:-/root}/.cargo/bin:$PATH"
    fi
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is still unavailable after rustup" >&2
    exit 1
  fi
  echo "==> using $(cargo --version)"

  echo "==> building the release binary (this is the slow step; 5-15 minutes)"
  if (( $(total_memory_mb) < 4000 )); then
    echo "==> low-memory build profile: jobs=1, LTO off, 16 codegen units, opt-level 2"
    (
      cd "$SCRIPT_DIR/.."
      CARGO_BUILD_JOBS=1 \
      CARGO_PROFILE_RELEASE_LTO=false \
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
      CARGO_PROFILE_RELEASE_OPT_LEVEL=2 \
        cargo build --release
    )
  else
    ( cd "$SCRIPT_DIR/.." && cargo build --release )
  fi

  if [[ ! -f "$BIN" ]]; then
    echo "build finished but the binary is missing: $BIN" >&2
    exit 1
  fi
}

if [[ "$(id -u)" -ne 0 ]]; then
  echo "run as root (sudo)" >&2
  exit 1
fi
if ! command -v systemctl >/dev/null 2>&1 || [[ "$(ps -p 1 -o comm= 2>/dev/null)" != "systemd" ]]; then
  echo "systemd is required; this host is probably a container (PID 1 is not systemd)" >&2
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

if [[ ! -f "$BIN" ]]; then
  if [[ "$BIN_EXPLICIT" -eq 1 ]]; then
    echo "binary not found: $BIN" >&2
    exit 1
  fi
  build_release
fi

echo "==> installing runtime dependencies"
apt_refresh
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
