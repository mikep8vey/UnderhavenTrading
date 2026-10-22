# UNDERHAVEN BYBIT TRADING LAB

A production-oriented, **paper-only** research and forward-testing platform for
Bybit USDT perpetual futures. It runs unattended on a small VPS, scans the
liquid linear-perpetual universe, evaluates three independent strategies, writes
every decision to a local SQLite ledger, and serves a private authenticated
dashboard with live state over SSE.

> **There is no live trading path in this repository.** No order-entry endpoint,
> private order-entry WebSocket topic, live toggle, or trading credential is
> reachable by the execution engine. This is enforced by module boundaries,
> repository tests, and the absence of any code that can submit an order. See
> [`docs/SAFETY.md`](docs/SAFETY.md).

## What it does

* **Dynamic universe scanner** – USDT-settled linear perpetuals only. Rejects
  stale quotes, crossed books, wide spreads, thin depth and extreme funding, then
  ranks by liquidity, spread, depth, turnover and volatility.
* **Three independent strategies** – Trend Following (EMA/ATR), Mean Reversion
  (Bollinger/z-score), Breakout/Momentum (range/ATR/volume). Each has its own
  `$100.00` paper wallet, ledger, positions, equity curve, metrics and report.
* **Cost-aware edge validation** – taker fees, spread, slippage and funding are
  charged to the paper ledger. A signal is only admitted when reward/risk clears
  the configured floor and expected net edge after costs is positive. Until
  there are enough closed trades, the dashboard says `INSUFFICIENT DATA` or
  `NOT YET VALIDATED` instead of inventing a probability.
* **Historical simulation** – no look-ahead, one-bar execution delay,
  conservative intrabar stop/target resolution (stop first), train/validation/
  out-of-sample segments, walk-forward folds, cost sensitivity, CSV and JSON
  exports.
* **Risk engine** – 0.5% equity risk per trade, 2% daily-loss lockout, 10%
  drawdown lockout, max two open positions, 1x leverage, mandatory stops,
  notional/correlation limits, consecutive-loss cooldown and an emergency
  flatten/kill switch.
* **Durable paper engine** – transactional ledger, idempotent fills and funding,
  restart recovery from SQLite, reconciliation, append-only audit triggers.
* **Secure admin surface** – Argon2id passwords, hashed session tokens,
  `HttpOnly; SameSite=Strict` cookies, per-session CSRF tokens, login rate
  limiting, session revocation, strict security headers and a strict CSP.
* **Optional read-only Bybit key** – verified with a signed `/v5/user/query-api`
  call, rejected unless Bybit reports `readOnly=1`, then encrypted with
  AES-256-GCM using a master key stored outside SQLite. Keys are never returned
  to the browser.
* **Automatic HTTPS by public IP** – Let's Encrypt `shortlived` profile via
  `acme.sh` + nginx, with a clearly labelled self-signed fallback.

## Dashboard sections

| Section | Purpose |
| --- | --- |
| **A – Command deck** | Aggregate equity, daily P&L, drawdown, exposure, costs, feed age, engine gates. |
| **B – Account & costs** | Per-strategy wallet ledger: cash, equity, realized/unrealized, fees, funding, day P&L. |
| **C – Strategy desk** | One card per strategy: equity sparkline, win rate, profit factor, expectancy, drawdown, OOS and forward-test status. |
| **D – Universe scanner** | Eligible market map with spread, depth, turnover, volatility, score and rejection reason. |
| **E – Book & fills** | Open paper positions with manual flatten, recent closed trades, CSV export. |
| **F – Edge lab** | Backtest runner, history import, segment/walk-forward/sensitivity reports, stored runs and exports. |
| **G – Risk & system** | Hard risk gates, pause/resume/emergency controls, system health, audit trail, password change and read-only key management. |

## Architecture

```
Bybit public REST ─┐
                   ├─> market cache ─> scanner ─> strategy evaluators
Bybit public WS ───┘                              │
                                                  v
                                      risk + edge admission gate
                                                  │
                                                  v
                                   transactional SQLite paper ledger
                                                  │
                              ┌───────────────────┴──────────────────┐
                              v                                      v
                    SSE dashboard / JSON API                 CSV/JSON reports
```

