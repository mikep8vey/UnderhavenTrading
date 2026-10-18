# weather-bot

Polymarket weather trading bot in Rust. Two strategies share one maker-first execution layer.
**Paper mode is the default.** Live mode needs `--enable` plus keys in the environment.

> **Status: written but not compiled or run by its author's tooling.** The code was produced in an
> environment with no Rust toolchain and no network. Expect to fix a few compile errors on first
> `cargo build`, and treat the live-trading path (EIP-712 order signing, API-credential derivation, the
> Gamma/CLOB/Open-Meteo response shapes) as **unverified until you have tested it with dry-run and then a
> tiny live order**. Nothing here is a claim that the strategies are profitable.

## Strategies

### 1. Forecast edge (morning/midday)
For every bracket of every tradable event (today before `cutoff_hour_local`, and tomorrow), the bot pools
all ensemble members (ECMWF IFS, ECMWF AIFS, GEFS, ICON via Open-Meteo, **airport** lat/lon), takes each
member's daily extreme in the market's unit, applies `floor()`, and tallies per bracket → `P_model`.
It evaluates YES and NO and picks the better side:

`edge = P_model − ask − taker_fee(ask)`; trade only if `edge > 0.08`, `spread < 0.03`, and ask depth ≥ 3× size.
Size = `0.25 × Kelly`, capped at 5% of bankroll. If the market has no `feeSchedule`, the fee term is 0 and
taker fallback is disabled for that market.

### 2. Late-day NO harvester (afternoon/evening)
Every 30s the bot rebuilds the **station-local** running high/low from METAR/SPECI. A bracket whose range the
running extreme has already crossed becomes **PROVISIONALLY_DEAD**. If the NO ask is in 0.70–0.99 (and ≥ 0.10,
spread ≤ 0.03) the bracket must pass the settlement-safety check before any order is placed.
Size = `0.15 × Kelly` (with `P(NO) = harvester.model_prob`), capped at 8%.

**Crossing rule (a deliberate deviation from the spec text).** The spec says "running_high ≥ bracket_max". In
floor space that would mark a bracket dead while the day's max is still *inside* it (e.g. running high 72.0 in
a "72" bracket). The bot uses `floor(running_high) > bracket.hi` (HIGH markets) and
`floor(running_low) < bracket.lo` (LOW markets). Open-ended brackets cannot die on the side they are open.

## Provisionally-dead rule: SETTLEMENT_SAFETY_CHECK
All must pass, otherwise `SETTLEMENT_UNSAFE` is logged and the bracket is skipped:
1. Market rules text names the registry station (and no decoy airport) — else `MANUAL_REVIEW_REQUIRED`.
2. Running extreme is from the station's climate day, which equals the market date and "today" there.
   NWS CLI stations use the **local standard-time** day all year (`day_basis = "lst"` in `cities.toml`);
   Europe uses the civil local day.
3. The resolution source's latest bulletin (NWS CLI) is for the same date and within `settle_tolerance` (0.5)
   of the METAR extreme.
4. The bracket's kill boundary is ≥ `boundary_margin` (0.5) from the running extreme.
5. Market is active, not closed, and accepting orders.

**Consequences you should know about:** Paris and London have no fetchable resolution-source bulletin in this
build (`pils = ""`), so check 3 fails closed and the harvester never trades them; they are forecast-edge only.
For US cities the latest CLI must already be dated today (intraday CLI issuance), and METAR (whole °C or tenths)
converted to °F will often disagree with the CLI by more than 0.5°F — the bot will skip those. That is
the intended fail-closed behaviour; loosen `settle_tolerance` only knowingly.

## Station registry
`config/stations.toml` is the hard-coded table from the spec; airport coordinates only, never city-center.
`cities.toml` adds timezone, unit, day basis, accepted station-name spellings and decoy airports.

> **Check this one:** Dallas is registered as Love Field `KDAL` with PIL `CLIDFW`, exactly as specified. `CLIDFW`
> is, as far as I know, the Dallas/Fort Worth (DFW) climate bulletin, and Love Field's is `CLIDAL`. If I'm right, the
> CLI check will disagree with Love Field METAR and fail closed (safe, but the harvester will not trade Dallas).
> Verify the PIL on the NWS/IEM site before changing it.

