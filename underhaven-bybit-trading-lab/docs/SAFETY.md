# Safety model

UNDERHAVEN is a **paper-only research platform**. Its safety claim is narrow and
deliberate: it cannot send an order to Bybit and it cannot be switched into a
mode that does. This document explains how that is enforced, what else is
protected, and what remains an operational responsibility.

## 1. The paper-only boundary

### What "paper only" means here

* All positions, fills, fees, funding and equity are rows in a local SQLite
  database. No exchange order, cancellation or position request exists.
* Market data is read from Bybit's **public** REST and WebSocket endpoints.
* The optional API key path performs only two signed read-only metadata calls
  (`/v5/account/fee-rate` and `/v5/user/query-api`), plus an unused read-only
  balance helper.
* There is no live toggle, no "go live" CLI flag, no feature flag, and no
  environment variable that enables trading. `UNDERHAVEN_TRADING_MODE` may only
  be `paper`; any other value aborts startup.

### How it is enforced

| Layer | Enforcement |
| --- | --- |
| Compile time | `PAPER_ONLY` constants in `lib.rs` and `model.rs`; no order-entry client type exists. |
| Module boundary | `paper.rs` writes to SQLite only. It does not import `PublicClient`, `reqwest` or `tungstenite`. |
| Adapter surface | `bybit.rs` implements only public market endpoints plus two signed read-only calls. There is no generic "signed POST" helper. |
| WebSocket surface | `ws.rs` subscribes only to `tickers`, `orderbook` and `kline`; it sends only `subscribe`, `ping` and `pong`. There is no private `auth` frame. |
| CLI surface | No `live`, `order`, `trade` or `withdraw` subcommand. |
| HTTP surface | Control actions are limited to pause, resume, emergency stop, clear emergency, reset paper lockouts and flatten a paper position. |
| Repository test | `tests/paper_only.rs` scans every `src/*.rs` file for order-entry endpoints and function names, verifies the WebSocket topic set, the control-action whitelist, the CSRF/session matrix, the credential read-only check ordering, and that the UI has no remote assets. |
| Audit ledger | `audit_events` has `BEFORE UPDATE` and `BEFORE DELETE` triggers that abort. The test exercises both. |

The source scan is intentionally blunt. If a future contributor needs a new
Bybit endpoint, the test fails and the change must be reviewed explicitly rather
than slipping in as a string.

### What paper-only does **not** mean

* It does not prove that a strategy is profitable.
* It does not reproduce queue position, exchange matching, liquidation,
  auto-deleveraging, or real latency.
* It does not protect against a compromised host, a stolen admin session, or a
  malicious operator. Those are covered separately below.

## 2. Authentication and session security

* **Passwords:** Argon2id, random 16-byte salt, default parameters (19 MiB,
  t=2, p=1). Minimum 12 characters, at least three of lowercase/uppercase/
  digit/symbol, and must not contain the username.
* **Sessions:** 32 random bytes, base64url. Only a SHA-256 hash is stored in
  SQLite. The raw token exists only in the browser cookie. TTL is 12 hours.
* **Cookies:** `HttpOnly; SameSite=Strict; Path=/`, plus `Secure` when
  `UNDERHAVEN_SECURE_COOKIES=true`. The production installer sets `true`.
* **Non-loopback bind:** the process refuses to start with insecure cookies
  unless `UNDERHAVEN_ALLOW_INSECURE_HTTP=true` is explicitly set. That override
  is for a trusted private network only.
* **CSRF:** a per-session token is returned by `/api/session` and required in
  `X-CSRF-Token` on every mutation except login.
* **Rate limiting:** failed logins are counted per client IP before Argon2 is
  invoked; the default is 5 failures per 15 minutes. Login work is equalised for
  unknown usernames with a dummy Argon2 verification.
* **Revocation:** logout revokes the current session; changing the password
  revokes all sessions for that user and clears the cookie.
* **Audit:** login success/failure, logout, password changes, control actions,
  credential changes and engine lifecycle events are appended to `audit_events`.

### Reverse-proxy trust

The app binds loopback behind nginx by default. Because loopback is the only
trusted proxy, `X-Forwarded-For` is trusted only from it. The shipped nginx
configs **overwrite** `X-Forwarded-For` with `$remote_addr` instead of appending
a client-supplied value, so an external request cannot spoof the rate-limit key.
If you front the app with another proxy, configure equivalent overwrite
semantics.

## 3. Credential handling

The Bybit key is optional and is never required for paper trading.

1. The key is submitted only over the authenticated HTTPS dashboard.
2. The server verifies it with `/v5/user/query-api`.
3. If Bybit reports anything other than `readOnly=1`, the key is rejected and
   **nothing is written**.
