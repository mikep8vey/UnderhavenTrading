# UnderHaven Trading Bot 3.0

UnderHaven Trading Bot is a browser-operated Polymarket arbitrage research and execution console designed for an Ubuntu VPS. It implements the strategy from the public BORED-GROK-ARBITRAGE architecture, but upgrades the core detector so it evaluates **executable depth, protocol fees, net edge, and risk controls** before a trade is considered.

## What this release changes

- Fixes the previous `404` Let's Encrypt ACME challenge problem by serving the challenge directory with an explicit Nginx `alias`, using a catch-all default HTTP server, reloading Nginx before testing, and performing both local and public HTTP preflight checks.
- Installs current Certbot automatically and requests a trusted Let's Encrypt **IP-address certificate** for the VPS IPv4. IP certificates are short-lived, so renewal is automated twice daily and Nginx is reloaded after renewal.
- Installs the application under `/opt/underhaven-trading` instead of `/root`, so the systemd service can run correctly as the `underhaven` user.
- Prompts for the administrator username and password, then asks for the password **twice** and refuses mismatches.
- Starts the scanner as a persistent systemd service. Closing the browser, logging out, sleeping, or shutting down the PC does not stop the VPS bot.
- PAPER mode starts automatically and persists a paper-study session in SQLite.
- Records qualifying opportunities, simulated paper trades, timestamps, spend, fees, net edge, simulated P&L, win rate, and session totals.
- Adds CSV export of the paper opportunity history so a 24-hour study can be downloaded.
- Uses BTC Up/Down **15-minute markets only** by default. 5-minute markets are excluded.
- Uses best executable asks and walks the order book to estimate the VWAP for the configured trade size instead of assuming the first quote is available for the entire trade.
- Uses Polymarket's documented taker-fee formula when screening taker execution. Current published crypto taker rate is 0.07; sports is 0.05. The implementation also attempts to read an explicit per-market fee parameter when exposed by market metadata.
- Requires the configured **net edge** to exceed the safety margin before a candidate is qualified.
- Live execution is OFF by default.
- Live orders use equal-share FOK limit orders on both legs rather than spending the same USDC amount on both sides. This keeps the pair quantities matched.
- If the first leg is confirmed but the second leg is not confirmed, live mode is immediately disabled and the event is recorded for recovery. The system does not label that state risk-free.
- Adds a daily live-loss kill switch.
- Adds optional best-effort automatic merging after both live legs are confirmed, using Polymarket's current Python SDK transaction workflow. A merge failure is recorded and never counted as profit.
- Keeps credentials/private keys/seed phrases encrypted on the VPS rather than in Git.
- Includes the UnderHaven logo and favicon.

## Important: what the bot can and cannot guarantee

The bot is deliberately built around this philosophy:

> **Don't trade because an opportunity looks profitable. Trade only when the currently executable order-book data supports a sufficient net arbitrage edge after costs and configured risk controls.**

It cannot honestly guarantee that every live trade will be profitable. A price gap can disappear between observation and submission, a leg can fail to fill, protocol fees can change, an order can be matched before settlement is visible, or a transaction can fail. The bot therefore refuses candidates that do not clear the configured net-edge threshold and uses FOK execution, a daily loss stop, and persistent trade logs.

The paper-study P&L is **hypothetical**. It assumes both equal-share legs can be acquired at the measured executable depth and that the resulting pair can be redeemed/merged for $1 per share. It is not a promise of live P&L.

## Recommended rollout

1. Install on a fresh Ubuntu VPS.
2. Open the HTTPS URL printed by the installer.
3. Leave the bot in PAPER mode for at least 24 hours.
4. Study the opportunity history, simulated P&L, win rate, fee estimates, and rejected/qualified conditions.
5. Connect your Polymarket signer and verify that the displayed collateral balance matches the intended trading account.
6. Keep LIVE disabled until you understand the paper results.
7. If you decide to test live execution, use only a small amount such as $10–$20 that you are prepared to lose.
8. Inspect actual fills, fees, unmatched-leg events, merge results, and realized account P&L.
9. Only then consider increasing size.

## Polymarket authentication

Polymarket's current CLOB authentication has two layers:

- L1: a wallet/private key signs an authentication message.
- L2: the wallet authentication creates or derives an API key, secret, and passphrase for authenticated CLOB requests.

UnderHaven derives those credentials automatically after you enter the signing private key.

### Is the private key alone enough?

**Sometimes, but not always by itself.** A direct EOA can often use its private key as the signer and funded wallet. Polymarket also supports proxy/email/smart-wallet models where the **signer address** and **funder/deposit wallet** can be different. UnderHaven therefore asks for both when necessary and lets you select the account/signature type.

For a normal direct EOA:

```text
Private key: your signing key
Funder: leave blank unless Polymarket tells you otherwise
Signature type: 0 — direct EOA
```

For a Polymarket email/Google/Magic-style proxy account, the exported signing key may be different from the wallet address that holds the funds. Use the corresponding funder/profile/deposit wallet shown by Polymarket and the matching signature type.

**Never paste a private key or seed phrase into ChatGPT, GitHub, a support ticket, or a public file.** If a key was exposed, treat that wallet as compromised and do not fund it.

## 24-hour paper mode

The bot runs continuously from systemd. The dashboard is only the display/control surface.

Every scanner session has:

- session start/end time
- scan count
- qualifying opportunities
- simulated paper trades
- wins/losses
- simulated spend
- simulated P&L
- opportunity timestamps
- YES/NO executable asks
- order-book liquidity
- gross edge
- fee estimate
- net edge
- executable share count
- executable spend

Use **Export Paper Opportunity CSV** to download the opportunity history.

A high paper win rate does not prove that live orders will have the same fills. The purpose of the paper period is to validate the detector and execution assumptions before risking money.

## Strategy engine

For a binary YES/NO market:

```text
pair cost = executable YES ask + executable NO ask
gross edge = 1.00 - pair cost
net dollars = shares - YES cost - NO cost - protocol fees
net edge = net dollars / shares
```

The engine walks available asks so a $10 trade is not incorrectly priced from a quote that only has $0.20 of liquidity behind it.

For taker screening, Polymarket documents:

```text
fee = C × feeRate × p × (1 - p)
```

where `C` is shares and `p` is share price. Makers are not charged taker fees, but maker execution is not treated as risk-free: a resting order can fill one leg without the other. UnderHaven therefore defaults to taker-style executable screening for the BTC arb and requires the net edge to survive the current fee estimate.

## Live execution

LIVE is disabled after installation.

When enabled, the engine:

```text
observe order books
    ↓
calculate executable depth
    ↓
calculate gross edge
    ↓
calculate fee estimate
    ↓
calculate net edge
    ↓
apply safety margin
    ↓
re-check order books
    ↓
FOK BUY YES for N shares
    ↓
confirm
    ↓
FOK BUY NO for the same N shares
    ↓
confirm
    ↓
optionally attempt merge
    ↓
record result
    ↓
continue scanning
```

The bot does not require the browser to remain open.

### Important two-leg limitation

There is no magic atomic guarantee across two independent CLOB orders. Even with FOK orders, the first leg can be confirmed while the second leg fails. UnderHaven stops live execution and records the event instead of pretending the position is risk-free.

## Automatic merge

The current Polymarket Python SDK supports position merge workflows. UnderHaven's `auto_merge` option attempts a merge after both live legs are confirmed.

A merge is a blockchain/relayer transaction, so it is treated as a separate state:

- `MERGE_SUBMITTED`
- `MERGE_FAILED`

A successful merge is not counted as realized profit until the underlying account/transaction state supports that conclusion.

## HTTPS by VPS IP

No domain is required.

The installer detects the public IPv4 and configures:

```text
http://YOUR_VPS_IP
https://YOUR_VPS_IP
```

Let's Encrypt now supports public IP-address certificates. IP certificates use the short-lived profile and are valid for about 160 hours. Certbot 5.4+ supports the webroot flow used here. The installer runs a staging preflight first and then requests the trusted production certificate.

If the ACME preflight fails, check that TCP/80 is reachable from the Internet before retrying.

## Installation

On a fresh Ubuntu VPS:

```bash
chmod +x install.sh
sudo ./install.sh
```