## Maker-rebate execution layer (`src/execution/`)
- Every order is posted **post-only GTC** first (`OrderBuilder::maker` hard-codes `post_only = true`).
- Taker fallback exists only with `--allow-taker`, after the timeout (60s forecast / 45s harvester), and only if
  the strategy's rule still holds (forecast: `fair − ask − fee > 0.10`; harvester: ask still ≥ 0.85). Every override
  is logged (`TAKER_OVERRIDE`) with fair value, fee and rule.
- Quotes are cancelled **by id** and reposted when fair value moves > 0.01 or every 10s; max quote age 15s.
- Protection: spread ≤ 0.03 and ask depth ≥ 3× size or the quote is pulled. Everything is cancelled on a crypto
  1-minute range > 1.5% (BTC/ETH/XRP, Binance klines), a METAR swing > 2°C in 15 min, or the drawdown kill switch.
  Volatility guards pause for `pause_secs`; the drawdown switch (15%) is permanent and writes a `KILL_SWITCH` file
  next to the database that blocks restarts until you delete it.
- Fees are read from each market's `feeSchedule` (`rate`, `exponent`, `rebateRate`); none are hard-coded.
  Fee per share is modelled as `price × rate × (price·(1−price))^exponent`. Confirm this against Polymarket's current fee
  docs; it only affects edge/rebate estimates.
- Rebates: each maker fill adds its notional to `maker_volume` and `rebateRate × fee-equivalent` to `accrued_usdc`
  (table `rebates`). It is an **estimate**; Polymarket pays the actual rebate. Shown as `accrued_rebate_usdc`.
- Two-sided quoting is not enabled; only the `two_sided_sum_ok` guard (sum ≤ 0.99) is included.

## Other deviations from the spec
- Added `src/lib.rs` and `src/app.rs` (integration tests need a library crate).
- Rule 4's `taker_fee(P_model)` uses the execution price (a fee is a function of the price traded).
- The bot only buys and holds to settlement; there is no exit logic.
- Hourly Open-Meteo data slightly understates true intraday highs; consider a bias term after calibrating.
- `crypto vol` is a 1-minute candle-range proxy, not tick-level realized volatility.

## Paper and live accounts
Both accounts run side by side on the **same** signals, edge and probabilities. Each has its own orders, fills, positions,
rebate and equity tables (`paper_*`, `live_*`), exposure limits, kill switch and daily loss cap. Sizing uses each account's own
equity (`bankroll_usdc` for paper, `live_bankroll_usdc` for live). `weather-bot dry-run` runs paper only; `weather-bot live --enable` runs both.

## Calibration and shrinkage (forecast strategy)
1. **Calibration.** Every scan logs the raw model probability next to the market price (`forecast_log`). After a market settles
   its outcome is filled in, and a reliability table (ten probability buckets, observed vs predicted) is learned from it. It stays off
   until `calibration_min_samples` (100) forecasts have settled, and each bucket is pulled toward the raw value by a prior.
2. **Shrinkage.** `p_yes = w * calibrated_model + (1 - w) * market_yes_mid`, default `w = 0.3`, so the bot only acts on the part of its
   view that the market does not already price.
3. **Guards.** No NO-buys on a bracket the market prices above 30% YES (`no_max_yes_mid`) and no entries priced below 5 cents
   (`min_entry_price`). These came from the first paper run: three of four NO positions on the market's favourite bracket lost their whole stake.
4. The dashboard's *Calibration* tab shows Brier scores for the raw model, the market and the blend. If the model does not beat the
   market there, do not fund the live account.

## Daily loss cap and retry limit
Per account: no new trades for the rest of the UTC day after a 5% loss since the day's start (`daily_loss_cap_pct`), and at most 3 unfilled
attempts per market per day, 15 minutes apart (`max_attempts_per_market`, `retry_cooldown_secs`).

## Dashboard
Built into the binary (`src/dashboard/`): login page, Argon2-hashed admin credential chosen at install time, session cookies
(HttpOnly, Secure, SameSite=Strict), login lockout, and a read-only UI with a Paper/Live switch, equity chart, exposure by city,
positions, signals, orders, fills, events and calibration. It binds to 127.0.0.1; Caddy serves it over HTTPS with an automatically
issued, browser-trusted certificate (installer sets it up).

## Layout
See `BUILD.md` for build, run, SQLite and uninstall instructions.