4. On success, key and secret are encrypted with AES-256-GCM under a 32-byte
   master key file outside SQLite.
5. The plaintext secret is not logged and is not returned by any endpoint.
6. The paper engine cannot read the encrypted credential row and never uses it.

Operational rules:

* Create a Bybit **read-only** API key. Do not enable trading, withdrawal or
  transfer permissions, even though the app checks the read-only flag.
* Keep `/etc/underhaven/master.key` at `root:underhaven 0640`. Back it up
  separately from the database: an encrypted credential is useless without it,
  and possession of both is equivalent to possession of the key.
* Rotate the key at Bybit and re-enter it if you suspect exposure. Deleting it in
  the dashboard removes the encrypted row.
* If the master key is lost, delete the stored credential and re-enter it. The
  rest of the ledger is unaffected.

## 4. Network exposure

Recommended production shape:

```
Internet :443 ──> nginx (TLS) ──> 127.0.0.1:8080 underhaven
Internet :80  ──> nginx (ACME challenge + redirect)
```

* Only 80/443 need to be public. The application port stays loopback.
* The systemd unit uses `NoNewPrivileges`, `ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, a dedicated user, and a writable path limited to
  `/var/lib/underhaven`.
* The service reads the master key from `/etc/underhaven`, which the service
  user cannot modify.
* Security headers include a strict CSP (`default-src 'self'`), `nosniff`,
  `DENY` framing, `no-referrer`, a restrictive permissions policy and HSTS when
  Secure cookies are enabled.
* The dashboard has no CDN, no third-party script and no inline script. uPlot is
  vendored.

## 5. Financial safety controls

Even in paper mode, the engine enforces the same discipline expected of a live
system, so forward-test results are not produced by reckless sizing:

| Control | Default |
| --- | --- |
| Risk per trade | 0.5% of strategy equity |
| Daily loss lockout | 2% |
| Peak drawdown lockout | 10% |
| Maximum open positions | 2 (global), 1 per strategy |
| Leverage | 1x (validated range 1–10, default 1) |
| Mandatory stop | Yes; startup fails if disabled |
| Maximum total notional | 100% of equity |
| Correlated notional cap | 60% |
| Consecutive-loss cooldown | 5 losses, then 720 minutes |
| Maximum position age | 24 hours |
| Stale-data gate | 5,000 ms |
| Spread gate | 8 bps |
| Depth gate | 25,000 USD per side within 10 bps |

The **emergency stop** flattens every open paper position at the current quote
and blocks new entries until explicitly cleared. `pause` blocks new entries but
leaves existing positions and their stops under management. Lockouts are
persisted, so a restart does not silently reset a daily-loss or drawdown lock.

## 6. Ledger integrity

* Each fill has an idempotency key; replaying an entry/exit does not double-book
  cash.
* Funding settlements are idempotent on `(strategy, symbol, position_id,
  funding_time_ms)`.
* Wallet mutations and position changes occur in transactions.
* On startup, the engine reconciles wallets against open/closed positions and
  reports any mismatch instead of hiding it.
* `audit_events` is append-only at the database level.
* `underhaven backup` uses SQLite `VACUUM INTO` for a consistent copy while the
  service is running.

## 7. Operational responsibilities

The software cannot protect you from:

* a compromised VPS, SSH key or root account;
* a weak or reused admin password;
* exposing port 8080 directly to the internet;
* disabling Secure cookies on a public address;
* trusting a self-signed certificate without verifying its fingerprint;
* losing the master key or the database without a backup;
* misinterpreting paper results as evidence of future returns.

Run the service behind TLS, restrict SSH, keep the host patched, monitor
`journalctl -u underhaven`, and test restore procedures. See
[`DEPLOY.md`](DEPLOY.md) and [`OPERATIONS.md`](OPERATIONS.md).

## 8. Incident response (short form)

1. **Stop new risk:** dashboard → G → *Emergency stop*. This flattens paper
   positions. It does not contact Bybit.
2. **Stop the process:** `sudo systemctl stop underhaven`.
3. **Preserve evidence:** copy the database and recent journal output before
   changing anything.
4. **Revoke access:** change the admin password, delete the stored Bybit key at
   Bybit, and rotate `/etc/underhaven/master.key` if credential exposure is
   possible.
5. **Restore:** restore a known-good database with the service stopped, then
   start it and verify `/healthz`, the ledger reconciliation and the audit trail.

## 9. Reporting a boundary defect

If you find any code path that can submit an order, open a private security
report rather than a public issue. Include the file, the call path and whether
any credential was used. The repository test is the first line of defence, not
a substitute for review.
