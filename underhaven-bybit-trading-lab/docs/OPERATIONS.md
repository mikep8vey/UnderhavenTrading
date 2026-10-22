# Operations

Day-to-day operation of a running UNDERHAVEN paper lab. The platform is
unattended by design, but it is not self-justifying: someone must check that the
feed is alive, the ledger reconciles, and the reported edge is not an artifact.

## 1. Service commands

```bash
sudo systemctl status underhaven --no-pager
sudo systemctl restart underhaven
sudo systemctl stop underhaven
sudo systemctl start underhaven
journalctl -u underhaven -f
journalctl -u underhaven --since '1 hour ago' -p warning
```

The unit sends `SIGINT` on stop, which triggers a graceful shutdown: background
tasks are cancelled, the engine writes a final audit event, and SQLite is closed
by the process exit. `TimeoutStopSec=25`.

Health check:

```bash
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS https://<public-name>/healthz
```

The JSON is deliberately minimal: `status`, `version` and `paper_only`. It is
public so a local proxy or uptime monitor can probe liveness without
credentials, and it contains no account data. Uptime, database size, WAL state,
feed/WebSocket health and Bybit API counters are only available in the
authenticated dashboard (section **G – Risk & system**).

The HTTP listener binds before the startup market-data warm-up. If Bybit is
slow or unreachable, `/healthz` and the dashboard still answer promptly and
show the feed as unhealthy; a reverse proxy will not report a false 502.

## 2. First check after a restart

1. Open section **A – Command deck**. Confirm:
   * `PAPER ONLY` badge,
   * feed age below the staleness gate,
   * WebSocket `connected`,
   * public REST healthy,
   * clock offset small (a persistent offset above 2 s deserves investigation).
2. Confirm the engine is **running** or **paused** as intended. A restart does
   not clear a persisted emergency stop, daily-loss lock or drawdown lock.
3. Open section **B – Account & costs** and check that each strategy has its own
   `$100.00` initial equity and that cash/equity reconcile.
4. Open section **G – Risk & system** and read the latest audit events. Startup
   writes a `system.start` event; shutdown writes `system.shutdown`.
5. Check that no position is older than the configured maximum age and that
   every open position has a stop.

If the feed is unhealthy, the engine should refuse new entries. Do not "fix" that
by disabling gates.

## 3. Dashboard map

| Section | What to look at |
| --- | --- |
| **A – Command deck** | Aggregate equity, daily P&L, drawdown, exposure, costs, feed age, engine gates. |
| **B – Account & costs** | Per-strategy cash, equity, realized/unrealized, fees, funding, day P&L, ledger status. |
| **C – Strategy desk** | Equity sparkline, win rate, profit factor, expectancy, drawdown, OOS status, forward-test status. |
| **D – Universe scanner** | Eligible symbols, spread, depth, turnover, volatility, score, rejection reasons. |
| **E – Book & fills** | Open paper positions, manual flatten, recent closed trades, CSV export. |
| **F – Edge lab** | Backtest runner, history import, segment/walk-forward/sensitivity reports, stored runs, CSV/JSON. |
| **G – Risk & system** | Risk gates, pause/resume/emergency controls, system health, audit trail, password change, read-only key. |

The UI is served from the binary and updated by authenticated SSE every 2
seconds. There is no polling storm and no third-party asset.

## 4. Engine controls

| Action | Effect |
| --- | --- |
| **Pause** | Blocks new entries. Existing positions remain managed (stops, targets, trailing, max age, funding). |
| **Resume** | Clears a manual pause. It does **not** clear daily-loss/drawdown lockouts or an emergency stop. |
| **Emergency stop** | Flattens every open paper position at the current quote and blocks new entries until cleared. |
| **Clear emergency** | Re-enables entries after an emergency stop, subject to all other gates. |
| **Reset lockouts** | Clears daily-loss/drawdown/consecutive-loss lockouts. Use only after investigating the cause. |
| **Flatten position** | Closes a single paper position at the current quote. |

Every action is CSRF-protected and audited with the operator name and reason.

### Lockouts

* **Daily loss:** equity below `day_start_equity` by 2% locks entries for the UTC
  day. The day-start snapshot is persisted.
* **Drawdown:** equity 10% below peak equity locks entries until explicitly
  reset. This is deliberately sticky.
* **Consecutive losses:** after 5 losing trades, a 720-minute cooldown applies.
* **Stale feed / wide spread / thin book:** entry gates, not persistent lockouts.
* **Funding sanity band:** a symbol with extreme funding is excluded by the
  scanner.

Do not reset a lockout merely to "keep the sample going". A lockout is part of
the forward-test record.

## 5. Reading the results honestly

### Strategy status

| Label | Meaning |
| --- | --- |
| `NOT YET VALIDATED` | No closed paper trades. No probability is claimed. |
| `INSUFFICIENT DATA` | Fewer than 30 closed trades. Do not treat win rate as stable. |
| `EXPERIMENTAL` | 30–99 closed trades. Directional evidence only. |
| `FORWARD SAMPLE` | 100+ closed trades. Still not a guarantee; regime and costs matter. |

### Backtest status

* **Out-of-sample** is the last 20% of the selected range (60% train, 20%
  validation, 20% OOS). It is not a fresh market sample.
* **Walk-forward** uses 40% train / 20% test folds. A fold with no validated
  variant deliberately does not trade its test window; that is reported, not
  hidden.
* **Sensitivity** shows whether nearby parameters collapse. A single lucky
  parameter set is not an edge.
* **INSUFFICIENT DATA** appears when a segment has fewer than the configured
  minimum trades (default 10 in the simulator; the dashboard list uses 30 for a
  headline).