The installer:

1. installs Python, Nginx, Snap/Certbot and dependencies;
2. installs the current Polymarket clients;
3. copies the release to `/opt/underhaven-trading`;
4. asks for admin username;
5. asks for admin password;
6. asks for the password again;
7. creates the encrypted credential vault;
8. detects the public IPv4;
9. configures Nginx HTTP challenge handling;
10. tests the ACME challenge path;
11. requests the Let's Encrypt IP certificate;
12. configures HTTPS and HTTP→HTTPS redirect;
13. creates the persistent systemd bot service;
14. creates the twice-daily certificate renewal service;
15. starts PAPER mode.

At the end it prints the HTTP and HTTPS URLs.

## Service commands

```bash
sudo systemctl status underhaven-trading
sudo systemctl restart underhaven-trading
sudo systemctl stop underhaven-trading
sudo journalctl -u underhaven-trading -f
```

Certificate:

```bash
sudo systemctl status underhaven-certbot.timer
sudo systemctl start underhaven-certbot.service
sudo certbot certificates
```

## Update

```bash
sudo ./update.sh
```

The update script preserves `/var/lib/underhaven-trading`, including the paper history and encrypted credentials.

## Uninstall (full wipe)

```bash
sudo bash uninstall.sh              # asks you to type DELETE
sudo bash uninstall.sh --dry-run    # shows exactly what would be removed, changes nothing
sudo bash uninstall.sh --yes        # no prompt
sudo bash uninstall.sh --keep-data  # keep the database + vault key (trade history, saved wallet)
sudo bash uninstall.sh --keep-source  # do not delete this folder / git clone
```

By default this removes everything UnderHaven created: the systemd services, `/opt/underhaven-trading`,
the database and scan history (`/var/lib/underhaven-trading`), the vault key (`/etc/underhaven-trading`),
the nginx site, the Let's Encrypt IP certificate, the `underhaven` user, and the git clone the script is
running from. nginx, Certbot and firewall rules for ports 80/443 are left alone because other things may use them.

**Back up any saved wallet's private key / seed phrase first.** Once the vault key is deleted, the encrypted copy
in the database cannot be recovered.

It also works after the clone is gone: `sudo bash /opt/underhaven-trading/uninstall.sh`.

Clean reinstall of the latest version:

```bash
sudo bash uninstall.sh --yes
cd ~ && git clone <your-repo-url> && cd <repo>/UnderHaven-Trading-Bot-v3 && sudo bash install.sh
```

## Source strategy

The implementation follows the architecture of the public repository:

urlBORED-GROK-ARBITRAGEhttps://github.com/bored2boar/BORED-GROK-ARBITRAGE

That repository describes the watcher → arbitrage engine → risk checks → executor → merge flow and explicitly states the `BOTH LEGS OR NEITHER` invariant. UnderHaven extends that architecture with persistent VPS operation, a web dashboard, encrypted credentials, 24-hour paper telemetry, executable-depth pricing, current fee calculations, HTTPS installation, and operational controls.

## Official current references

- urlPolymarket API authentication documentationhttps://docs.polymarket.com/getting-started/api
- urlPolymarket feeshttps://docs.polymarket.com/trading/fees
- urlPolymarket Python SDKhttps://github.com/Polymarket/py-sdk
- urlPolymarket CLOB client v2https://github.com/Polymarket/py-clob-client-v2
- urlLet's Encrypt IP certificateshttps://letsencrypt.org/2026/01/15/6day-and-ip-general-availability
- urlCertbot IP certificate supporthttps://letsencrypt.org/2026/03/11/shorter-certs-certbot

## v3.2.0 — VPS SQLite/502 fix

This release fixes a first-launch failure where the installer initialized the SQLite database as `root`, while the systemd service runs as the `underhaven` user. SQLite then reported `attempt to write a readonly database`, causing Flask to exit and Nginx to return `502 Bad Gateway`.

The installer now:

- initializes the database as the `underhaven` service user;
- re-applies ownership to `/var/lib/underhaven-trading` and `/etc/underhaven-trading`;
- preserves SQLite WAL write permissions;
- waits for `/health` before declaring the installation successful;
- prints the service log automatically if the backend fails to start.

