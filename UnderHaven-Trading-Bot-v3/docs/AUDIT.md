# Repository audit (before v4.0) and what changed

## Strategy inventory
| Strategy | Status before | Now |
|---|---|---|
| Same-platform bundle (Polymarket UP+DOWN asks < $1, BTC 15m) | implemented, never found a real gap | retained as **G**, on the shared engine, 5m + 15m |
| Cross-platform arbitrage | missing | **A**, strict matching, depth-aware |
| Momentum, mean reversion, order-book imbalance, expiration-aware entry | missing | **B, C, D, E** (hypotheses, forward-tested in paper) |
| Strategy coordination | missing | **F** |

## Defects found and fixed (cumulative)
- Fill detection accepted `{"success": false}` as filled; no unwind after a one-sided fill; loss stop could not see losses (now booked as worst-case losses).
- Per-request client creation leaked file descriptors ("Too many open files"); order books were read one after another (phantom gaps); "no order book yet" counted as an error.
- Polymarket `orderMinSize` treated as shares (it is USDC); V2 markets use `positionIds`; fee parameters ignored (now `feeSchedule`).
- Duplicate paper trades on the same window; dashboard chart serialisation error; screen overflow; auto-refresh wiped forms; no CSRF; login not throttled.
- Kalshi: no client existed; now signed requests, backoff, uncertain-write reconciliation.
- Obsolete multi-coin and BTC-only dashboards and scanner code removed (backups of earlier zips are unaffected).
