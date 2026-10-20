# Changes in this build

## Execution

- **Depth gate split.** `maker_depth_multiple = 0.0` (off by default: a passive bid does not consume
  ask liquidity) and `taker_depth_multiple = 3.0` (the optional taker fallback still needs 3x ask depth).
  Both live in `[execution]` in `config/risk.toml`.
- **No timer repricing.** A resting quote is cancelled only when the price it would post changes, a book
  guard fails, or the hard TTL (`max_quote_age_secs = 600`) expires. `reprice_secs = 90` is now only the
  fair-value refresh cadence. The old `10s / 15s` churn is gone.
- **Paper fills from the public tape.** A resting buy fills when a `SELL` print at or below its price
  appears after placement (`data-api.polymarket.com/trades?market=<conditionId>`), capped by the print
  size. This replaces the incorrect `best_ask <= quote` rule. New `endpoints.data_api_url`.
- **`open` set leak fixed** when a fill/cancel is discovered through `status` rather than `cancel_sync`.

## Latency

- **Fast order loop.** `execution.poll_ms = 50` replaces the fixed 2s sleep between order/book/tape checks.
  The paper-tape cache is also 50ms. This reduces the bot's own reaction delay; the public REST round-trip
  remains network-bound and is typically tens to hundreds of milliseconds. This is deliberately aggressive:
  the tape client backs off for 30s after an error, and failed book requests retry with a 250ms backoff.
  Set `poll_ms = 0` to fall back to `poll_secs` (2s) if the API rate-limits.
- **Dashboard refresh 10s -> 1s.** The UI is now near-live without re-querying SQLite 20 times per second.

## Observability

- **Per-signal execution status** in the new `signal_exec` table (one row per signal and account):
  `SKIPPED_DEPTH`, `SKIPPED_MIN_SIZE`, `SKIPPED_RISK`, `SKIPPED_RETRY`, `SKIPPED_HALT`, `ORDERED`,
  `PARTIAL`, `FILLED`, `UNFILLED`, `FAILED`, `HALTED`. Shown in the Signals tab.
- **Orders/fills since the current run** on the dashboard. The all-time counts previously hid a run that
  placed nothing.
- **`HARVESTER_SUMMARY`** event every five minutes: `today_events`, `before_start`, `metar_fail`,
  `no_extreme`, `alive`, `dead`, `no_book`, `ask_filtered`, `unsafe`, `dispatched`.
- **Walk-forward, city-day-clustered Brier** on the Calibration tab, alongside the in-sample score (which
  is labelled optimistic). Clusters are `(city, event_date)`, so one city-day cannot dominate.

## Data / configuration

- Dallas Love Field PIL corrected from `CLIDFW` to `CLIDAL`.
- Windows development builds compile: the Unix-only signal handler and 0600 permissions are gated.

## Upgrading

- Use the shipped `config/risk.toml`, or add the new `[execution]` keys. Older configs still load (the
  new keys default). `reprice_secs` is clamped to 60–120 and `max_quote_age_secs` to 300–600 in code, so
  an old `10` / `15` config cannot reintroduce the churn; set them to `90` and `600` for clarity.
- The `signal_exec` table is created automatically at startup; existing data is untouched.
- Paper mode needs outbound access to `https://data-api.polymarket.com` for fills.
- Live trading is still off unless explicitly enabled; validate paper fills first.
