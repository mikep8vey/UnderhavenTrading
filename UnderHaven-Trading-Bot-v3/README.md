# UnderHaven Trading Bot v4.0

Scans **BTC "Up or Down" 5-minute and 15-minute markets** on **Polymarket** and **Kalshi**, evaluates several strategies on **real order books**, records realistic **paper** results on your VPS, and has a **carefully gated live mode** (off by default) for a later small experiment.

Nothing in this project generates fake prices, fake order books, random fills or invented profit. When there is nothing to trade the dashboard says so and shows why.

## Honest status (read this first)
| Area | Status |
|---|---|
| Shared fee / depth / P&L engine, strict matcher, strategies, paper broker, settlement, risk, persistence, dashboard | Built and covered by 60 automated tests (offline, fixture exchanges) |
| Real Polymarket / Kalshi **market data** parsing | Written to the current official docs, **not exercised against the live APIs from the build environment** (no internet there). First run on your VPS is the real test: check `/markets` and `/health`. |
| Live order placement (Polymarket SDK, Kalshi signed API) | Implemented, unit-tested against scripted responses, **never run against a real exchange**. Test Kalshi on its **demo** environment first. Kalshi's newer "V2 orders" endpoint is **not** implemented; the documented legacy `POST /portfolio/orders` is. |
| Kalshi 5-minute BTC market | None found in the docs; the bot discovers dynamically and reports "not listed" rather than assuming. |
| Cross-platform arbitrage | A pair is tradable only if settlement rules are PROVEN identical. Typical real pairs differ (Polymarket: Chainlink point price; Kalshi: 60-second average of a CF Benchmarks index), so **expect Strategy A to find no verified pairs**. Research mode (paper only) can measure how often the venues disagree. |
| Profitability | **Unproven.** Earlier scans of Polymarket BTC 15-minute markets found no executable gap after fees. Momentum / mean-reversion are hypotheses to be tested by forward paper trading, not validated edges. |

## Strategies (each has its own switch, stats, rejection reasons, and per-duration results)
- **A Cross-platform arbitrage**: Polymarket UP + Kalshi DOWN (or the reverse) on a VERIFIED pair. Profit per matched pair = `1.00 - (ask_A + ask_B) - fees - slippage - safety`, from actual ask depth.
- **B Momentum**: probability that BTC finishes the window above its start (distance, short-term volatility, optional drift, time left) vs the executable ask.
- **C Mean reversion**: fade a market jump of the UP price that BTC itself did not make and the model does not confirm.
- **D Order-book quality / imbalance**: filter on everything (fresh, valid, spread, depth) and an optional small probability adjustment (`imb_weight`, default 0).
- **E Expiration-aware entry window**: per duration, minimum elapsed / remaining time, including latency and exchange order delay.
- **F Coordinator**: ranks by expected net dollars x quality, blocks duplicate exposure, applies risk, records every accept / reject with the reason.
- **G Same-platform bundle (retained from earlier versions)**: Polymarket UP + DOWN asks for less than $1 after fees.

The BTC reference price (Coinbase, Kraken, Binance fallbacks) is the bot's own input; it is **not** either exchange's settlement source, so signals near the strike carry basis risk.

## Money rules (single module `app/btc/calc.py`, Decimal arithmetic)
- Buys use **asks**, sells use **bids**, walked level by level for the real quantity; never mid-prices or last trades.
- Polymarket fee `C x rate x (p(1-p))^exponent`, taker only, rate/exponent from the market's `feeSchedule` when stated (else the documented 0.07, labelled an estimate). Kalshi fee `ceil_to_cent(M x 0.07 x C x P x (1-P))` per order. Fees are estimates until an exchange reports the real fee.
- Polymarket `orderMinSize` is a USDC **notional** (fixed in v4; earlier versions treated it as shares).
- P&L = proceeds - exit fees - **all-in paid** (cost basis + entry fees). A position is never "won" until it is sold or the exchange reports its result.
- Regression fixture for the corrected exit example (97.0c + 2.0c, 103 contracts): `tests/test_calc.py`. The stated $99.85 cost, $99.99 paid and +$1.97 P&L cannot all be true: net P&L from the fills is +$1.83 (`101.97 - 0.15 - 99.99`). No such exit card exists in this repository; the engine now produces reconciled numbers.

