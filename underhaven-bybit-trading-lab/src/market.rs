//! In-memory market cache and the dynamic instrument scanner.
//!
//! The cache is deliberately dumb storage: WebSocket tasks write, the scanner
//! and paper engine read. Order-book integrity is validated before data enters
//! the cache, and every scan rejects stale or crossed books instead of silently
//! using them.

use crate::bybit::book_is_tradeable;
use crate::indicators;
use crate::model::{Candle, FundingRate, Instrument, Interval, OrderBook, ScanRow, Side, Ticker};
use std::collections::HashMap;

pub const CANDLE_CAP: usize = 1_500;

#[derive(Debug, Default)]
pub struct MarketCache {
    pub tickers: HashMap<String, Ticker>,
    pub books: HashMap<String, OrderBook>,
    pub candles: HashMap<(String, Interval), Vec<Candle>>,
    pub instruments: HashMap<String, Instrument>,
    pub funding: HashMap<String, Vec<FundingRate>>,
    pub last_tick_ms: i64,
}

impl MarketCache {
    pub fn upsert_instruments(&mut self, instruments: Vec<Instrument>) {
        for i in instruments {
            self.instruments.insert(i.symbol.clone(), i);
        }
    }

    pub fn upsert_tickers(&mut self, tickers: Vec<Ticker>) {
        for t in tickers {
            self.last_tick_ms = self.last_tick_ms.max(t.ts_ms);
            self.tickers.insert(t.symbol.clone(), t);
        }
    }

    pub fn upsert_candle(&mut self, candle: Candle) {
        let key = (candle.symbol.clone(), candle.interval);
        let entry = self.candles.entry(key).or_default();
        if let Some(existing) = entry
            .iter_mut()
            .find(|c| c.open_time_ms == candle.open_time_ms)
        {
            *existing = candle;
        } else {
            entry.push(candle);
            entry.sort_by_key(|c| c.open_time_ms);
            if entry.len() > CANDLE_CAP {
                let overflow = entry.len() - CANDLE_CAP;
                entry.drain(0..overflow);
            }
        }
    }

    pub fn upsert_candles(&mut self, candles: Vec<Candle>) {
        for c in candles {
            self.upsert_candle(c);
        }
    }

    pub fn set_funding(&mut self, symbol: &str, rates: Vec<FundingRate>) {
        self.funding.insert(symbol.to_string(), rates);
    }

    /// Most recent confirmed candles, ascending. The strategy layer never sees an
    /// unconfirmed candle as its last element.
    pub fn candles(&self, symbol: &str, interval: Interval, limit: usize) -> Vec<Candle> {
        let Some(list) = self.candles.get(&(symbol.to_string(), interval)) else {
            return Vec::new();
        };
        let mut confirmed: Vec<Candle> = list.iter().filter(|c| c.confirm).cloned().collect();
        if confirmed.len() > limit {
            confirmed.drain(0..confirmed.len() - limit);
        }
        confirmed
    }

    /// Candle sets keyed by interval, as expected by `strategy::evaluate_all`.
    pub fn candles_by_interval(
        &self,
        symbol: &str,
        intervals: &[Interval],
        limit: usize,
    ) -> HashMap<Interval, Vec<Candle>> {
        let mut map = HashMap::new();
        for interval in intervals {
            let list = self.candles(symbol, *interval, limit);
            if !list.is_empty() {
                map.insert(*interval, list);
            }
        }
        map
    }

    pub fn ticker(&self, symbol: &str) -> Option<&Ticker> {
        self.tickers.get(symbol)
    }

    pub fn book(&self, symbol: &str) -> Option<&OrderBook> {
        self.books.get(symbol)
    }

    pub fn instrument(&self, symbol: &str) -> Option<&Instrument> {
        self.instruments.get(symbol)
    }

    pub fn latest_funding_rate(&self, symbol: &str) -> f64 {
        self.funding
            .get(symbol)
            .and_then(|list| list.last())
            .map(|f| f.rate)
            .or_else(|| self.tickers.get(symbol).map(|t| t.funding_rate))
            .unwrap_or(0.0)
    }

    pub fn eligible_symbols(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .instruments
            .values()
            .filter(|i| i.is_eligible())
            .map(|i| i.symbol.clone())
            .collect();
        out.sort();
        out
    }
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub max_symbols: usize,
    pub max_spread_bps: f64,
    pub min_depth_usd: f64,
    pub min_turnover_24h: f64,
    pub depth_band_bps: f64,
    pub max_funding_pct: f64,
    pub max_staleness_ms: i64,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            max_symbols: 12,
            max_spread_bps: 8.0,
            min_depth_usd: 25_000.0,
            min_turnover_24h: 5_000_000.0,
            depth_band_bps: 10.0,
            max_funding_pct: 0.05,
            max_staleness_ms: 5_000,
        }
    }
}

