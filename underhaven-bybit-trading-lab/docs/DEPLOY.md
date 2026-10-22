# Deployment

This guide targets **Ubuntu 22.04 or 24.04** on a VPS with a public IPv4
address, inbound TCP 80/443, and systemd. The application binds loopback and is
published by nginx.

> **Verification status.** The installer, systemd unit and nginx configuration
> in this repository were authored and reviewed on a Windows development host.
> They were **not executed on a VPS**, and no public Let's Encrypt certificate
> was requested during development. Treat the first production install as a
> supervised change and follow the verification steps below. See the delivery
> report for the exact NOT RUN list.

## 1. Prerequisites

On the VPS:

* Ubuntu 22.04/24.04, root or sudo.
* A public IPv4 address. For the IP certificate path, TCP 80 must be reachable
  from the public internet for HTTP-01 validation.
* Ports 80 and 443 allowed by the provider firewall. Use `--open-firewall` to
  add UFW rules if UFW is active.
* A DNS name is optional. `--domain lab.example.com` uses the normal certificate
  profile with HTTP-01; `--ip` uses the short-lived IP profile.
* Outbound HTTPS/WSS to Bybit (`api.bybit.com`, `stream.bybit.com`) and to
  `get.acme.sh` during installation.

Build the binary on a compatible Linux host (same CPU architecture and libc) or
in CI:

```bash
sudo apt-get update
sudo apt-get install -y build-essential pkg-config curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
git clone <your-repository> underhaven
cd underhaven/underhaven-bybit-trading-lab
cargo build --release
```

The resulting binary is `target/release/underhaven`. The VPS does not need a
Rust toolchain if you copy this binary to it.

## 2. Install

From the project directory on the VPS:

```bash
sudo ./deploy/install.sh --ip 203.0.113.10
# or
sudo ./deploy/install.sh --domain lab.example.com
```

Options:

| Option | Meaning |
| --- | --- |
| `--ip <addr>` | Public IPv4/IPv6; uses Let's Encrypt short-lived profile. |
| `--domain <name>` | DNS name; uses the normal certificate profile. |
| `--bin <path>` | Binary to install (default `target/release/underhaven`). |
| `--admin-user <name>` | Initial admin username (default `admin`). |
| `--open-firewall` | Add UFW allow rules for 80/443. |
| `--no-tls` | HTTP bootstrap vhost only; for testing behind another proxy. |

The installer:

1. installs `nginx`, `openssl`, `curl`, `ca-certificates`, `cron`;
2. creates the `underhaven` system user and `/opt/underhaven`,
   `/var/lib/underhaven`, `/etc/underhaven`;
3. installs the binary atomically;
4. generates `/etc/underhaven/master.key` (32 random bytes, `root:underhaven`,
   `0640`) outside SQLite;
5. writes `/etc/underhaven/underhaven.env` from
   `deploy/underhaven.env.example` and substitutes the public URL;
6. installs and enables the systemd unit;
7. replaces the default nginx vhost with an HTTP bootstrap vhost;
8. runs `acme.sh` for the certificate, or falls back to self-signed TLS;
9. starts the paper engine and creates the initial administrator if a terminal
   or `UNDERHAVEN_ADMIN_PASSWORD` is available.

The service account is `underhaven`; its home is `/opt/underhaven`; it cannot
write `/etc/underhaven`.

### Admin creation

Interactive install prompts for the password. Non-interactive install prints the
command to run later:

```bash
sudo -u underhaven env \
  UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
  UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
  UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
  /opt/underhaven/bin/underhaven init-admin --username admin
```

Or supply it once:

```bash
sudo UNDERHAVEN_ADMIN_PASSWORD='a-long-unique-password' \
  ./deploy/install.sh --ip 203.0.113.10
```

Password policy: at least 12 characters, at least three of lowercase,
uppercase, digits, symbols, and it must not contain the username.

To reset a forgotten password deliberately:

```bash
sudo -u underhaven env \
  UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
  UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
  UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
  /opt/underhaven/bin/underhaven init-admin --username admin --force
```

This revokes sessions and changes the password. It does not touch the ledger.

## 3. TLS details

### Let's Encrypt short-lived IP certificate

`deploy/acme-shortlived.sh` runs roughly:

```bash
acme.sh --issue -d "$IP" -w /var/www/letsencrypt \
  --server letsencrypt \
  --certificate-profile shortlived --days 3 --keylength ec-256

acme.sh --install-cert -d "$IP" --ecc \
  --key-file /etc/underhaven/tls/key.pem \
  --fullchain-file /etc/underhaven/tls/fullchain.pem \
  --reloadcmd 'systemctl reload nginx'
```

