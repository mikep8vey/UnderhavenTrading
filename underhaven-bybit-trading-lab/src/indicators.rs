//! Deterministic indicator implementations. All functions operate on closed
//! candles only; callers must not pass an unconfirmed candle.

use crate::model::Candle;

pub fn sma(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    let sum: f64 = values[values.len() - period..].iter().sum();
    Some(sum / period as f64)
}

/// Full EMA series aligned to the input values (warm-up entries are `None`).
pub fn ema_series(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if period == 0 || values.len() < period {
        return out;
    }
    let k = 2.0 / (period as f64 + 1.0);
    let seed: f64 = values[..period].iter().sum::<f64>() / period as f64;
    out[period - 1] = Some(seed);
    let mut prev = seed;
    for i in period..values.len() {
        prev = values[i] * k + prev * (1.0 - k);
        out[i] = Some(prev);
    }
    out
}

pub fn ema(values: &[f64], period: usize) -> Option<f64> {
    ema_series(values, period).last().copied().flatten()
}

/// Wilder's ATR. Requires `period + 1` candles (the first true range is skipped).
pub fn atr(candles: &[Candle], period: usize) -> Option<f64> {
    if period == 0 || candles.len() < period + 1 {
        return None;
    }
    let mut trs = Vec::with_capacity(candles.len() - 1);
    for w in candles.windows(2) {
        let prev_close = w[0].close;
        let c = &w[1];
        let tr = (c.high - c.low)
            .max((c.high - prev_close).abs())
            .max((c.low - prev_close).abs());
        trs.push(tr);
    }
    if trs.len() < period {
        return None;
    }
    let mut value: f64 = trs[..period].iter().sum::<f64>() / period as f64;
    for tr in &trs[period..] {
        value = (value * (period as f64 - 1.0) + tr) / period as f64;
    }
    Some(value)
}

/// Simple (population) standard deviation of the last `period` values.
pub fn stddev(values: &[f64], period: usize) -> Option<f64> {
    let mean = sma(values, period)?;
    let var = values[values.len() - period..]
        .iter()
        .map(|v| (v - mean).powi(2))
        .sum::<f64>()
        / period as f64;
    Some(var.sqrt())
}

#[derive(Debug, Clone, Copy)]
pub struct Bollinger {
    pub mean: f64,
    pub upper: f64,
    pub lower: f64,
    pub stddev: f64,
    pub width_pct: f64,
}

pub fn bollinger(values: &[f64], period: usize, k: f64) -> Option<Bollinger> {
    let mean = sma(values, period)?;
    let sd = stddev(values, period)?;
    let upper = mean + k * sd;
    let lower = mean - k * sd;
    let width_pct = if mean != 0.0 {
        ((upper - lower) / mean) * 100.0
    } else {
        0.0
    };
    Some(Bollinger {
        mean,
        upper,
        lower,
        stddev: sd,
        width_pct,
    })
}

pub fn zscore(values: &[f64], period: usize, value: f64) -> Option<f64> {
    let mean = sma(values, period)?;
    let sd = stddev(values, period)?;
    if sd <= f64::EPSILON {
        return None;
    }
    Some((value - mean) / sd)
}

/// Rate of change over `period` bars, in percent.
pub fn roc(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() <= period {
        return None;
    }
    let old = values[values.len() - 1 - period];
    let new = values[values.len() - 1];
    if old == 0.0 {
        return None;
    }
    Some(((new - old) / old) * 100.0)
}

pub fn highest(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    values[values.len() - period..]
        .iter()
        .copied()
        .reduce(f64::max)
}

pub fn lowest(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    values[values.len() - period..]
        .iter()
        .copied()
        .reduce(f64::min)
}

pub fn closes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.close).collect()
}

/// Annualized realized volatility from log returns (365 days, no risk-free rate).
/// This is descriptive only; it is not a forecast.
pub fn realized_volatility_pct(candles: &[Candle], bars_per_year: f64) -> Option<f64> {
    if candles.len() < 10 {
        return None;
    }
    let mut rets = Vec::with_capacity(candles.len() - 1);
    for w in candles.windows(2) {
        if w[0].close > 0.0 && w[1].close > 0.0 {
            rets.push((w[1].close / w[0].close).ln());
        }
    }
    if rets.len() < 5 {
        return None;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() as f64 - 1.0);
    Some(var.sqrt() * bars_per_year.sqrt() * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Interval;

    fn candle(i: i64, o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            symbol: "TESTUSDT".into(),
            interval: Interval::M5,
            open_time_ms: i * 300_000,
            open: o,
            high: h,
            low: l,
            close: c,
            volume: 1.0,
            turnover: c,
            confirm: true,
            source: "test".into(),
            ingested_at_ms: 0,
        }
    }

    #[test]
    fn ema_known_values() {
        let vals = [1.0, 2.0, 3.0, 4.0, 5.0];
        let e = ema_series(&vals, 3);
        assert!((e[2].unwrap() - 2.0).abs() < 1e-12);
        assert!((e[3].unwrap() - 3.0).abs() < 1e-12);
        assert!((e[4].unwrap() - 4.0).abs() < 1e-12);
        assert!(e[0].is_none());
    }

    #[test]
    fn ema_reacts_more_than_sma() {
        let vals = [10.0, 10.0, 10.0, 10.0, 20.0];
        assert!(ema(&vals, 3).unwrap() > sma(&vals, 3).unwrap());
    }

    #[test]
    fn atr_wilder_matches_manual() {
        // TRs: 2, 2, 2, 2, 2 -> ATR 2 regardless of warm-up
        let mut cs = Vec::new();
        for i in 0..10 {
            cs.push(candle(i, 10.0, 11.0, 9.0, 10.0));
        }
        let a = atr(&cs, 3).unwrap();
        assert!((a - 2.0).abs() < 1e-9, "atr={a}");
    }

    #[test]
    fn bollinger_bands_symmetric() {
        let vals = [1.0, 2.0, 3.0, 4.0, 5.0];
        let b = bollinger(&vals, 5, 2.0).unwrap();
        assert!((b.mean - 3.0).abs() < 1e-12);
        assert!((b.upper - b.mean - (b.mean - b.lower)).abs() < 1e-12);
        assert!(b.stddev > 0.0);
    }

    #[test]
    fn zscore_extremes() {
        let vals = [1.0, 1.0, 1.0, 1.0, 5.0];
        assert!(zscore(&vals, 5, 5.0).unwrap() > 1.5);
    }

    #[test]
    fn insufficient_data_is_none() {
        assert_eq!(sma(&[1.0], 5), None);
        assert_eq!(ema(&[1.0], 5), None);
        assert_eq!(atr(&[], 14), None);
    }

    #[test]
    fn roc_calculation() {
        assert!((roc(&[100.0, 110.0], 1).unwrap() - 10.0).abs() < 1e-9);
    }
}