* Costs include taker fees, an explicit spread assumption, slippage and funding.
  A gross-profitable strategy can be net-negative.

Never compare a backtest with a forward sample without checking that the fee,
spread and funding assumptions match.

### Account reconciliation

The engine reconciles wallets against positions at startup and after repairs.
A mismatch is surfaced in the dashboard and audit log. If reconciliation cannot
explain a difference, stop the engine and inspect the database before resuming.

## 6. Common operating tasks

### Change the admin password

Dashboard → **G** → password. All sessions, including the current one, are
revoked. Sign in again.

### Rotate or remove the Bybit read-only key

Dashboard → **G** → read-only key. Delete, then re-enter the new key. At Bybit,
revoke the old key. Rotating `/etc/underhaven/master.key` requires re-entering
all stored credentials.

### Import history and run a backtest

```bash
underhaven import-history BTCUSDT --interval 5m --days 90
underhaven backtest BTCUSDT --strategy trend --interval 5m --days 90
underhaven backtest ETHUSDT --strategy mean_reversion --interval 15m --days 120 --candles 15000
underhaven backtest SOLUSDT --strategy breakout --interval 5m --days 90 --output ./exports
```

The CLI and web runner share the same simulator. Reports are stored in SQLite
and exported as JSON plus a trade CSV. The simulator is O(n²); cap candles
around 20,000 on a small VPS.

### Export paper trades

Dashboard → **E** → CSV, or:

```bash
sqlite3 /var/lib/underhaven/underhaven.db \
  "SELECT * FROM trades ORDER BY exit_time_ms DESC LIMIT 100;"
```

Use the dashboard export for a stable column order.

### Backups

```bash
sudo ./deploy/backup.sh 14
```

Schedule it if the VPS is long-lived. Store the database and
`/etc/underhaven/master.key` separately; neither is useful for credentials
without the other. Test a restore on a non-production host.

### Inspect the database

```bash
sudo -u underhaven sqlite3 /var/lib/underhaven/underhaven.db \
  "PRAGMA integrity_check;"
sudo -u underhaven sqlite3 /var/lib/underhaven/underhaven.db \
  "SELECT strategy, cash, equity, fees, funding FROM wallets;"
sudo -u underhaven sqlite3 /var/lib/underhaven/underhaven.db \
  "SELECT ts_ms, event_type, actor, detail FROM audit_events ORDER BY id DESC LIMIT 20;"
```

Do not edit ledger rows by hand. If a repair is unavoidable, stop the service,
take a backup, and document what changed.

## 7. Failure handling

| Observation | Response |
| --- | --- |
| WebSocket reconnecting for minutes | Check outbound WSS and Bybit status. REST fallback keeps quotes fresh; entries may remain blocked by the feed gate. |
| Scanner shows all symbols rejected | Read the reason column. A global cause is usually stale data, clock drift or a Bybit API change. |
| API error counter climbing | Inspect logs for `retCode`/HTTP status. Repeated 10006 is rate limiting; the adapter backs off. |
| `database is locked` | Ensure only one `underhaven` process uses the DB. Check disk space and long-running backtests. |
| Disk usage growing | Backtests and candles accumulate. Vacuum/backup, prune old non-seed candles, and keep exports outside the DB. |
| Repeated login failures | Check the source IPs in the audit log. Consider restricting the dashboard by firewall/VPN. |
| Unexpected lockout | Read the audit trail and wallet history before resetting. |
| Service crash loop | `journalctl -u underhaven -n 200`; verify master key/session key permissions and DB integrity. |

## 8. Security maintenance

* Keep the host patched; reboot for kernel updates.
* Restrict SSH to keys; disable password login.
* Allow only 80/443 publicly (and your SSH port). Never expose 8080.
* Keep `UNDERHAVEN_SECURE_COOKIES=true` on any public address.
* Review `audit_events` for unexpected logins or control actions.
* Rotate the admin password and Bybit key periodically.
* Verify certificate expiry and acme.sh renewal:

```bash
sudo /root/.acme.sh/acme.sh --list
sudo openssl x509 -in /etc/underhaven/tls/fullchain.pem -noout -dates
sudo systemctl list-timers | grep -i acme || true
```

* For the self-signed fallback, regenerate before the 365-day expiry and plan a
  move to a trusted certificate.

## 9. Capacity and retention

* The reference VPS is small (1–2 vCPU, 1–2 GB RAM). SQLite WAL and the scanner
  are light; backtests are the CPU spike.
* Keep at most ~20,000 candles per backtest run.
* The engine writes an equity point at most once per minute per strategy.
* Audit rows are small and append-only; retain them. If you must archive,
  export rather than delete (the database triggers block deletion).
* Set a disk-usage alert. A full disk is the most likely cause of a ledger
  failure.

## 10. Routine checklist

**Daily**

- [ ] `/healthz` healthy and public TLS valid.
- [ ] Feed age and WebSocket state normal.
- [ ] No new unexpected lockout or emergency stop.
- [ ] Open positions have stops and are within maximum age.
- [ ] Audit tail has no unexpected login/control event.

**Weekly**

- [ ] Review strategy status and trade sample size.
- [ ] Compare fees/funding/slippage against assumptions.
- [ ] Check disk, memory, backup completion and certificate expiry.
- [ ] Review rejected-symbol reasons for a persistent data problem.

**Monthly**

- [ ] Restore a backup on a test host.
- [ ] Review credentials and rotate if needed.
- [ ] Re-run representative backtests and compare with the previous month.
- [ ] Update dependencies deliberately, then run the full verification suite.