Notes:

* Let's Encrypt requires the `shortlived` **certificate profile** for IP
  addresses. Certificates are valid for roughly six days.
* `--days 3` makes acme.sh renew with about three days left. The daily acme.sh
  cron job is sufficient; do not disable it.
* Validation is **HTTP-01 on port 80** using the webroot
  `/var/www/letsencrypt`. DNS-01 is not used.
* `nginx-underhaven.conf` keeps the ACME location on port 80 and redirects
  everything else to HTTPS.
* If acme.sh reports that renewal is not due, the script installs the existing
  certificate instead of treating that as a failure.
* The app records `UNDERHAVEN_TLS_MODE=letsencrypt` and displays it in the
  dashboard so the mode is explicit.

Check issuance and renewal:

```bash
sudo /root/.acme.sh/acme.sh --list
sudo openssl x509 -in /etc/underhaven/tls/fullchain.pem -noout -subject -issuer -dates
sudo -u underhaven env UNDERHAVEN_PUBLIC_URL=https://203.0.113.10 \
  /opt/underhaven/bin/underhaven tls-check
```

`tls-check` verifies the public HTTPS endpoint and certificate chain from the
VPS. It will fail for the self-signed fallback by design.

### Self-signed fallback

If issuance fails, the installer runs `deploy/selfsigned.sh` and installs
`nginx-underhaven-selfsigned.conf`. The certificate has a 365-day lifetime, an
IP or DNS SAN as appropriate, and is written to
`/etc/underhaven/tls/selfsigned.{crt,key}`.

The dashboard sets `UNDERHAVEN_TLS_MODE=self_signed` and shows a warning chip.
Browsers will not trust this certificate without a manual exception. Regenerate
it before expiry:

```bash
sudo ./deploy/selfsigned.sh 203.0.113.10
```

Fix the underlying HTTP-01 issue and re-run `install.sh` to move to a publicly
trusted certificate. Do not treat a self-signed deployment as production-grade
for a public address.

### Behind an existing reverse proxy

Set `UNDERHAVEN_BIND=127.0.0.1:8080`, keep `UNDERHAVEN_SECURE_COOKIES=true`, and
set `UNDERHAVEN_TLS_MODE=proxy`. The proxy must overwrite `X-Forwarded-For`
rather than append a client-supplied value. Disable proxy buffering for
`/api/stream` (SSE).

## 4. Configuration

Edit `/etc/underhaven/underhaven.env` and restart:

```bash
sudo systemctl restart underhaven
```

Keep the file `root:underhaven 0640`. It contains no secret by default; the
session key is in the data directory and the AES master key is separate.

Important variables are documented in the README. Production defaults:

```
UNDERHAVEN_DATA_DIR=/var/lib/underhaven
UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db
UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key
UNDERHAVEN_BIND=127.0.0.1:8080
UNDERHAVEN_SECURE_COOKIES=true
UNDERHAVEN_TLS_MODE=letsencrypt
```

Changing `UNDERHAVEN_MASTER_KEY_FILE` makes existing encrypted Bybit
credentials unreadable; delete and re-enter the key if you rotate it.

### Read-only Bybit key (optional)

1. In Bybit, create an API key with **read-only** permissions. Do not enable
   trading, withdrawal or transfer.
2. Sign in to the dashboard over HTTPS.
3. Open section **G – Risk & system** → read-only key.
4. Enter a label, the key and the secret.
5. The server calls `/v5/user/query-api`. A key that Bybit does not report as
   `readOnly=1` is rejected and nothing is stored.
6. On success the key is encrypted with AES-256-GCM using
   `/etc/underhaven/master.key`.

The dashboard only shows verification status, UID and note. The secret is never
returned. See [`SAFETY.md`](SAFETY.md).

## 5. Verification after install

```bash
# Service is active and healthy
systemctl status underhaven --no-pager
curl -fsS http://127.0.0.1:8080/healthz

# Public HTTPS endpoint and certificate
curl -fsS https://203.0.113.10/healthz
sudo -u underhaven env UNDERHAVEN_PUBLIC_URL=https://203.0.113.10 \
  /opt/underhaven/bin/underhaven tls-check

# nginx config and certificate dates
sudo nginx -t
sudo openssl x509 -in /etc/underhaven/tls/fullchain.pem -noout -dates

# No order-entry code in the installed source
cd /path/to/source && cargo test --test paper_only
```

Then sign in, confirm section **G** shows:

* `paper_only: true`,
* the expected TLS mode,
* a healthy public feed and WebSocket,
* no lockout caused by a previous run,
* the audit trail contains `system.start` / login events.

