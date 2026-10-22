# Bybit integration

UNDERHAVEN uses Bybit's **V5 public market-data API** and the **public linear
WebSocket stream**. It does not call any order-entry, cancellation, position or
withdrawal endpoint. The optional API key path uses two signed **read-only**
metadata calls only.

This document records what was reviewed, what the adapter implements, and which
assumptions remain.

## 1. SDK review

Before writing the adapter, the official Rust SDK `bybit-api` **0.1.2** (MIT,
crates.io) was inspected. It was not adopted for the runtime because:

| Gap | Consequence for this project |
| --- | --- |
| `get_klines` exposes no `start`/`end` pagination | Cannot import a reproducible 90-day window. |
| `get_funding_history` exposes no time-range pagination | Funding costs for backtests would be incomplete. |
| `get_instruments_info` has no `status`/`limit`/cursor arguments and loose field models | Cannot reliably filter `Trading` USDT linear perpetuals. |
| WebSocket helper has no reconnect, resubscribe or heartbeat watchdog | An always-on VPS process would silently stop receiving data. |
| No rate-limit-aware retry/backoff policy | Bursts can trip Bybit limits and drop the feed. |
| Uses `native-tls`, while this project standardises on `rustls` | Extra system dependency and a second TLS stack. |

The platform therefore uses a **small typed adapter** in `src/bybit.rs`
(plus `src/ws.rs`), implementing only the endpoints and behaviours it needs.
The adapter is not a general-purpose SDK and is intentionally kept auditable.

The dependency set is pinned in `Cargo.toml`; `Cargo.lock` is committed.

## 2. Endpoints used

All requests use `category=linear` and the base URL from
`UNDERHAVEN_REST_BASE` (default `https://api.bybit.com`).

| Purpose | Endpoint | Auth | Notes |
| --- | --- | --- | --- |
| Server time / clock offset | `GET /v5/market/time` | public | Warns above 2 s drift. |
| Instrument catalogue | `GET /v5/market/instruments-info` | public | Filters `LinearPerpetual`, `USDT` quote/settle, `Trading`. |
| Tickers | `GET /v5/market/tickers` | public | Turnover, funding, open interest. |
| Candles | `GET /v5/market/kline` | public | 1m/5m/15m/1h, 1,000 per page, ascending after normalisation. |
| Order book | `GET /v5/market/orderbook` | public | Snapshot used for depth/spread and after WS gaps. |
| Funding history | `GET /v5/market/funding/history` | public | Paged by time for backtests and forward settlement. |
| Fee rate | `GET /v5/account/fee-rate` | signed, read-only | Replaces conservative fee assumptions only after key verification. |
| Key introspection | `GET /v5/user/query-api` | signed, read-only | Requires `readOnly=1`; otherwise the key is discarded. |
| Wallet balance (optional) | `GET /v5/account/wallet-balance` | signed, read-only | Read-only display helper; never used for sizing or execution. |

**Not present anywhere in the repository:** `/v5/order/*`, `/v5/execution/*`,
`/v5/position/*`, `/v5/account/withdraw`, `/v5/asset/withdraw`. The repository
test `tests/paper_only.rs` fails if any of these strings appear in `src/`.

## 3. WebSocket behaviour

URL: `wss://stream.bybit.com/v5/public/linear` (override with
`UNDERHAVEN_WS_URL`). Only public topics are subscribed:

* `tickers.{symbol}`
* `orderbook.{depth}.{symbol}` (default depth 50)
* `kline.{interval}.{symbol}`

There is no `auth` frame and no private `order`, `execution`, `position` or
`wallet` topic. `tests/paper_only.rs` asserts this behaviourally through
`ws::subscription_args`.

Supervision in `src/ws.rs`:

1. Subscribe in batches of at most 10 topics.
2. Send an application-level `{"op":"ping"}` every 20 s.
3. Reconnect if no frame arrives for 40 s (watchdog).
4. Exponential backoff with jitter, capped at 30 s.
5. Validate order-book update sequence; on a gap, request a fresh REST snapshot
   and merge it without losing the ticker/kline stream.
6. Reconnect cleanly when the selected symbol set changes; subscriptions are
   rebuilt from the scanner output.

