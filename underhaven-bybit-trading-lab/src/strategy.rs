//! The three independent strategy engines.
//!
//! Every evaluator receives only *completed* candles in ascending time order.
//! The final element is the most recently completed candle; the signal is known
//! only after that candle closes. Execution is modeled by the caller at the next
//! available market price (or the next bar open in a backtest).

use crate::config::{BreakoutConfig, MeanReversionConfig, StrategyConfigs, TrendConfig};
use crate::indicators;
use crate::model::{Candle, Direction, Interval, StrategyId};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalDraft {
    pub strategy: StrategyId,
    pub symbol: String,
    pub timeframe: Interval,
    pub direction: Direction,
    pub candle_time_ms: i64,
    pub signal_time_ms: i64,
    pub entry_reference: f64,
    pub stop: f64,
    pub target: f64,
    pub atr: f64,
    pub trailing_distance: f64,
    pub max_hold_bars: i64,
    pub reason: String,
    pub features: serde_json::Value,
}

impl SignalDraft {
    pub fn reward_risk(&self) -> f64 {
        let risk = (self.entry_reference - self.stop).abs();
        let reward = (self.target - self.entry_reference).abs();
        if risk <= 0.0 {
            0.0
        } else {
            reward / risk
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyEval {
    pub strategy: StrategyId,
    pub symbol: String,
    pub timeframe: Interval,
    pub candidate: Option<SignalDraft>,
    pub note: String,
    pub features: serde_json::Value,
}

pub fn evaluate_all(
    symbol: &str,
    candles_by_interval: &std::collections::HashMap<Interval, Vec<Candle>>,
    now_ms: i64,
    cfg: &StrategyConfigs,
) -> Vec<StrategyEval> {
    let mut out = Vec::new();
    for id in StrategyId::ALL {
        if !cfg.enabled(id) {
            continue;
        }
        let interval = cfg.timeframe(id);
        let Some(candles) = candles_by_interval.get(&interval) else {
            out.push(StrategyEval {
                strategy: id,
                symbol: symbol.to_string(),
                timeframe: interval,
                candidate: None,
                note: "no candles loaded for timeframe".into(),
                features: json!({}),
            });
            continue;
        };
        let eval = match id {
            StrategyId::Trend => evaluate_trend(symbol, candles, now_ms, &cfg.trend),
            StrategyId::MeanReversion => {
                evaluate_mean_reversion(symbol, candles, now_ms, &cfg.mean_reversion)
            }
            StrategyId::Breakout => evaluate_breakout(symbol, candles, now_ms, &cfg.breakout),
        };
        out.push(eval);
    }
    out
}

fn base_features(candles: &[Candle]) -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    if let Some(last) = candles.last() {
        m.insert("last_candle_time_ms".into(), json!(last.open_time_ms));
        m.insert("last_open".into(), json!(last.open));
        m.insert("last_high".into(), json!(last.high));
        m.insert("last_low".into(), json!(last.low));
        m.insert("last_close".into(), json!(last.close));
        m.insert("last_volume".into(), json!(last.volume));
        m.insert("last_source".into(), json!(last.source));
    }
    m.insert("closed_candles".into(), json!(candles.len()));
    m
}

pub fn evaluate_trend(
    symbol: &str,
    candles: &[Candle],
    now_ms: i64,
    cfg: &TrendConfig,
) -> StrategyEval {
    let mut features = base_features(candles);
    let timeframe = cfg.timeframe;
    let fail = |note: String, features: serde_json::Map<String, serde_json::Value>| StrategyEval {
        strategy: StrategyId::Trend,
        symbol: symbol.to_string(),
        timeframe,
        candidate: None,
        note,
        features: serde_json::Value::Object(features),
    };

    let needed = cfg
        .ema_slow
        .max(cfg.trend_reference)
        .max(cfg.momentum_period + 1)
        .max(cfg.atr_period + 1)
        + 2;
    if candles.len() < needed {
        return fail(
            format!(
                "insufficient history ({}/{needed} closed candles)",
                candles.len()
            ),
            features,
        );
    }
    if candles.last().map(|c| !c.confirm).unwrap_or(true) {
        return fail("last candle is not confirmed".into(), features);
    }

    let cls = indicators::closes(candles);
    let fast = indicators::ema(&cls, cfg.ema_fast);
    let slow = indicators::ema(&cls, cfg.ema_slow);
    let reference = indicators::ema(&cls, cfg.trend_reference);
    let momentum = indicators::roc(&cls, cfg.momentum_period);
    let atr = indicators::atr(candles, cfg.atr_period);
    let (Some(fast), Some(slow), Some(reference), Some(momentum), Some(atr)) =
        (fast, slow, reference, momentum, atr)
    else {
        return fail("indicator warm-up incomplete".into(), features);
    };
    let last = candles.last().expect("checked non-empty");
    let close = last.close;
    let atr_pct = atr / close * 100.0;
    let fast_prev = indicators::ema(&cls[..cls.len() - 1], cfg.ema_fast).unwrap_or(fast);
    let slow_prev = indicators::ema(&cls[..cls.len() - 1], cfg.ema_slow).unwrap_or(slow);

    features.insert("ema_fast".into(), json!(fast));
    features.insert("ema_slow".into(), json!(slow));
    features.insert("ema_reference".into(), json!(reference));
    features.insert("momentum_pct".into(), json!(momentum));
    features.insert("atr".into(), json!(atr));
    features.insert("atr_pct".into(), json!(atr_pct));

    if !(cfg.min_atr_pct..=cfg.max_atr_pct).contains(&atr_pct) {
        return fail(
            format!(
                "volatility filter: ATR {:.3}% outside [{:.3}%, {:.3}%]",
                atr_pct, cfg.min_atr_pct, cfg.max_atr_pct
            ),
            features,
        );
    }

    let bullish = fast > slow && close > reference && momentum >= cfg.momentum_min_pct;
    let bearish = fast < slow && close < reference && momentum <= -cfg.momentum_min_pct;
    let confirmed_bull = bullish && fast_prev > slow_prev;
    let confirmed_bear = bearish && fast_prev < slow_prev;

    if !confirmed_bull && !confirmed_bear {
        return fail(
            format!(
                "no confirmed trend (EMA{} {:.6} vs EMA{} {:.6}, ROC {:.3}%)",
                cfg.ema_fast, fast, cfg.ema_slow, slow, momentum
            ),
            features,
        );
    }

    let direction = if confirmed_bull {
        Direction::Long
    } else {
        Direction::Short
    };
    let stop = if direction == Direction::Long {
        close - atr * cfg.stop_atr_mult
    } else {
        close + atr * cfg.stop_atr_mult
    };
    let target = if direction == Direction::Long {
        close + atr * cfg.target_atr_mult
    } else {
        close - atr * cfg.target_atr_mult
    };
    if stop <= 0.0 || target <= 0.0 {
        return fail("ATR-derived stop/target is non-positive".into(), features);
    }

    let candidate = SignalDraft {
        strategy: StrategyId::Trend,
        symbol: symbol.to_string(),
        timeframe,
        direction,
        candle_time_ms: last.open_time_ms,
        signal_time_ms: now_ms,
        entry_reference: close,
        stop,
        target,
        atr,
        trailing_distance: atr * cfg.trailing_atr_mult,
        max_hold_bars: cfg.max_hold_bars,
        reason: format!(
            "{} trend confirmed: EMA{} {:.6} {} EMA{} {:.6}, close {} EMA{} {:.6}, ROC {:.3}%",
            direction,
            cfg.ema_fast,
            fast,
            if direction == Direction::Long {
                ">"
            } else {
                "<"
            },
            cfg.ema_slow,
            slow,
            if close > reference { ">" } else { "<" },
            cfg.trend_reference,
            reference,
            momentum
        ),
        features: serde_json::Value::Object(features.clone()),
    };
    StrategyEval {
        strategy: StrategyId::Trend,
        symbol: symbol.to_string(),
        timeframe,
        candidate: Some(candidate),
        note: "confirmed trend signal".into(),
        features: serde_json::Value::Object(features),
    }
}

pub fn evaluate_mean_reversion(
    symbol: &str,
    candles: &[Candle],
    now_ms: i64,
    cfg: &MeanReversionConfig,
) -> StrategyEval {
    let mut features = base_features(candles);
    let timeframe = cfg.timeframe;
    let fail = |note: String, features: serde_json::Map<String, serde_json::Value>| StrategyEval {
        strategy: StrategyId::MeanReversion,
        symbol: symbol.to_string(),
        timeframe,
        candidate: None,
        note,
        features: serde_json::Value::Object(features),
    };

    let needed = cfg.sma_period.max(cfg.atr_period + 1).max(200) + 2;
    if candles.len() < needed {
        return fail(
            format!(
                "insufficient history ({}/{} closed candles; needs 200-bar regime filter)",
                candles.len(),
                needed
            ),
            features,
        );
    }
    if candles.last().map(|c| !c.confirm).unwrap_or(true) {
        return fail("last candle is not confirmed".into(), features);
    }

    let cls = indicators::closes(candles);
    let last = candles.last().expect("checked non-empty");
    let close = last.close;
    let prev_close = candles[candles.len() - 2].close;
    let Some(bb) = indicators::bollinger(&cls, cfg.sma_period, cfg.bb_k) else {
        return fail("Bollinger warm-up incomplete".into(), features);
    };
    let Some(z) = indicators::zscore(&cls, cfg.sma_period, close) else {
        return fail("zero-width deviation band".into(), features);
    };
    let Some(atr) = indicators::atr(candles, cfg.atr_period) else {
        return fail("ATR warm-up incomplete".into(), features);
    };
    let ema50 = indicators::ema(&cls, 50);
    let ema200 = indicators::ema(&cls, 200);
    let (Some(ema50), Some(ema200)) = (ema50, ema200) else {
        return fail("regime EMA warm-up incomplete".into(), features);
    };
    let trend_strength = if atr > 0.0 {
        (ema50 - ema200).abs() / atr
    } else {
        f64::INFINITY
    };
    features.insert("sma".into(), json!(bb.mean));
    features.insert("bb_upper".into(), json!(bb.upper));
    features.insert("bb_lower".into(), json!(bb.lower));
    features.insert("bb_width_pct".into(), json!(bb.width_pct));
    features.insert("zscore".into(), json!(z));
    features.insert("atr".into(), json!(atr));
    features.insert("trend_strength_atr".into(), json!(trend_strength));

    if bb.width_pct < cfg.min_band_width_pct {
        return fail(
            format!(
                "volatility too low: band width {:.3}% < {:.3}%",
                bb.width_pct, cfg.min_band_width_pct
            ),
            features,
        );
    }
    if bb.width_pct > cfg.max_band_width_pct {
        return fail(
            format!(
                "volatility too high: band width {:.3}% > {:.3}%",
                bb.width_pct, cfg.max_band_width_pct
            ),
            features,
        );
    }
    if trend_strength > cfg.max_trend_strength {
        return fail(
            format!(
                "trend regime filter: |EMA50-EMA200|/ATR = {:.2} > {:.2}",
                trend_strength, cfg.max_trend_strength
            ),
            features,
        );
    }

    let long_setup = z <= -cfg.z_entry && (!cfg.require_reversal || close > prev_close);
    let short_setup = z >= cfg.z_entry && (!cfg.require_reversal || close < prev_close);
    if !long_setup && !short_setup {
        return fail(
            format!(
                "no extreme with stabilization (z={:.2}, prev_close={:.6}, close={:.6})",
                z, prev_close, close
            ),
            features,
        );
    }

    let direction = if long_setup {
        Direction::Long
    } else {
        Direction::Short
    };
    let stop = if direction == Direction::Long {
        close - atr * cfg.stop_atr_mult
    } else {
        close + atr * cfg.stop_atr_mult
    };
    let target = bb.mean;
    if stop <= 0.0 || (direction == Direction::Long && target <= close) {
        return fail("mean-reversion target is not above entry".into(), features);
    }
    if direction == Direction::Short && target >= close {
        return fail("mean-reversion target is not below entry".into(), features);
    }

    let candidate = SignalDraft {
        strategy: StrategyId::MeanReversion,
        symbol: symbol.to_string(),
        timeframe,
        direction,
        candle_time_ms: last.open_time_ms,
        signal_time_ms: now_ms,
        entry_reference: close,
        stop,
        target,
        atr,
        trailing_distance: 0.0,
        max_hold_bars: cfg.max_hold_bars,
        reason: format!(
            "{} reversion: z={:.2} (entry threshold {:.2}), band width {:.3}%, trend strength {:.2} ATR",
            direction, z, cfg.z_entry, bb.width_pct, trend_strength
        ),
        features: serde_json::Value::Object(features.clone()),
    };
    StrategyEval {
        strategy: StrategyId::MeanReversion,
        symbol: symbol.to_string(),
        timeframe,
        candidate: Some(candidate),
        note: "extreme deviation with stabilization".into(),
        features: serde_json::Value::Object(features),
    }
}

pub fn evaluate_breakout(
    symbol: &str,
    candles: &[Candle],
    now_ms: i64,
    cfg: &BreakoutConfig,
) -> StrategyEval {
    let mut features = base_features(candles);
    let timeframe = cfg.timeframe;
    let fail = |note: String, features: serde_json::Map<String, serde_json::Value>| StrategyEval {
        strategy: StrategyId::Breakout,
        symbol: symbol.to_string(),
        timeframe,
        candidate: None,
        note,
        features: serde_json::Value::Object(features),
    };

    let needed = cfg
        .lookback
        .max(cfg.atr_period + 1)
        .max(cfg.volume_period + 1)
        + 2;
    if candles.len() < needed + 1 {
        return fail(
            format!(
                "insufficient history ({}/{needed} closed candles)",
                candles.len()
            ),
            features,
        );
    }
    if candles.last().map(|c| !c.confirm).unwrap_or(true) {
        return fail("last candle is not confirmed".into(), features);
    }

    let last = candles.last().expect("checked non-empty");
    let close = last.close;
    let prior = &candles[candles.len() - 1 - cfg.lookback..candles.len() - 1];
    let range_high = prior.iter().map(|c| c.high).fold(f64::MIN, f64::max);
    let range_low = prior.iter().map(|c| c.low).fold(f64::MAX, f64::min);
    let Some(atr) = indicators::atr(candles, cfg.atr_period) else {
        return fail("ATR warm-up incomplete".into(), features);
    };
    let volumes: Vec<f64> = candles.iter().map(|c| c.volume).collect();
    let Some(avg_volume) = indicators::sma(&volumes[..volumes.len() - 1], cfg.volume_period) else {
        return fail("volume warm-up incomplete".into(), features);
    };
    let range_pct = (range_high - range_low) / close * 100.0;
    features.insert("range_high".into(), json!(range_high));
    features.insert("range_low".into(), json!(range_low));
    features.insert("range_pct".into(), json!(range_pct));
    features.insert("atr".into(), json!(atr));
    features.insert("volume".into(), json!(last.volume));
    features.insert("avg_volume".into(), json!(avg_volume));

    if !(cfg.min_range_pct..=cfg.max_range_pct).contains(&range_pct) {
        return fail(
            format!(
                "range filter: {:.3}% outside [{:.3}%, {:.3}%]",
                range_pct, cfg.min_range_pct, cfg.max_range_pct
            ),
            features,
        );
    }
    if avg_volume <= 0.0 || last.volume < avg_volume * cfg.volume_mult {
        return fail(
            format!(
                "volume confirmation failed: {:.4} < {:.4} x avg {:.4}",
                last.volume, cfg.volume_mult, avg_volume
            ),
            features,
        );
    }

    let up_trigger = range_high + atr * cfg.buffer_atr;
    let down_trigger = range_low - atr * cfg.buffer_atr;
    let direction = if close > up_trigger {
        Direction::Long
    } else if close < down_trigger {
        Direction::Short
    } else {
        return fail(
            format!(
                "no breakout: close {:.6} inside [{:.6}, {:.6}] with ATR buffer",
                close, down_trigger, up_trigger
            ),
            features,
        );
    };

    let stop = if direction == Direction::Long {
        close - atr * cfg.stop_atr_mult
    } else {
        close + atr * cfg.stop_atr_mult
    };
    let target = if direction == Direction::Long {
        close + atr * cfg.target_atr_mult
    } else {
        close - atr * cfg.target_atr_mult
    };
    if stop <= 0.0 || target <= 0.0 {
        return fail("ATR-derived stop/target is non-positive".into(), features);
    }

    let candidate = SignalDraft {
        strategy: StrategyId::Breakout,
        symbol: symbol.to_string(),
        timeframe,
        direction,
        candle_time_ms: last.open_time_ms,
        signal_time_ms: now_ms,
        entry_reference: close,
        stop,
        target,
        atr,
        trailing_distance: atr * cfg.trailing_atr_mult,
        max_hold_bars: cfg.max_hold_bars,
        reason: format!(
            "{} breakout of [{:.6}, {:.6}] with volume {:.4} vs avg {:.4}",
            direction, range_low, range_high, last.volume, avg_volume
        ),
        features: serde_json::Value::Object(features.clone()),
    };
    StrategyEval {
        strategy: StrategyId::Breakout,
        symbol: symbol.to_string(),
        timeframe,
        candidate: Some(candidate),
        note: "confirmed breakout".into(),
        features: serde_json::Value::Object(features),
    }
}

/// Strategy-specific exit on a completed candle. Protective stops/targets and
/// max holding periods are handled by the execution/backtest engine; this adds
/// invalidation rules that require closed-candle information.
pub fn invalidation_exit(
    strategy: StrategyId,
    cfg: &StrategyConfigs,
    candles: &[Candle],
    direction: Direction,
    entry_price: f64,
) -> Option<String> {
    let cls = indicators::closes(candles);
    match strategy {
        StrategyId::Trend => {
            let c = &cfg.trend;
            if candles.len() < c.ema_slow + 2 {
                return None;
            }
            let fast = indicators::ema(&cls, c.ema_fast)?;
            let slow = indicators::ema(&cls, c.ema_slow)?;
            let close = *cls.last()?;
            let invalid = match direction {
                Direction::Long => fast < slow && close < slow,
                Direction::Short => fast > slow && close > slow,
            };
            invalid.then(|| "trend invalidation on closed candle".to_string())
        }
        StrategyId::MeanReversion => {
            let c = &cfg.mean_reversion;
            let mean = indicators::sma(&cls, c.sma_period)?;
            let close = *cls.last()?;
            let reached = match direction {
                Direction::Long => close >= mean,
                Direction::Short => close <= mean,
            };
            reached.then(|| "price reverted to rolling mean".to_string())
        }
        StrategyId::Breakout => {
            let c = &cfg.breakout;
            if candles.len() < c.lookback + 2 {
                return None;
            }
            let close = *cls.last()?;
            let prior = &candles[candles.len() - 1 - c.lookback..candles.len() - 1];
            let high = prior.iter().map(|x| x.high).fold(f64::MIN, f64::max);
            let low = prior.iter().map(|x| x.low).fold(f64::MAX, f64::min);
            let failed = match direction {
                Direction::Long => close < high && entry_price > high,
                Direction::Short => close > low && entry_price < low,
            };
            failed.then(|| "failed breakout: close re-entered prior range".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StrategyConfigs;

    fn candles_from_closes(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .enumerate()
            .map(|(i, c)| Candle {
                symbol: "BTCUSDT".into(),
                interval: Interval::M5,
                open_time_ms: (i as i64) * 300_000,
                open: *c,
                high: c * 1.002,
                low: c * 0.998,
                close: *c,
                volume: 100.0,
                turnover: c * 100.0,
                confirm: true,
                source: "test".into(),
                ingested_at_ms: 0,
            })
            .collect()
    }

    #[test]
    fn trend_requires_confirmation_and_momentum() {
        let mut closes = vec![100.0; 30];
        for i in 0..100 {
            closes.push(100.0 + i as f64 * 0.5);
        }
        let cs = candles_from_closes(&closes);
        let cfg = TrendConfig::default();
        let eval = evaluate_trend("BTCUSDT", &cs, 1, &cfg);
        assert!(eval.candidate.is_some(), "note={}", eval.note);
        assert_eq!(eval.candidate.unwrap().direction, Direction::Long);
    }

    #[test]
    fn trend_rejects_chop() {
        let closes: Vec<f64> = (0..150).map(|i| 100.0 + (i as f64 * 0.7).sin()).collect();
        let cs = candles_from_closes(&closes);
        let eval = evaluate_trend("BTCUSDT", &cs, 1, &TrendConfig::default());
        assert!(eval.candidate.is_none());
    }

    #[test]
    fn mean_reversion_detects_downside_extreme() {
        let mut closes = vec![100.0; 210];
        closes.extend([99.8, 99.4, 98.8, 98.0]);
        let mut cs = candles_from_closes(&closes);
        // Ensure the last candle shows stabilization (close above prior close).
        let n = cs.len();
        cs[n - 1].open = 98.4;
        cs[n - 1].close = 98.85;
        cs[n - 1].high = 99.0;
        cs[n - 1].low = 98.0;
        let eval = evaluate_mean_reversion("BTCUSDT", &cs, 1, &MeanReversionConfig::default());
        assert!(eval.candidate.is_some(), "note={}", eval.note);
        assert_eq!(eval.candidate.unwrap().direction, Direction::Long);
    }

    #[test]
    fn mean_reversion_rejects_strong_trend() {
        let closes: Vec<f64> = (0..260).map(|i| 100.0 + i as f64 * 0.8).collect();
        let cs = candles_from_closes(&closes);
        let eval = evaluate_mean_reversion("BTCUSDT", &cs, 1, &MeanReversionConfig::default());
        assert!(eval.candidate.is_none());
    }

    #[test]
    fn breakout_detects_high_break() {
        let mut closes: Vec<f64> = (0..60).map(|i| 100.0 + (i % 10) as f64 * 0.1).collect();
        closes.push(104.0);
        let mut cs = candles_from_closes(&closes);
        let n = cs.len();
        cs[n - 1].open = 102.0;
        cs[n - 1].high = 104.5;
        cs[n - 1].low = 101.9;
        cs[n - 1].volume = 1000.0;
        let eval = evaluate_breakout("BTCUSDT", &cs, 1, &BreakoutConfig::default());
        assert!(eval.candidate.is_some(), "note={}", eval.note);
    }

    #[test]
    fn evaluators_ignore_unconfirmed_candle() {
        let mut closes: Vec<f64> = (0..120).map(|i| 100.0 + i as f64 * 0.5).collect();
        closes.push(300.0);
        let mut cs = candles_from_closes(&closes);
        cs.last_mut().unwrap().confirm = false;
        let eval = evaluate_trend("BTCUSDT", &cs, 1, &TrendConfig::default());
        assert!(eval.candidate.is_none());
        assert!(eval.note.contains("not confirmed"), "note={}", eval.note);
    }

    #[test]
    fn all_strategies_are_independent() {
        let cfg = StrategyConfigs::default();
        assert_eq!(cfg.timeframe(StrategyId::Trend), Interval::M5);
        assert_eq!(cfg.timeframe(StrategyId::MeanReversion), Interval::M5);
        assert_eq!(cfg.timeframe(StrategyId::Breakout), Interval::M5);
    }
}