The TLS certificate and Nginx configuration are unchanged from v3.1 because the certificate installation was already successful.

## v3.3 dashboard update
- Reworked the dashboard into a responsive two-column trading console so key sections are visible without excessive vertical scrolling.
- Added a clear PAPER/LIVE mode switch at the top of the dashboard.
- Combined Polymarket connection, authenticated balance, and wallet controls into one panel.
- Added a persisted opportunity/P&L activity chart using the VPS database history.
- Added a sharper dark/glass visual theme, responsive tables, compact cards, and clearer execution/risk indicators.
- The trading engine and paper/live execution behavior are unchanged by the visual redesign.

## v3.4 dashboard/error-handling update

- Added a compact dark trading-terminal dashboard with Paper/Live mode indicator.
- Added persistent paper-study evidence: observed opportunities, hypothetical pair trades, simulated P&L and a net-edge chart.
- Added automatic dashboard refresh while the browser tab is visible.
- Added a friendly HTTP 500 diagnostics page and server-side traceback logging. If a dashboard load fails, the browser now shows a diagnostic ID and the full traceback is available with `journalctl -u underhaven-trading`.
- Paper-mode results are explicitly labeled as simulated/hypothetical. They are useful for evaluating the strategy and execution logic, but are not proof of realized live profitability.

## v3.5 report + layout update

* **Runs 24/7.** The bot is a systemd service (`Restart=always`, enabled at boot), so it keeps scanning when you close the
  browser, log out of SSH or reboot the VPS. After any restart it comes back up in **PAPER** mode; LIVE must be switched on again by hand.
* **Report page (`/report`, "Report" button).** Open it after sleeping: pick *Since my last login*, 6h, 12h, 24h, 48h or 7d.
  Shows scans run, uptime coverage, best gross/NET edge, how many scans had a qualifying gap, paper trades and simulated P&L,
  an hour-by-hour table, the best moments, recent errors and an Export CSV. History is stored as one summary row per minute and kept 30 days.
* **Layout fix.** The dashboard no longer overflows the screen on laptops, wide monitors or phones.
* **Full uninstall** (`uninstall.sh`, see above).

## v3.6 multi-coin update

**Coins.** BTC, ETH, SOL, XRP, DOGE, HYPE and BNB 15-minute Up/Down markets (`<coin>-updown-15m-<start>`), one table per coin on
the dashboard and one shared Trade log (filter by coin). Turn coins on/off under *Strategy & Risk Controls*.

**When it trades.** Paper mode and LIVE mode use the same rules, for every coin, whenever a gap appears:
1. NET edge (after fees and your slippage allowance) is at least your minimum;
2. it is not in the last N seconds of the window and meets the market's minimum order size;
3. it is profitable even if every share fills at the *worst* price the order book would force (profit lock);
4. both books are fetched again moments later and the gap is still there (re-check; LIVE always requires it);
5. at most one trade per market window.
LIVE additionally sends both legs as equal-size FOK orders, sells the first leg back if the second fails, books that loss against
the daily-loss stop, and switches itself off after any failure. None of this can guarantee a profit: fills, fees and timing can
still go against you. Keep PAPER mode until the Report shows gaps that *survive* the re-check over several days.

**Stability.** One pooled HTTP session and one cached SDK client replace the per-request clients that leaked file descriptors
("Too many open files"); a missing order book ("no book yet") is no longer counted as an error; books are fetched in one batched
request (falling back to parallel requests); `update.sh` raises the service's open-file limit. `/health` (when logged in) shows
`open_files`, `scan_secs` and `books_via` so you can see this working.

**Report.** Per-coin table and filter, "looked profitable -> survived" counts, "when gaps appear" chart, CSV export per coin.
Gaps that vanish on the re-check are kept out of the best-edge figures.

**Security.** CSRF token on every form, login throttling, `HttpOnly`/`SameSite` cookies, no-store caching for pages, minimal public `/health`.

**Wallet.** Compact wallet box, readable balance, funder/signature-type form that reuses the stored key, funder required for types 1-3.

**Tests.** `python3 tests/run_tests.py` (offline; uses a simulated exchange).
