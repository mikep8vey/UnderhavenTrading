# Ubuntu quick start

Two supported ways to run UNDERHAVEN on Ubuntu 22.04/24.04. Both are
**paper-only**; there is no live order path to enable.

## 0. Prerequisites

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev curl
# Rust 1.85 or newer
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version    # must be >= 1.85
```

The application itself needs no Python, Node, Redis or Postgres. Node is only
used by one optional test that parses the dashboard JavaScript.

## 1. Unpack and build

```bash
unzip underhaven-bybit-trading-lab.zip
cd underhaven-bybit-trading-lab
cargo build --release
cargo test --all-targets      # optional but recommended
```

Or skip this section entirely for a VPS: the installer in section 2b installs
Rust and the build dependencies itself and builds the binary. A low-memory host
gets a 2 GB swap file and a reduced-memory release profile automatically.

The binary is `target/release/underhaven`. SQLite migrations are compiled into
it, so you never need the `migrations/` directory on the server.

## 2a. Local / development run (loopback only)

Use this to look at the dashboard before touching a server:

```bash
export UNDERHAVEN_DATA_DIR="$PWD/data"
export UNDERHAVEN_BIND=127.0.0.1:8080
export UNDERHAVEN_SECURE_COOKIES=false      # loopback only
export UNDERHAVEN_AUTO_START=false          # start paused until you are ready

./target/release/underhaven init-admin --username admin
./target/release/underhaven serve
```

Open <http://127.0.0.1:8080>, sign in, then press **Resume** in section
**G – Risk & system** when you want the paper engine to evaluate signals.
The password must be at least 12 characters and use three of: lowercase,
uppercase, digits, symbols.

Useful CLI checks:

```bash
./target/release/underhaven doctor
./target/release/underhaven import-history BTCUSDT --interval 5m --days 90
./target/release/underhaven backtest BTCUSDT --strategy trend --interval 5m --days 90
./target/release/underhaven backup --output data/backups/manual.db
```

## 2b. Always-on VPS install (systemd + nginx + HTTPS by public IP)

Requirements: a public IPv4 address, inbound TCP 80/443 open, and a release
binary built for the VPS architecture. Build on the VPS or copy
`target/release/underhaven` from a machine with the same architecture.

One command. It installs Rust/build tools and builds the binary if needed:

```bash
sudo bash deploy/install.sh --ip 203.0.113.10       # your public IP
```

On a normal VPS with 4 GB+ RAM the build uses the normal release profile. On a
smaller host the installer adds a 2 GB swap file and builds with `jobs=1`,
LTO off, 16 codegen units and `opt-level=2`, which avoids the Linux OOM killer
(`SIGKILL`) while compiling. Use `--bin /path/to/underhaven` to skip building.

That script:

1. creates the `underhaven` service account and `/var/lib/underhaven`;
2. installs the binary to `/opt/underhaven/bin/underhaven`;
3. generates `/etc/underhaven/master.key` (encryption key, outside SQLite);
4. installs the systemd unit and the nginx reverse proxy;
5. requests a Let's Encrypt **shortlived** certificate for the IP through
   `acme.sh` (valid about 6 days, renewed by acme.sh's daily cron);
6. falls back to a clearly labelled **self-signed** certificate if issuance
   fails — browsers will warn until you replace it;
7. starts the service and prompts for the initial admin password.

Options: `--domain name.example.com` for a normal 90-day hostname certificate,
`--open-firewall` to add UFW rules, `--no-tls` for a plain HTTP reverse proxy on
a trusted network. See [`DEPLOY.md`](DEPLOY.md) for the full list.

After the install:

```bash
sudo systemctl status underhaven --no-pager
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS https://203.0.113.10/healthz
sudo nginx -t
sudo openssl x509 -in /etc/underhaven/tls/fullchain.pem -noout -dates
sudo -u underhaven env UNDERHAVEN_PUBLIC_URL=https://203.0.113.10 \
  /opt/underhaven/bin/underhaven tls-check
```

Open `https://203.0.113.10` and sign in. If the certificate is self-signed, the
dashboard shows `TLS self-signed` in the status strip instead of claiming a
trusted certificate.

## 3. Day-to-day service commands

```bash
sudo systemctl status underhaven --no-pager
sudo systemctl restart underhaven
sudo systemctl stop underhaven
sudo systemctl start underhaven
journalctl -u underhaven -f
journalctl -u underhaven --since '1 hour ago' -p warning
```

Backup, upgrade and removal:

```bash
sudo ./deploy/backup.sh                                    # consistent SQLite backup
sudo ./deploy/upgrade.sh ./target/release/underhaven       # atomic upgrade + health check
sudo ./deploy/uninstall.sh                                 # remove service, keep database
sudo ./deploy/uninstall.sh --purge                         # also delete data, keys, certs
```

## 4. Optional read-only Bybit key

Only needed to display your real fee rate and account UID. It is **not**
required for paper trading and cannot place orders.

1. In the Bybit UI create an API key with **read-only** permission and no
   trading, transfer or withdrawal rights.
2. Dashboard → **G – Risk & system** → *Optional read-only key*: enter a label,
   the key and the secret.
3. The server calls `/v5/user/query-api` and stores the key only if Bybit
   reports `readOnly=1`; otherwise nothing is stored. It is encrypted with
   AES-256-GCM using `/etc/underhaven/master.key` and is never returned to the
   browser.

## 5. Where things live (VPS install)

| Path | Purpose |
| --- | --- |
| `/opt/underhaven/bin/underhaven` | release binary |
| `/etc/underhaven/underhaven.env` | configuration (mode `0640 root:underhaven`) |
| `/etc/underhaven/master.key` | AES-256-GCM key for stored credentials |
| `/etc/underhaven/tls/` | nginx certificate and key |
| `/var/lib/underhaven/underhaven.db` | SQLite ledger (source of truth) |
| `/var/lib/underhaven/backups/` | `VACUUM INTO` backups |

## 6. Troubleshooting

| Symptom | Check |
| --- | --- |
| `502 Bad Gateway` | `curl http://127.0.0.1:8080/healthz`, then `systemctl status underhaven` and `journalctl -u underhaven -n 100` |
| No public HTTPS | port 80 reachable? `sudo ./deploy/acme-shortlived.sh <ip>` and read its output |
| Feed unhealthy | outbound WSS/REST blocked, clock drift (`underhaven doctor` warns above 2 s), or Bybit incident |
| Login refused | five failed attempts per IP per 15 minutes; wait or clear the window |
| Upgraded but old UI | assets revalidate now; force-reload once if a proxy cached them |

Full documentation: [`DEPLOY.md`](DEPLOY.md), [`OPERATIONS.md`](OPERATIONS.md),
[`SAFETY.md`](SAFETY.md), [`BYBIT_INTEGRATION.md`](BYBIT_INTEGRATION.md).