| Module | Responsibility |
| --- | --- |
| `src/config.rs` | Environment configuration, validated risk/cost/strategy defaults, paper-only mode refusal. |
| `src/bybit.rs` | Typed Bybit V5 public REST adapter, pacing/retries, signed read-only metadata calls. |
| `src/ws.rs` | Public WebSocket supervisor: reconnect/backoff/jitter, heartbeat, watchdog, order-book gap recovery. |
| `src/market.rs` | In-memory cache, scanner, ranking, seed merging. |
| `src/indicators.rs` | EMA, ATR, Bollinger, z-score, ROC, realized volatility. |
| `src/strategy.rs` | The three strategy evaluators and invalidation rules. |
| `src/costs.rs` | Cost breakdown, Wilson lower bound, edge gate. |
| `src/risk.rs` | Position sizing and hard risk gates. |
| `src/paper.rs` | Persistent paper engine, entry/exit/funding, lockouts, reconciliation. |
| `src/backtest.rs` | Historical simulation, segments, walk-forward, sensitivity, exports. |
| `src/store.rs` | SQLite schema access, migrations, idempotent fills, audit log. |
| `src/auth.rs` | Argon2id, sessions, CSRF, cookie policy, AES-256-GCM credential encryption. |
| `src/app.rs` | Runtime orchestration, dashboard aggregation, CLI helpers. |
| `src/web.rs` | Axum router, auth extractor, CSRF checks, SSE, JSON API, embedded UI. |
| `assets/` | Embedded HTML/CSS/JS and vendored uPlot 1.6.32 (no CDN). |
| `migrations/` | SQLite schema and append-only audit triggers. |
| `deploy/` | systemd, nginx, acme.sh, self-signed fallback, install/upgrade/backup/uninstall. |

The official Rust SDK `bybit-api` **0.1.2** was inspected and deliberately not
adopted at runtime: it lacks kline/funding pagination, instrument filtering and
WebSocket reconnect/heartbeat supervision, and pulls in `native-tls`. The typed
V5 adapter used instead, the endpoints it calls, signing details and its
remaining assumptions are documented in
[`docs/BYBIT_INTEGRATION.md`](docs/BYBIT_INTEGRATION.md).

## Quick start (development)

```bash
# Rust 1.85+ and SQLite are the only prerequisites.
cargo build
cargo test --all-targets

# Loopback development: Secure cookies are automatically not required on
# 127.0.0.1, but the default config still sets them; turn them off explicitly.
export UNDERHAVEN_SECURE_COOKIES=false
export UNDERHAVEN_DATA_DIR=./data
cargo run -- init-admin --username admin
cargo run -- serve
```

Open <http://127.0.0.1:8080>. The admin password must be at least 12 characters
and use three of: lowercase, uppercase, digits, symbols.

Optional local history and backtest:

```bash
cargo run -- import-history BTCUSDT --interval 5m --days 90
cargo run -- backtest BTCUSDT --strategy trend --interval 5m --days 90
cargo run -- doctor
```

## Production install

The supported target is Ubuntu 22.04/24.04 on a VPS with a public IPv4 address,
inbound TCP 80/443, and a release binary built for the same architecture.

```bash
cargo build --release
sudo ./deploy/install.sh --ip 203.0.113.10
```

The installer:

1. creates the `underhaven` service account and `/var/lib/underhaven`;
2. installs the binary to `/opt/underhaven/bin/underhaven`;
3. generates `/etc/underhaven/master.key` outside SQLite;
4. configures systemd hardening and nginx;
5. requests a Let's Encrypt **shortlived** IP certificate with `acme.sh`;
6. falls back to a clearly labelled self-signed certificate if issuance fails;
7. starts the paper engine and prompts for the initial admin password.

Full instructions, TLS details, upgrade, backup and uninstall:
[`docs/DEPLOY.md`](docs/DEPLOY.md). Day-to-day operation:
[`docs/OPERATIONS.md`](docs/OPERATIONS.md). Copy-paste Ubuntu commands:
[`docs/UBUNTU_QUICKSTART.md`](docs/UBUNTU_QUICKSTART.md).

## CLI

```
underhaven serve
underhaven init-admin [--username admin] [--force] [--password-stdin]
underhaven import-history BTCUSDT --interval 5m --days 90
underhaven backtest BTCUSDT --strategy trend --interval 5m --days 90
underhaven doctor
underhaven backup --output /var/lib/underhaven/backups/manual.db
underhaven tls-check [--url https://203.0.113.10]
```

There is no live/order subcommand.

## Configuration

All settings are environment variables. Important ones:

| Variable | Default | Notes |
| --- | --- | --- |
| `UNDERHAVEN_DATA_DIR` | `./data` | Database, session key, exports. |
| `UNDERHAVEN_DB_PATH` | `<data>/underhaven.db` | SQLite path. |
| `UNDERHAVEN_MASTER_KEY_FILE` | `<data>/master.key` | AES key outside SQLite. |
| `UNDERHAVEN_BIND` | `127.0.0.1:8080` | Bind loopback behind nginx. |
| `UNDERHAVEN_PUBLIC_URL` | unset | Shown in the UI; used by `tls-check`. |
| `UNDERHAVEN_TLS_MODE` | `unknown` | `letsencrypt`, `self_signed`, `proxy`, `none`, `unknown`. |
| `UNDERHAVEN_SECURE_COOKIES` | `true` | Non-loopback bind refuses insecure cookies unless explicitly overridden. |
| `UNDERHAVEN_ALLOW_INSECURE_HTTP` | `false` | Escape hatch for a trusted private network only. |
| `UNDERHAVEN_SYMBOLS` | BTC/ETH/SOL/XRP/DOGE | Seed symbols, always considered by the scanner. |
| `UNDERHAVEN_MAX_SYMBOLS` | `12` | Selected universe size. |
| `UNDERHAVEN_PRIMARY_INTERVAL` | `5m` | Scanner interval. |
| `UNDERHAVEN_MAX_SPREAD_BPS` | `8` | Scanner hard filter. |
| `UNDERHAVEN_MIN_DEPTH_USD` | `25000` | Depth within 10 bps of mid. |
| `UNDERHAVEN_MAX_STALENESS_MS` | `5000` | Reject older quotes. |
| `UNDERHAVEN_TAKER_FEE_BPS` | `7.5` | Conservative assumption, not a fee quote. |
| `UNDERHAVEN_SLIPPAGE_BPS` | `4` | Additional conservative fill assumption. |
| `UNDERHAVEN_AUTO_START` | `true` | Set false to start paused. |
| `UNDERHAVEN_LOGIN_MAX_ATTEMPTS` | `5` | Per IP per window. |
| `UNDERHAVEN_LOGIN_WINDOW_SECS` | `900` | Failed-login window. |
| `RUST_LOG` | `info` | `tracing` filter. |

A future live mode is rejected: `UNDERHAVEN_TRADING_MODE` must be `paper` or the
process refuses to start.

## Accounting and cost conventions

* Money is `f64` quantised to `1e-8` (`q8`) before persistence. This is a paper
  research ledger; a live system would use integer quote units or decimals.
* Fill prices include spread and slippage. Fees and funding are separate,
  single-entry cash movements. `spread_cost` and `slippage_est` are reporting
  estimates, not double charges.
* Bybit's actual fee schedule is account-specific. The default (7.5 bps taker,
  2.0 bps maker, 4 bps slippage) is a deliberately conservative assumption. If a
  verified read-only key is configured, `/v5/account/fee-rate` replaces it and
  the dashboard shows `bybit_account_fee_rate`.
* Funding is read from Bybit's public funding history for backtests and settled
  at 8-hour boundaries in forward paper trading. Missing funding history produces
  an explicit optimistic-result warning.

## Strategy and validation status

* **Trend Following** – EMA fast/slow alignment above/below a trend reference,
  momentum confirmation, ATR volatility band; ATR stop and target, optional
  trailing stop, maximum hold.
* **Mean Reversion** – Bollinger/z-score stretch with band-width and trend filters,
  optional reversal candle, ATR stop, maximum hold; disables itself when the
  regime is too trendy.
* **Breakout/Momentum** – range breakout with ATR buffer, volume expansion,
  range compression filter, ATR stop/target, trailing stop.

The edge gate never presents a probability from zero samples. A cold-start
signal can only pass with a hard cost cap and reward/risk floor, and is labelled
`unavailable`. After enough closed trades the gate uses the Wilson 95% lower
bound for win probability and requires a positive expected net edge.

## Verification

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
```

Repository-level tests in `tests/paper_only.rs` scan the engine modules for
order-entry endpoints and verify the UI does not load third-party assets. When
Node is installed, the same suite runs `node --check` against the embedded
dashboard scripts; Node is a development convenience, never a runtime dependency.
Deployment provisioning and public TLS issuance cannot be verified from a
Windows development host; see the final delivery report and `docs/DEPLOY.md`.

## Known limitations

* The backtester is `O(n²)` because indicators are recomputed on a growing prefix;
  CLI/web limits cap it at 20,000 candles.
* Historical order-book depth is not available for arbitrary periods. Backtests
  use an explicit spread assumption; forward paper trading uses the live book and
  conservative fills.
* The scanner and paper engine are single-process. SQLite is the source of truth,
  but this is not a horizontally scalable design.
* TLS by IP requires Let's Encrypt's `shortlived` profile and working HTTP-01
  validation on port 80. If that is blocked, the installer falls back to
  self-signed TLS, which browsers will not trust automatically.
* No exchange order is ever sent, so fill quality, queue position and liquidation
  behaviour are simulations, not exchange facts.

## License

MIT. See [`LICENSE`](LICENSE).