fn volatility_pct(cache: &MarketCache, symbol: &str, interval: Interval) -> f64 {
    let candles = cache.candles(symbol, interval, 120);
    if candles.len() >= 20 {
        let closes = indicators::closes(&candles);
        let mut returns = Vec::with_capacity(closes.len().saturating_sub(1));
        for pair in closes.windows(2) {
            if pair[0] > 0.0 {
                returns.push((pair[1] - pair[0]) / pair[0]);
            }
        }
        let n = returns.len().min(60);
        if n >= 10 {
            let slice = &returns[returns.len() - n..];
            let mean = slice.iter().sum::<f64>() / slice.len() as f64;
            let var =
                slice.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (slice.len() as f64 - 1.0);
            return var.sqrt() * 100.0;
        }
    }
    cache
        .ticker(symbol)
        .map(|t| (t.price_24h_pct * 100.0).abs())
        .unwrap_or(0.0)
}

/// Rank the eligible universe. The score is intentionally simple and auditable:
/// liquidity dominates, spread is penalised, and very low or very high volatility
/// is not rewarded. It is a *ranking* heuristic, not an expected-return model.
pub fn scan(
    cache: &MarketCache,
    cfg: &ScannerConfig,
    interval: Interval,
    now_ms: i64,
) -> Vec<ScanRow> {
    let mut rows = Vec::new();
    for instrument in cache.instruments.values() {
        let symbol = &instrument.symbol;
        let Some(ticker) = cache.tickers.get(symbol) else {
            continue;
        };
        let mut reasons: Vec<String> = Vec::new();
        if !instrument.is_eligible() {
            reasons.push("not a trading USDT linear perpetual".into());
        }
        if let Err(e) = instrument.validate() {
            reasons.push(format!("instrument metadata: {e}"));
        }
        if !ticker.is_valid() {
            reasons.push("invalid or missing bid/ask".into());
        }
        if now_ms.saturating_sub(ticker.ts_ms) > 60_000 {
            reasons.push(format!(
                "ticker stale by {} ms",
                now_ms.saturating_sub(ticker.ts_ms)
            ));
        }
        let turnover = ticker.turnover_24h;
        if turnover < cfg.min_turnover_24h {
            reasons.push(format!(
                "24h turnover {:.0} below {:.0}",
                turnover, cfg.min_turnover_24h
            ));
        }
        let funding = cache.latest_funding_rate(symbol);
        if (funding * 100.0).abs() > cfg.max_funding_pct {
            reasons.push(format!(
                "funding {:.4}% above {:.4}% cap",
                funding * 100.0,
                cfg.max_funding_pct
            ));
        }

        let (spread_bps, depth_bid, depth_ask) = match cache.books.get(symbol) {
            Some(book) if book_is_tradeable(book, now_ms, cfg.max_staleness_ms) => {
                let spread = book.spread_bps();
                let bid = book.depth_notional(Side::Sell, cfg.depth_band_bps);
                let ask = book.depth_notional(Side::Buy, cfg.depth_band_bps);
                if spread > cfg.max_spread_bps {
                    reasons.push(format!(
                        "spread {spread:.2} bps above {:.2} bps",
                        cfg.max_spread_bps
                    ));
                }
                if bid < cfg.min_depth_usd || ask < cfg.min_depth_usd {
                    reasons.push(format!(
                        "depth bid {bid:.0} / ask {ask:.0} below {:.0}",
                        cfg.min_depth_usd
                    ));
                }
                (spread, bid, ask)
            }
            Some(_) => {
                reasons.push("order book stale, crossed or invalid".into());
                (f64::INFINITY, 0.0, 0.0)
            }
            None => {
                reasons.push("no order book snapshot".into());
                (f64::INFINITY, 0.0, 0.0)
            }
        };

        let volatility = volatility_pct(cache, symbol, interval);
        let spread_component = if spread_bps.is_finite() {
            (cfg.max_spread_bps - spread_bps).max(0.0)
        } else {
            0.0
        };
        let depth_component = ((depth_bid.min(depth_ask)) / cfg.min_depth_usd.max(1.0)).min(20.0);
        let liquidity_score = (turnover / 1_000_000.0).ln_1p() * 8.0
            + depth_component
            + spread_component * 0.5
            + (volatility * 2.0).min(6.0);

        rows.push(ScanRow {
            symbol: symbol.clone(),
            last_price: ticker.last_price,
            spread_bps,
            depth_bid_usd: depth_bid,
            depth_ask_usd: depth_ask,
            turnover_24h: turnover,
            volatility_pct: volatility,
            liquidity_score,
            eligible: reasons.is_empty(),
            reject_reason: if reasons.is_empty() {
                None
            } else {
                Some(reasons.join("; "))
            },
            selected: false,
            updated_at_ms: now_ms,
        });
    }
    rows.sort_by(|a, b| {
        b.eligible
            .cmp(&a.eligible)
            .then_with(|| b.liquidity_score.total_cmp(&a.liquidity_score))
    });
    for (idx, row) in rows.iter_mut().enumerate() {
        row.selected = row.eligible && idx < cfg.max_symbols;
    }
    rows
}