If the stream is unhealthy, the dashboard shows the reason and the paper engine
refuses new entries. Existing stop/target exits still use the last cached quote
only when it passes the staleness gate; otherwise they remain open and the
condition is reported.

## 4. Signing and the optional read-only key

Signed requests use Bybit V5 HMAC-SHA256:

```
payload   = timestamp + api_key + recv_window + query_string
signature = hex(HMAC_SHA256(api_secret, payload))
headers   = X-BAPI-API-KEY, X-BAPI-TIMESTAMP, X-BAPI-SIGN, X-BAPI-RECV-WINDOW
```

`recv_window` is 5,000 ms. The server time offset is measured at startup and
surfaced in the system panel; a large offset is logged.

Key handling:

1. The operator submits key/secret over the authenticated HTTPS dashboard.
2. The server calls `/v5/user/query-api` with the submitted key.
3. If Bybit does not report `readOnly=1`, the request is rejected and **nothing
   is stored**.
4. On success, the key and secret are encrypted with AES-256-GCM using a master
   key file outside SQLite (`UNDERHAVEN_MASTER_KEY_FILE`, default
   `/etc/underhaven/master.key` on production installs).
5. The plaintext secret is never returned to the browser and is never written to
   logs. The dashboard only shows verification status, UID and note.
6. Deleting the key removes the encrypted row and updates the status panel.

The paper engine has no access to these credentials. It never constructs a
signed request and never imports `PublicClient`.

## 5. Retry, pacing and error handling

* Client-side pacing: at least 100 ms between requests, well inside Bybit's
  global envelope.
* Up to 4 retries with exponential backoff for transport errors, HTTP 429,
  HTTP 5xx and Bybit `retCode 10006` (rate limited).
* HTTP 4xx other than 429 is not retried.
* `retCode != 0` is surfaced as a typed `BybitError::Api` with code and message.
* Parse failures are explicit (`BybitError::Parse`), not silently defaulted.
* API request/error/rate-limit counters are exposed in the dashboard health
  panel (`api_requests`, `api_errors`, `rate_limit_hits`).

## 6. Data-quality gates

The scanner rejects a symbol instead of ranking dubious data:

* stale ticker or order book (`UNDERHAVEN_MAX_STALENESS_MS`, default 5,000 ms);
* crossed or one-sided book;
* spread above `UNDERHAVEN_MAX_SPREAD_BPS` (default 8 bps);
* depth within 10 bps below `UNDERHAVEN_MIN_DEPTH_USD` (default 25,000 USD per
  side);
* 24 h turnover below 5,000,000 USD;
* non-`Trading` instrument, non-linear contract, non-USDT settlement;
* funding outside the configured sanity band (default 0.05%);
* insufficient candle history for the strategy timeframe.

The rejection reason is displayed in section D of the dashboard and persisted
with the scan snapshot.

## 7. Assumptions and limitations

* **Fees are not quoted by Bybit for a public/unauthenticated account.** The
  defaults (7.5 bps taker, 2.0 bps maker, 4 bps slippage) are deliberately
  conservative *assumptions*, labelled as such. A verified read-only key
  replaces the taker/maker numbers with the account's fee rate and the dashboard
  reports `bybit_account_fee_rate`.
* **Historical order-book depth is unavailable.** Backtests take an explicit
  spread assumption (`spread_bps_assumption`); forward paper trading uses the
  live book with conservative fills.
* **Funding** is read from public funding history. Missing history produces a
  warning and an optimistic-result caveat rather than an invented rate.
* Bybit can change response fields or limits. The adapter's typed parsing and
  explicit errors are intended to fail visibly, not to guess.
* Testnet is supported by pointing `UNDERHAVEN_REST_BASE` and
  `UNDERHAVEN_WS_URL` at Bybit testnet endpoints; the same read-only checks
  apply. Testnet liquidity and funding are not production evidence.

## 8. Verification commands

```bash
# Public connectivity and configuration
cargo run -- doctor

# Import and inspect a symbol
cargo run -- import-history BTCUSDT --interval 5m --days 30

# Backtest with cost/funding assumptions
cargo run -- backtest BTCUSDT --strategy trend --interval 5m --days 30
```

No Bybit credentials are needed for any of the above. `doctor` makes public
requests only.