## Paper trading (default, always on)
Real books are re-fetched at "execution time" after a configurable latency (+ the market's own order delay). Fills consume only a fraction (`depth_haircut`, default 0.7) of displayed depth, successive paper orders cannot reuse the same liquidity within 5 s, first leg is FOK, second leg IOC with an explicit recovery policy, fees and slippage are applied, rejected/unfilled/partial orders are recorded. Separate balances per platform (`balance_polymarket`, `balance_kalshi`, default $20 each, locked once trading starts). Exact queue position and market impact cannot be known from public snapshots: paper fills are **estimates**.

## Risk controls (enforced in the backend, paper and live)
Max order, capital per trade, exposure per platform and total, open positions, daily loss, drawdown, spread, minimum depth, data age, minimum net edge, unmatched-leg exposure, retries, order rate. Failing or unavailable data rejects the trade. **Kill switch** (persisted across restarts): stops new entries; policy `hold_to_settlement` (default) keeps positions, `unwind_open` also sells open single positions. No martingale, averaging down or doubling.

## Live mode (disabled by default and after every restart)
1. Live page: connect Polymarket (wallet + funder + signature type) and add Kalshi credentials (key id + RSA private key; stored **encrypted** server-side, never shown again, deletable). Use Kalshi **demo** first.
2. Set per-account limits (Settings): defaults are $5 per order, $10 open, $3 daily loss on each platform. Tick "live permission" for chosen strategies (none by default). Unverified pairs can never go live.
3. Press **Run preflight** (connectivity, balances, markets, matching, limits, fee handling, warnings). Enabling requires every hard check to pass **and** typing `ENABLE LIVE TRADING`.
4. Emergency stop (top bar) disables live mode and trips the kill switch.
Cross-exchange trades are not atomic: leg 1 FOK, leg 2 IOC, then retry/unwind per `recovery`; unresolved exposure raises a `MANUAL_ACTION_REQUIRED` alert and the kill switch. Ambiguous submissions are reconciled by client order id and never re-sent blindly.

## Install on Ubuntu / update
```bash
sudo bash install.sh              # first install (nginx + systemd; dashboard on https://<ip>/)
cd ~/<repo>/UnderHaven-Trading-Bot-v3 && git pull && sudo bash update.sh    # update (database migrates in place)
sudo systemctl status underhaven-trading ; journalctl -u underhaven-trading -f
sudo bash uninstall.sh --dry-run  # then without --dry-run to remove everything
```
The scanner runs inside the `underhaven-trading` systemd service (restart on failure, independent of any browser). A lock file prevents a second scanner process. The app listens on 127.0.0.1 only; nginx provides public HTTPS.

## Monitor paper trading
Overview (status, live windows, last-24h detections / rejections / reasons, balances, P&L), Markets (every window, books, entry status, pair verification checks), Strategies (per-strategy and per-duration results), Paper trading (orders, fills, trades, equity curve, drawdown, CSV exports). `GET /health` (logged in) and `/ready` for monitoring.
**Before any live experiment:** run paper for at least 1-2 weeks; require many completed trades per strategy (the dashboard warns below 30), positive net P&L after fees, drawdown within your limit, and review rejection reasons, order fill rates and any `SETTLEMENT_MISMATCH` events. Treat anything less as anecdote.

## Backup, tests, troubleshooting
- Backup: `sudo sqlite3 /var/lib/underhaven-trading/underhaven.db ".backup /root/uh-backup.db"` (also keep `/etc/underhaven-trading/vault.key`; without it stored credentials cannot be decrypted).
- Tests: `python3 tests/run_tests.py` (60 tests, offline). Optional browser layout check: `python3 tests/layout_check.py`.
- "NOT UPDATING" on Overview: see `journalctl -u underhaven-trading`, check `/health` `errors`. "No Kalshi market": Kalshi may not list that duration right now. Forms say "out of date": reload (CSRF token).