Start the paper engine only after the feed and clock are healthy. The default
`UNDERHAVEN_AUTO_START=true` begins evaluating as soon as the process starts,
subject to the risk gates.

## 6. Upgrade

```bash
cargo build --release
sudo ./deploy/upgrade.sh ./target/release/underhaven
```

`upgrade.sh` takes a `VACUUM INTO` backup, installs the binary atomically,
restarts the service, and requires `/healthz` to become healthy within 30 s. If
it fails, the old binary is **not** restored automatically; the pre-upgrade
database backup path is printed. Keep the previous binary for a manual rollback:

```bash
sudo install -o root -g root -m 0755 /path/to/old/underhaven /opt/underhaven/bin/underhaven
sudo systemctl restart underhaven
```

Database migrations run at startup. Back up before every upgrade.

## 7. Backup and restore

Consistent online backup:

```bash
sudo ./deploy/backup.sh 14
# or
sudo -u underhaven env \
  UNDERHAVEN_DATA_DIR=/var/lib/underhaven \
  UNDERHAVEN_DB_PATH=/var/lib/underhaven/underhaven.db \
  UNDERHAVEN_MASTER_KEY_FILE=/etc/underhaven/master.key \
  /opt/underhaven/bin/underhaven backup \
  --output /var/lib/underhaven/backups/manual.db
```

`deploy/backup.sh` keeps 14 days by default and deletes older
`underhaven-*.db` files in `/var/lib/underhaven/backups`. Add a systemd timer or
cron entry if you want unattended backups.

Back up **both**:

* `/var/lib/underhaven/underhaven.db` (plus `-wal`/`-shm` if copying manually;
  prefer `underhaven backup`),
* `/etc/underhaven/master.key` (separate, access-controlled storage).

Restore with the service stopped:

```bash
sudo systemctl stop underhaven
sudo -u underhaven cp /var/lib/underhaven/backups/underhaven-YYYYMMDD-HHMMSS.db \
  /var/lib/underhaven/underhaven.db
sudo -u underhaven rm -f /var/lib/underhaven/underhaven.db-wal \
  /var/lib/underhaven/underhaven.db-shm
sudo systemctl start underhaven
```

If the master key is lost, encrypted credentials cannot be decrypted. Delete the
stored key in the dashboard and re-enter it; the ledger is unaffected.

## 8. Uninstall

```bash
sudo ./deploy/uninstall.sh          # keep data and keys
sudo ./deploy/uninstall.sh --purge  # delete database, keys, binary, user
```

Uninstall stops and disables the service, removes the systemd unit and nginx
vhost, restores the distribution default vhost if it was disabled, and reloads
nginx. `--purge` also deletes `/var/lib/underhaven`, `/etc/underhaven`,
`/opt/underhaven` and the service account. It does not revoke the Bybit API key;
do that at Bybit.

## 9. Troubleshooting

| Symptom | Check |
| --- | --- |
| Service exits immediately | `journalctl -u underhaven -n 100`; check permissions on `/var/lib/underhaven` and `/etc/underhaven/master.key`. |
| `502 Bad Gateway` | `curl http://127.0.0.1:8080/healthz`; check bind address and service status. |
| SSE disconnects | nginx must have `proxy_buffering off` and a long `proxy_read_timeout` for `/api/stream`. |
| Certificate issuance fails | Port 80 reachable? `curl http://<ip>/.well-known/acme-challenge/test`; check `acme.sh --list`, `/root/.acme.sh/*.log`. |
| Browser warns on certificate | Confirm `UNDERHAVEN_TLS_MODE`; self-signed is expected to warn. Check cert SAN matches the URL. |
| Login rate-limited | Wait for `UNDERHAVEN_LOGIN_WINDOW_SECS`; inspect failed-login audit events. |
| Feed unhealthy | Check outbound WSS, clock drift (`doctor` warns above 2 s), and Bybit status. |
| Backtest times out | The simulator is O(n²); lower `--candles` or use a shorter range. |
| SQLite busy | Ensure one process owns the database; check disk space and WAL files. |

## 10. What was not verified here

The following require a real Linux host and public network and were **not**
executed during development:

* `deploy/install.sh`, `upgrade.sh`, `backup.sh`, `uninstall.sh` on Ubuntu;
* systemd unit hardening and service startup under systemd;
* nginx configuration syntax and SSE proxying;
* `acme.sh` issuance and renewal for a public IP;
* browser trust of an IP certificate;
* UFW/firewall interaction.

These are documented as supervised first-install steps, not as verified facts.