/// Ensure the configured seed symbols survive scanner churn, as long as their
/// metadata is available. This keeps the UI and backtests anchored to familiar
/// majors while the scanner still drives the broader universe.
pub fn merge_seed_symbols(
    selected: &[String],
    seeds: &[String],
    cache: &MarketCache,
    max_symbols: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in seeds {
        if cache.instruments.contains_key(s) && !out.contains(s) && out.len() < max_symbols {
            out.push(s.clone());
        }
    }
    for s in selected {
        if out.len() >= max_symbols {
            break;
        }
        if !out.contains(s) {
            out.push(s.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookLevel, Instrument};

    fn instrument(symbol: &str) -> Instrument {
        Instrument {
            symbol: symbol.into(),
            base_coin: symbol.trim_end_matches("USDT").into(),
            quote_coin: "USDT".into(),
            settle_coin: "USDT".into(),
            contract_type: "LinearPerpetual".into(),
            status: "Trading".into(),
            tick_size: 0.01,
            qty_step: 0.001,
            min_qty: 0.001,
            max_qty: 1000.0,
            max_market_qty: 1000.0,
            min_notional: 5.0,
            funding_interval_minutes: 480,
            launch_time_ms: 0,
            source: "test".into(),
            fetched_at_ms: 0,
        }
    }

    fn cache_with(spread: f64, depth: f64, crossed: bool) -> MarketCache {
        let mut cache = MarketCache::default();
        cache.upsert_instruments(vec![instrument("BTCUSDT")]);
        cache.upsert_tickers(vec![Ticker {
            symbol: "BTCUSDT".into(),
            last_price: 50_000.0,
            bid_price: 50_000.0 - spread / 2.0,
            ask_price: 50_000.0 + spread / 2.0,
            turnover_24h: 1_000_000_000.0,
            ts_ms: 1_000_000,
            source: "test".into(),
            ..Ticker::default()
        }]);
        cache.books.insert(
            "BTCUSDT".into(),
            OrderBook {
                symbol: "BTCUSDT".into(),
                ts_ms: 1_000_000,
                received_ms: 1_000_000,
                update_id: 1,
                seq: 1,
                bids: vec![BookLevel {
                    price: if crossed {
                        50_001.0
                    } else {
                        50_000.0 - spread / 2.0
                    },
                    size: depth,
                }],
                asks: vec![BookLevel {
                    price: 50_000.0 + spread / 2.0,
                    size: depth,
                }],
                source: "test".into(),
                valid: true,
                gap_count: 0,
            },
        );
        cache
    }

    #[test]
    fn rejects_crossed_book() {
        let cache = cache_with(1.0, 10_000.0, true);
        let rows = scan(&cache, &ScannerConfig::default(), Interval::M5, 1_000_000);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].eligible);
        assert!(rows[0]
            .reject_reason
            .as_deref()
            .unwrap()
            .contains("crossed"));
    }

    #[test]
    fn rejects_wide_spread_and_thin_depth() {
        let cache = cache_with(100.0, 0.001, false);
        let rows = scan(&cache, &ScannerConfig::default(), Interval::M5, 1_000_000);
        assert!(!rows[0].eligible);
        let reason = rows[0].reject_reason.as_deref().unwrap();
        assert!(reason.contains("spread"));
        assert!(reason.contains("depth"));
    }

    #[test]
    fn rejects_stale_book() {
        let cache = cache_with(1.0, 10_000.0, false);
        let rows = scan(&cache, &ScannerConfig::default(), Interval::M5, 1_100_000);
        assert!(!rows[0].eligible);
        assert!(rows[0].reject_reason.as_deref().unwrap().contains("stale"));
    }

    #[test]
    fn ranks_eligible_symbols_and_respects_cap() {
        let mut cache = cache_with(1.0, 10_000.0, false);
        for (idx, sym) in ["ETHUSDT", "SOLUSDT", "XRPUSDT"].iter().enumerate() {
            cache.upsert_instruments(vec![instrument(sym)]);
            cache.upsert_tickers(vec![Ticker {
                symbol: (*sym).into(),
                last_price: 100.0,
                bid_price: 99.99,
                ask_price: 100.01,
                turnover_24h: 50_000_000.0 / (idx + 1) as f64,
                ts_ms: 1_000_000,
                source: "test".into(),
                ..Ticker::default()
            }]);
            cache.books.insert(
                (*sym).into(),
                OrderBook {
                    symbol: (*sym).into(),
                    ts_ms: 1_000_000,
                    received_ms: 1_000_000,
                    bids: vec![BookLevel {
                        price: 99.99,
                        size: 100_000.0,
                    }],
                    asks: vec![BookLevel {
                        price: 100.01,
                        size: 100_000.0,
                    }],
                    source: "test".into(),
                    valid: true,
                    ..OrderBook::default()
                },
            );
        }
        let cfg = ScannerConfig {
            max_symbols: 2,
            ..ScannerConfig::default()
        };
        let rows = scan(&cache, &cfg, Interval::M5, 1_000_000);
        assert_eq!(rows.iter().filter(|r| r.selected).count(), 2);
        assert!(rows[0].selected);
    }

    #[test]
    fn seeds_survive_rank_churn() {
        let cache = cache_with(1.0, 10_000.0, false);
        let out = merge_seed_symbols(&[], &["BTCUSDT".into()], &cache, 3);
        assert_eq!(out, vec!["BTCUSDT".to_string()]);
    }
}
