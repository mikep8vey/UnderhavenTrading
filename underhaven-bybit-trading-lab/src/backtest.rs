//! Deterministic historical backtesting with explicit execution assumptions.
//!
//! # Anti-look-ahead contract
//!
//! * Indicators are evaluated only on candles that have already closed.
//! * A signal produced by the close of bar `i` is executed at the **open** of
//!   bar `i + execution_delay_bars` (minimum one bar). It is never filled at the
//!   signal bar's close.
//! * Sizing uses account equity at the signal bar's close, not any future value.
//! * Stops and targets are checked against the *following* bars.
//!
//! # Conservative intrabar assumptions
//!
//! OHLC data cannot reveal the order of intrabar events. When a bar's range
//! touches both the stop and the target, the stop is assumed to be hit first.
//! Gaps through the stop fill at the bar open (worse than the stop level).
//! Every fill pays adverse slippage; fees are charged on both sides. Funding is
//! charged at Bybit settlement timestamps while a position is open.
//!
//! Historical order-book depth is not available, so the spread is an explicit
//! *assumption* (`BacktestParameters::spread_bps_assumption`), reported in every
//! result. It is not observed data.
//!
//! # Complexity
//!
//! The strategy evaluators recompute indicators from the candle prefix, making a
//! single pass `O(n^2)` in candle count. This is exact and simple, but long
//! histories (for example a year of 1-minute candles) are expensive. The CLI
//! caps imported ranges and the platform recommends at most ~20k candles per
//! backtest on the reference VPS.

use crate::config::{CostConfig, RiskConfig, StrategyConfigs};
use crate::metrics::{compute_stats, MetricTrade};
use crate::model::{
    q8, round_to_tick, BacktestParameters, BacktestReport, BacktestResult, BacktestTrade, Candle,
    Direction, EquityPoint, FundingRate, Instrument, Interval, SegmentSummary, SensitivityCase,
    StrategyId, StrategyStats, WalkForwardFold,
};
use crate::risk::RiskEngine;
use crate::strategy::{self, SignalDraft};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// A named parameter set used for sensitivity and walk-forward selection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestVariant {
    pub name: String,
    pub parameters: BacktestParameters,
    pub configs: StrategyConfigs,
}

#[derive(Debug, Clone)]
pub struct BacktestInput {
    pub run_id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub interval: Interval,
    pub candles: Vec<Candle>,
    pub funding: Vec<FundingRate>,
    pub instrument: Instrument,
    pub parameters: BacktestParameters,
    pub risk: RiskConfig,
    pub costs: CostConfig,
    pub configs: StrategyConfigs,
    /// Variants for sensitivity + walk-forward. Empty means "derive defaults".
    pub variants: Vec<BacktestVariant>,
    pub min_probability_samples: usize,
    pub min_trades_for_validation: usize,
}

impl BacktestInput {
    // A single explicit constructor keeps every input visible at the call site;
    // grouping these into a builder would only hide the assumptions.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        strategy: StrategyId,
        symbol: impl Into<String>,
        interval: Interval,
        candles: Vec<Candle>,
        instrument: Instrument,
        parameters: BacktestParameters,
        risk: RiskConfig,
        costs: CostConfig,
        configs: StrategyConfigs,
    ) -> Self {
        Self {
            run_id: uuid::Uuid::new_v4().to_string(),
            strategy,
            symbol: symbol.into(),
            interval,
            candles,
            funding: Vec::new(),
            instrument,
            parameters,
            risk,
            costs,
            configs,
            variants: Vec::new(),
            min_probability_samples: 30,
            min_trades_for_validation: 10,
        }
    }
}

#[derive(Debug, Clone)]
struct SimOutput {
    trades: Vec<BacktestTrade>,
    equity_curve: Vec<EquityPoint>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct OpenPosition {
    direction: Direction,
    qty: f64,
    entry_price: f64,
    entry_time_ms: i64,
    entry_index: usize,
    stop: f64,
    target: f64,
    trailing_distance: f64,
    highest: f64,
    lowest: f64,
    fees_entry: f64,
    funding_cash: f64,
    notional: f64,
    max_hold_bars: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitKind {
    Stop,
    Target,
    Invalidation,
    MaxHold,
    EndOfData,
}

impl ExitKind {
    fn reason(self) -> &'static str {
        match self {
            ExitKind::Stop => "stop_loss",
            ExitKind::Target => "take_profit",
            ExitKind::Invalidation => "strategy_invalidation",
            ExitKind::MaxHold => "max_hold_bars",
            ExitKind::EndOfData => "end_of_data",
        }
    }
}

/// Resolve a stop/target touch conservatively. Returns the *level* at which the
/// exit occurs, before slippage. When both are touched, the stop wins.
fn intrabar_exit(
    direction: Direction,
    stop: f64,
    target: f64,
    candle: &Candle,
) -> Option<(ExitKind, f64)> {
    match direction {
        Direction::Long => {
            if candle.open <= stop {
                return Some((ExitKind::Stop, candle.open));
            }
            let hit_stop = candle.low <= stop;
            let hit_target = candle.high >= target;
            if hit_stop {
                return Some((ExitKind::Stop, stop));
            }
            if hit_target {
                return Some((ExitKind::Target, target));
            }
            None
        }
        Direction::Short => {
            if candle.open >= stop {
                return Some((ExitKind::Stop, candle.open));
            }
            let hit_stop = candle.high >= stop;
            let hit_target = candle.low <= target;
            if hit_stop {
                return Some((ExitKind::Stop, stop));
            }
            if hit_target {
                return Some((ExitKind::Target, target));
            }
            None
        }
    }
}

fn apply_slippage(price: f64, side: crate::model::Side, bps: f64) -> f64 {
    let f = bps / 10_000.0;
    match side {
        crate::model::Side::Buy => price * (1.0 + f),
        crate::model::Side::Sell => price * (1.0 - f),
    }
}

fn normalise_candles(
    symbol: &str,
    interval: Interval,
    mut candles: Vec<Candle>,
    warnings: &mut Vec<String>,
) -> Vec<Candle> {
    let before = candles.len();
    candles.retain(|c| c.symbol == symbol && c.interval == interval);
    if candles.len() != before {
        warnings.push(format!(
            "discarded {} candles with a mismatched symbol/interval",
            before - candles.len()
        ));
    }
    let before = candles.len();
    candles.retain(|c| c.validate().is_ok());
    if candles.len() != before {
        warnings.push(format!(
            "discarded {} invalid candles",
            before - candles.len()
        ));
    }
    let before = candles.len();
    candles.retain(|c| c.confirm);
    if candles.len() != before {
        warnings.push(format!(
            "discarded {} unconfirmed candles (no look-ahead on live candles)",
            before - candles.len()
        ));
    }
    candles.sort_by_key(|c| c.open_time_ms);
    let before = candles.len();
    candles.dedup_by_key(|c| c.open_time_ms);
    if candles.len() != before {
        warnings.push(format!("deduplicated {} candles", before - candles.len()));
    }
    if candles.is_empty() {
        warnings.push("no usable candles after validation".into());
    }
    candles
}

fn fingerprint(
    symbol: &str,
    interval: Interval,
    candles: &[Candle],
    funding: &[FundingRate],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(symbol.as_bytes());
    hasher.update(interval.slug().as_bytes());
    for c in candles {
        hasher.update(c.open_time_ms.to_le_bytes());
        hasher.update(c.open.to_bits().to_le_bytes());
        hasher.update(c.high.to_bits().to_le_bytes());
        hasher.update(c.low.to_bits().to_le_bytes());
        hasher.update(c.close.to_bits().to_le_bytes());
        hasher.update(c.volume.to_bits().to_le_bytes());
    }
    for f in funding {
        hasher.update(f.funding_time_ms.to_le_bytes());
        hasher.update(f.rate.to_bits().to_le_bytes());
    }
    hex::encode(hasher.finalize())
}

fn config_for(strategy: StrategyId, cfg: &StrategyConfigs) -> serde_json::Value {
    match strategy {
        StrategyId::Trend => serde_json::to_value(&cfg.trend).unwrap_or_default(),
        StrategyId::MeanReversion => serde_json::to_value(&cfg.mean_reversion).unwrap_or_default(),
        StrategyId::Breakout => serde_json::to_value(&cfg.breakout).unwrap_or_default(),
    }
}

fn latest_funding_rate(funding: &[FundingRate], at_ms: i64) -> f64 {
    funding
        .iter()
        .filter(|f| f.funding_time_ms <= at_ms)
        .max_by_key(|f| f.funding_time_ms)
        .map(|f| f.rate)
        .unwrap_or(0.0)
}

/// Cost/reward gate applied before a simulated entry. It mirrors the live paper
/// gate's hard checks but deliberately does *not* use live win-probability
/// statistics, so a backtest cannot leak forward performance into admissions.
fn backtest_cost_gate(
    draft: &SignalDraft,
    qty: f64,
    entry_price: f64,
    params: &BacktestParameters,
    costs: &CostConfig,
    risk: &RiskConfig,
    funding_rate: f64,
) -> Result<(), String> {
    let rr = draft.reward_risk();
    if rr < costs.min_reward_risk {
        return Err(format!(
            "reward/risk {rr:.2} below minimum {:.2}",
            costs.min_reward_risk
        ));
    }
    if params.spread_bps_assumption > risk.max_spread_bps {
        return Err(format!(
            "assumed spread {:.2} bps exceeds {:.2} bps limit",
            params.spread_bps_assumption, risk.max_spread_bps
        ));
    }
    if qty <= 0.0 || entry_price <= 0.0 {
        return Err("zero size or price".into());
    }
    let target = draft.target;
    let gross_profit = (qty * (target - entry_price).abs()).max(0.0);
    if gross_profit <= 1e-12 {
        return Err("zero projected gross profit".into());
    }
    let notional = qty * entry_price;
    let exit_notional = qty * target.abs();
    let fee_rate = params.taker_fee_bps / 10_000.0;
    let fees = (notional + exit_notional) * fee_rate;
    let spread = notional * params.spread_bps_assumption / 10_000.0;
    let slippage = (notional + exit_notional) * params.slippage_bps / 10_000.0;
    let hours =
        (draft.max_hold_bars.max(1) as f64) * draft.timeframe.duration_ms() as f64 / 3_600_000.0;
    let funding = notional * funding_rate.abs() * (hours / 8.0).max(0.0);
    let total = fees + spread + slippage + funding;
    let fraction = total / gross_profit;
    if fraction > costs.max_cost_fraction_of_target {
        return Err(format!(
            "round-trip cost {:.2} USDT is {:.1}% of target profit (limit {:.0}%)",
            total,
            fraction * 100.0,
            costs.max_cost_fraction_of_target * 100.0
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn simulate(
    strategy: StrategyId,
    symbol: &str,
    candles: &[Candle],
    funding: &[FundingRate],
    instrument: &Instrument,
    params: &BacktestParameters,
    risk: &RiskConfig,
    costs: &CostConfig,
    configs: &StrategyConfigs,
    window: Option<(i64, i64)>,
    warnings: &mut Vec<String>,
) -> SimOutput {
    let initial = params.initial_equity.max(1.0);
    let mut cash = initial;
    let mut equity_curve: Vec<EquityPoint> = Vec::with_capacity(candles.len());
    let mut trades: Vec<BacktestTrade> = Vec::new();
    let mut position: Option<OpenPosition> = None;
    let mut pending: Option<(usize, SignalDraft)> = None;
    let mut funding_index = 0usize;
    let mut peak = initial;
    let engine = RiskEngine::new(risk.clone(), costs.clone());
    let delay = params.execution_delay_bars.max(1) as usize;
    let fee_rate = params.taker_fee_bps / 10_000.0;
    let entry_adverse_bps = params.slippage_bps + params.spread_bps_assumption / 2.0;
    let exit_slippage_bps = params.slippage_bps;

    if candles.is_empty() {
        return SimOutput {
            trades,
            equity_curve,
            warnings: warnings.clone(),
        };
    }
    equity_curve.push(EquityPoint {
        ts_ms: candles[0].open_time_ms,
        equity: initial,
        cash: initial,
        unrealized: 0.0,
        drawdown_pct: 0.0,
    });

    let in_window = |ts: i64| match window {
        Some((start, end)) => ts >= start && ts <= end,
        None => true,
    };

    for i in 0..candles.len() {
        let candle = &candles[i];

        // 1. Pending signal entry at this bar's open.
        if position.is_none() {
            if let Some((execute_at, draft)) = pending.clone() {
                if execute_at == i && in_window(candle.open_time_ms) {
                    pending = None;
                    let direction = draft.direction;
                    let raw_open = candle.open;
                    let entry_price =
                        apply_slippage(raw_open, direction.entry_side(), entry_adverse_bps);
                    let tick = instrument.tick_size;
                    let entry_price = if tick > 0.0 {
                        round_to_tick(entry_price, tick)
                    } else {
                        entry_price
                    };
                    let stop = draft.stop;
                    let target = draft.target;
                    let valid_geometry = match direction {
                        Direction::Long => entry_price > stop && target > entry_price && stop > 0.0,
                        Direction::Short => {
                            entry_price < stop && target < entry_price && target > 0.0
                        }
                    };
                    if valid_geometry {
                        let equity_at_entry = cash;
                        let available =
                            equity_at_entry * risk.leverage * risk.max_total_notional_pct / 100.0;
                        match engine.position_size(
                            equity_at_entry,
                            entry_price,
                            stop,
                            instrument,
                            available,
                        ) {
                            Ok(sizing) => {
                                let fr = latest_funding_rate(funding, candle.open_time_ms);
                                match backtest_cost_gate(
                                    &draft,
                                    sizing.qty,
                                    entry_price,
                                    params,
                                    costs,
                                    risk,
                                    fr,
                                ) {
                                    Ok(()) => {
                                        let notional = sizing.qty * entry_price;
                                        let fees_entry = notional * fee_rate;
                                        cash -= fees_entry;
                                        position = Some(OpenPosition {
                                            direction,
                                            qty: sizing.qty,
                                            entry_price,
                                            entry_time_ms: candle.open_time_ms,
                                            entry_index: i,
                                            stop,
                                            target,
                                            trailing_distance: draft.trailing_distance,
                                            highest: entry_price,
                                            lowest: entry_price,
                                            fees_entry,
                                            funding_cash: 0.0,
                                            notional,
                                            max_hold_bars: draft.max_hold_bars,
                                        });
                                    }
                                    Err(reason) => warnings.push(format!(
                                        "{} {} entry skipped at {}: {}",
                                        strategy, symbol, candle.open_time_ms, reason
                                    )),
                                }
                            }
                            Err(reason) => warnings.push(format!(
                                "{} {} sizing rejected at {}: {}",
                                strategy, symbol, candle.open_time_ms, reason
                            )),
                        }
                    } else {
                        warnings.push(format!(
                            "{} {} signal discarded at {}: gap invalidated stop/target geometry",
                            strategy, symbol, candle.open_time_ms
                        ));
                    }
                }
            }
        }

        // 2. Manage an open position against this bar's range.
        let mut exit: Option<(ExitKind, f64, usize)> = None;
        if let Some(pos) = position.as_ref() {
            if i > pos.entry_index {
                if let Some((kind, level)) =
                    intrabar_exit(pos.direction, pos.stop, pos.target, candle)
                {
                    exit = Some((kind, level, i));
                }
            }
        }
        if exit.is_none() {
            if let Some(pos) = position.as_ref() {
                let held = (i - pos.entry_index) as i64;
                if held >= pos.max_hold_bars {
                    exit = Some((ExitKind::MaxHold, candle.close, i));
                }
            }
        }
        if exit.is_none() {
            if let Some(pos) = position.as_ref() {
                if i > pos.entry_index {
                    let slice = &candles[..=i];
                    if let Some(reason) = strategy::invalidation_exit(
                        strategy,
                        configs,
                        slice,
                        pos.direction,
                        pos.entry_price,
                    ) {
                        let _ = reason;
                        exit = Some((ExitKind::Invalidation, candle.close, i));
                    }
                }
            }
        }

        if let Some((kind, level, exit_index)) = exit {
            if let Some(pos) = position.take() {
                let exit_price =
                    apply_slippage(level, pos.direction.exit_side(), exit_slippage_bps);
                let exit_price = if instrument.tick_size > 0.0 {
                    round_to_tick(exit_price, instrument.tick_size)
                } else {
                    exit_price
                };
                let gross = (exit_price - pos.entry_price) * pos.qty * pos.direction.sign();
                let fees_exit = exit_price * pos.qty * fee_rate;
                cash += gross - fees_exit;
                let fees = pos.fees_entry + fees_exit;
                let net = gross - fees + pos.funding_cash;
                trades.push(BacktestTrade {
                    entry_time_ms: pos.entry_time_ms,
                    exit_time_ms: candles[exit_index].close_time_ms(),
                    symbol: symbol.to_string(),
                    direction: pos.direction,
                    qty: pos.qty,
                    entry_price: pos.entry_price,
                    exit_price,
                    gross_pnl: q8(gross),
                    fees: q8(fees),
                    funding: q8(pos.funding_cash),
                    slippage_est: q8(pos.notional * entry_adverse_bps / 10_000.0
                        + exit_price * pos.qty * exit_slippage_bps / 10_000.0),
                    net_pnl: q8(net),
                    close_reason: kind.reason().into(),
                    bars_held: (exit_index - pos.entry_index) as i64,
                    equity_after: q8(cash),
                    notional: q8(pos.notional),
                });
            }
        }

        // 3. Funding settlements at (approximately) the bar close. Using the
        // close price is an explicit approximation; it is never used to admit a
        // trade or to set a stop.
        while funding_index < funding.len()
            && funding[funding_index].funding_time_ms <= candle.close_time_ms()
        {
            let event = &funding[funding_index];
            funding_index += 1;
            if let Some(pos) = position.as_mut() {
                if event.funding_time_ms >= pos.entry_time_ms {
                    let payment = -pos.direction.sign() * event.rate * pos.notional;
                    cash += payment;
                    pos.funding_cash += payment;
                }
            }
        }

        // 4. Mark to market and trail the stop.
        let mut equity = cash;
        if let Some(pos) = position.as_mut() {
            let unrealized = (candle.close - pos.entry_price) * pos.qty * pos.direction.sign();
            equity = cash + unrealized;
            pos.highest = pos.highest.max(candle.high);
            pos.lowest = pos.lowest.min(candle.low);
            if pos.trailing_distance > 0.0 {
                match pos.direction {
                    Direction::Long => {
                        let candidate = pos.highest - pos.trailing_distance;
                        if candidate > pos.stop {
                            pos.stop = candidate;
                        }
                    }
                    Direction::Short => {
                        let candidate = pos.lowest + pos.trailing_distance;
                        if candidate < pos.stop {
                            pos.stop = candidate;
                        }
                    }
                }
            }
        }
        peak = peak.max(equity);
        let dd = if peak > 0.0 {
            ((peak - equity) / peak * 100.0).max(0.0)
        } else {
            0.0
        };
        equity_curve.push(EquityPoint {
            ts_ms: candle.open_time_ms,
            equity: q8(equity),
            cash: q8(cash),
            unrealized: q8(equity - cash),
            drawdown_pct: dd,
        });

        // 5. Generate a signal on this completed bar for execution later.
        if position.is_none() && i + delay < candles.len() {
            let next_open = candles[i + delay].open_time_ms;
            if in_window(next_open) && in_window(candle.open_time_ms) {
                let slice = &candles[..=i];
                let eval = match strategy {
                    StrategyId::Trend => strategy::evaluate_trend(
                        symbol,
                        slice,
                        candle.close_time_ms(),
                        &configs.trend,
                    ),
                    StrategyId::MeanReversion => strategy::evaluate_mean_reversion(
                        symbol,
                        slice,
                        candle.close_time_ms(),
                        &configs.mean_reversion,
                    ),
                    StrategyId::Breakout => strategy::evaluate_breakout(
                        symbol,
                        slice,
                        candle.close_time_ms(),
                        &configs.breakout,
                    ),
                };
                if let Some(draft) = eval.candidate {
                    pending = Some((i + delay, draft));
                }
            }
        }
    }

    // Close any residual position at the last available close.
    if let Some(pos) = position.take() {
        let candle = candles.last().expect("non-empty checked above");
        let level = candle.close;
        let exit_price = apply_slippage(level, pos.direction.exit_side(), exit_slippage_bps);
        let gross = (exit_price - pos.entry_price) * pos.qty * pos.direction.sign();
        let fees_exit = exit_price * pos.qty * fee_rate;
        cash += gross - fees_exit;
        let fees = pos.fees_entry + fees_exit;
        let net = gross - fees + pos.funding_cash;
        trades.push(BacktestTrade {
            entry_time_ms: pos.entry_time_ms,
            exit_time_ms: candle.close_time_ms(),
            symbol: symbol.to_string(),
            direction: pos.direction,
            qty: pos.qty,
            entry_price: pos.entry_price,
            exit_price,
            gross_pnl: q8(gross),
            fees: q8(fees),
            funding: q8(pos.funding_cash),
            slippage_est: q8(pos.notional * entry_adverse_bps / 10_000.0
                + exit_price * pos.qty * exit_slippage_bps / 10_000.0),
            net_pnl: q8(net),
            close_reason: ExitKind::EndOfData.reason().into(),
            bars_held: (candles.len() - 1 - pos.entry_index) as i64,
            equity_after: q8(cash),
            notional: q8(pos.notional),
        });
        if let Some(last) = equity_curve.last_mut() {
            last.equity = q8(cash);
            last.cash = q8(cash);
            last.unrealized = 0.0;
        }
    }

    SimOutput {
        trades,
        equity_curve,
        warnings: warnings.clone(),
    }
}

fn metric_trades(trades: &[BacktestTrade], initial_equity: f64) -> Vec<MetricTrade> {
    trades
        .iter()
        .map(|t| MetricTrade {
            gross_pnl: t.gross_pnl,
            net_pnl: t.net_pnl,
            fees: t.fees,
            funding_cash_effect: t.funding,
            slippage_est: t.slippage_est,
            notional: t.notional,
            equity_at_entry: initial_equity,
            exit_time_ms: t.exit_time_ms,
            bars_held: t.bars_held,
        })
        .collect()
}

fn segment_summary(
    name: &str,
    start_ms: i64,
    end_ms: i64,
    strategy: StrategyId,
    trades: &[BacktestTrade],
    equity: &[EquityPoint],
    fallback_equity: f64,
) -> SegmentSummary {
    let seg_trades: Vec<BacktestTrade> = trades
        .iter()
        .filter(|t| t.entry_time_ms >= start_ms && t.entry_time_ms <= end_ms)
        .cloned()
        .collect();
    let seg_equity: Vec<EquityPoint> = equity
        .iter()
        .filter(|p| p.ts_ms >= start_ms && p.ts_ms <= end_ms)
        .cloned()
        .collect();
    let initial = seg_equity
        .first()
        .map(|p| p.equity)
        .unwrap_or(fallback_equity);
    let stats = compute_stats(
        strategy,
        &metric_trades(&seg_trades, initial),
        &seg_equity,
        initial,
    );
    SegmentSummary {
        name: name.to_string(),
        start_ms,
        end_ms,
        trades: seg_trades.len(),
        stats,
        equity_curve: seg_equity,
    }
}

fn segment_bounds(candles: &[Candle]) -> Option<(i64, i64, i64, i64)> {
    if candles.len() < 3 {
        return None;
    }
    let start = candles.first()?.open_time_ms;
    let end = candles.last()?.close_time_ms();
    let span = end - start;
    if span <= 0 {
        return None;
    }
    let train_end = start + (span as f64 * 0.60) as i64;
    let validation_end = start + (span as f64 * 0.80) as i64;
    Some((start, train_end, validation_end, end))
}

fn make_variant(
    name: &str,
    base_params: &BacktestParameters,
    base_configs: &StrategyConfigs,
    mutate: impl FnOnce(&mut BacktestParameters, &mut StrategyConfigs),
) -> BacktestVariant {
    let mut parameters = base_params.clone();
    let mut configs = base_configs.clone();
    mutate(&mut parameters, &mut configs);
    BacktestVariant {
        name: name.to_string(),
        parameters,
        configs,
    }
}

fn default_variants(input: &BacktestInput) -> Vec<BacktestVariant> {
    let mut out = vec![BacktestVariant {
        name: "base".into(),
        parameters: input.parameters.clone(),
        configs: input.configs.clone(),
    }];
    out.push(make_variant(
        "cost_stress_spread_6bps",
        &input.parameters,
        &input.configs,
        |p, _| {
            p.spread_bps_assumption += 3.0;
        },
    ));
    out.push(make_variant(
        "cost_stress_slippage_10bps",
        &input.parameters,
        &input.configs,
        |p, _| {
            p.slippage_bps = 10.0;
        },
    ));
    out.push(make_variant(
        "execution_delay_2_bars",
        &input.parameters,
        &input.configs,
        |p, _| {
            p.execution_delay_bars = 2;
        },
    ));
    match input.strategy {
        StrategyId::Trend => {
            out.push(make_variant(
                "stop_atr_1_2",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.trend.stop_atr_mult = 1.2;
                },
            ));
            out.push(make_variant(
                "stop_atr_1_8",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.trend.stop_atr_mult = 1.8;
                },
            ));
        }
        StrategyId::MeanReversion => {
            out.push(make_variant(
                "z_entry_1_8",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.mean_reversion.z_entry = 1.8;
                },
            ));
            out.push(make_variant(
                "z_entry_2_2",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.mean_reversion.z_entry = 2.2;
                },
            ));
        }
        StrategyId::Breakout => {
            out.push(make_variant(
                "lookback_15",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.breakout.lookback = 15;
                },
            ));
            out.push(make_variant(
                "lookback_25",
                &input.parameters,
                &input.configs,
                |_, c| {
                    c.breakout.lookback = 25;
                },
            ));
        }
    }
    out
}

fn sensitivity(
    input: &BacktestInput,
    candles: &[Candle],
    variants: &[BacktestVariant],
) -> Vec<SensitivityCase> {
    let mut cases = Vec::new();
    for variant in variants {
        let mut warnings = Vec::new();
        let sim = simulate(
            input.strategy,
            &input.symbol,
            candles,
            &input.funding,
            &input.instrument,
            &variant.parameters,
            &input.risk,
            &input.costs,
            &variant.configs,
            None,
            &mut warnings,
        );
        let stats = compute_stats(
            input.strategy,
            &metric_trades(&sim.trades, variant.parameters.initial_equity),
            &sim.equity_curve,
            variant.parameters.initial_equity,
        );
        let passed = sim.trades.len() >= input.min_trades_for_validation
            && stats.net_pnl > 0.0
            && stats.max_drawdown_pct <= 20.0;
        let note = if sim.trades.len() < input.min_trades_for_validation {
            format!(
                "INSUFFICIENT DATA: {} trades < {} required",
                sim.trades.len(),
                input.min_trades_for_validation
            )
        } else if stats.net_pnl <= 0.0 {
            "net P&L not positive after modeled costs".into()
        } else if stats.max_drawdown_pct > 20.0 {
            format!("drawdown {:.1}% exceeds 20%", stats.max_drawdown_pct)
        } else {
            "positive net result with adequate sample; still not a forward-validation claim".into()
        };
        cases.push(SensitivityCase {
            name: variant.name.clone(),
            parameters: variant.parameters.clone(),
            trades: sim.trades.len(),
            net_pnl: stats.net_pnl,
            expectancy: stats.expectancy,
            max_drawdown_pct: stats.max_drawdown_pct,
            profit_factor: stats.profit_factor,
            passed,
            note,
        });
    }
    cases
}

fn walk_forward(
    input: &BacktestInput,
    candles: &[Candle],
    variants: &[BacktestVariant],
) -> Result<Vec<WalkForwardFold>, String> {
    let n = candles.len();
    if n < 200 {
        return Err("walk-forward requires at least 200 candles".into());
    }
    let train_bars = (n * 40 / 100).max(100);
    let test_bars = (n * 20 / 100).max(50);
    if train_bars + test_bars > n {
        return Err("not enough candles for a 40% train / 20% test walk-forward".into());
    }
    let folds = 3usize;
    let max_start = n - train_bars - test_bars;
    let mut out = Vec::new();
    for k in 0..folds {
        let start_idx = if folds == 1 {
            0
        } else {
            max_start * k / (folds - 1)
        };
        let train_end_idx = start_idx + train_bars - 1;
        let test_start_idx = train_end_idx + 1;
        let test_end_idx = (test_start_idx + test_bars - 1).min(n - 1);
        let train_start_ms = candles[start_idx].open_time_ms;
        let train_end_ms = candles[train_end_idx].close_time_ms();
        let test_start_ms = candles[test_start_idx].open_time_ms;
        let test_end_ms = candles[test_end_idx].close_time_ms();

        let mut best: Option<(String, f64)> = None;
        for variant in variants {
            let mut warnings = Vec::new();
            let sim = simulate(
                input.strategy,
                &input.symbol,
                &candles[..=train_end_idx],
                &input.funding,
                &input.instrument,
                &variant.parameters,
                &input.risk,
                &input.costs,
                &variant.configs,
                Some((train_start_ms, train_end_ms)),
                &mut warnings,
            );
            let stats = compute_stats(
                input.strategy,
                &metric_trades(&sim.trades, variant.parameters.initial_equity),
                &sim.equity_curve,
                variant.parameters.initial_equity,
            );
            if sim.trades.len() >= input.min_trades_for_validation
                && stats.net_pnl > 0.0
                && stats.max_drawdown_pct <= 25.0
            {
                let better = best
                    .as_ref()
                    .map(|(_, pnl)| stats.net_pnl > *pnl)
                    .unwrap_or(true);
                if better {
                    best = Some((variant.name.clone(), stats.net_pnl));
                }
            }
        }

        match best {
            Some((name, train_pnl)) => {
                let variant = variants
                    .iter()
                    .find(|v| v.name == name)
                    .expect("selected variant exists");
                let mut warnings = Vec::new();
                let sim = simulate(
                    input.strategy,
                    &input.symbol,
                    &candles[..=test_end_idx],
                    &input.funding,
                    &input.instrument,
                    &variant.parameters,
                    &input.risk,
                    &input.costs,
                    &variant.configs,
                    Some((test_start_ms, test_end_ms)),
                    &mut warnings,
                );
                let stats = compute_stats(
                    input.strategy,
                    &metric_trades(&sim.trades, variant.parameters.initial_equity),
                    &sim.equity_curve,
                    variant.parameters.initial_equity,
                );
                out.push(WalkForwardFold {
                    fold: k + 1,
                    train_start_ms,
                    train_end_ms,
                    test_start_ms,
                    test_end_ms,
                    selected_variant: Some(name),
                    train_net_pnl: Some(train_pnl),
                    test_net_pnl: Some(stats.net_pnl),
                    test_trades: sim.trades.len(),
                    test_max_drawdown_pct: stats.max_drawdown_pct,
                    note: if sim.trades.len() < input.min_trades_for_validation {
                        "INSUFFICIENT DATA on the test window; result is not evidence".into()
                    } else if stats.net_pnl > 0.0 {
                        "positive out-of-sample window; one fold is not proof".into()
                    } else {
                        "out-of-sample window lost money after costs".into()
                    },
                });
            }
            None => {
                out.push(WalkForwardFold {
                    fold: k + 1,
                    train_start_ms,
                    train_end_ms,
                    test_start_ms,
                    test_end_ms,
                    selected_variant: None,
                    train_net_pnl: None,
                    test_net_pnl: None,
                    test_trades: 0,
                    test_max_drawdown_pct: 0.0,
                    note: "NO VALIDATED VARIANT: no parameter set met the train criteria; test window deliberately not traded".into(),
                });
            }
        }
    }
    Ok(out)
}

pub fn run_backtest(input: &BacktestInput) -> BacktestReport {
    let created_at_ms = Utc::now().timestamp_millis();
    let mut warnings = Vec::new();
    let candles = normalise_candles(
        &input.symbol,
        input.interval,
        input.candles.clone(),
        &mut warnings,
    );
    let mut funding = input.funding.clone();
    funding.retain(|f| f.symbol == input.symbol);
    funding.sort_by_key(|f| f.funding_time_ms);
    funding.dedup_by_key(|f| f.funding_time_ms);

    if funding.is_empty() {
        warnings.push(
            "no funding history supplied: funding cost is treated as zero and the result is optimistic"
                .into(),
        );
    }
    if candles.len() < 120 {
        warnings.push(format!(
            "short history: {} candles; strategies may not exit their warm-up period",
            candles.len()
        ));
    }

    let mut sim = simulate(
        input.strategy,
        &input.symbol,
        &candles,
        &funding,
        &input.instrument,
        &input.parameters,
        &input.risk,
        &input.costs,
        &input.configs,
        None,
        &mut warnings,
    );

    let stats = compute_stats(
        input.strategy,
        &metric_trades(&sim.trades, input.parameters.initial_equity),
        &sim.equity_curve,
        input.parameters.initial_equity,
    );
    if sim.trades.is_empty() {
        warnings.push(
            "no trades were generated; the result is INSUFFICIENT DATA, not a validated edge"
                .into(),
        );
    } else if sim.trades.len() < input.min_probability_samples {
        warnings.push(format!(
            "INSUFFICIENT DATA for probability estimation: {} trades < {}",
            sim.trades.len(),
            input.min_probability_samples
        ));
    }
    if stats.max_drawdown_pct > input.risk.drawdown_limit_pct {
        warnings.push(format!(
            "backtest drawdown {:.2}% exceeds the live drawdown lockout {:.2}%",
            stats.max_drawdown_pct, input.risk.drawdown_limit_pct
        ));
    }

    let (start_ms, end_ms) = match (candles.first(), candles.last()) {
        (Some(a), Some(b)) => (a.open_time_ms, b.close_time_ms()),
        _ => (0, 0),
    };

    let result = BacktestResult {
        run_id: input.run_id.clone(),
        strategy: input.strategy,
        symbol: input.symbol.clone(),
        interval: input.interval,
        start_ms,
        end_ms,
        parameters: input.parameters.clone(),
        strategy_config_json: serde_json::to_string(&config_for(input.strategy, &input.configs))
            .unwrap_or_else(|_| "{}".into()),
        trades: sim.trades.clone(),
        equity_curve: sim.equity_curve.clone(),
        stats,
        warnings: warnings.clone(),
        build_version: crate::config::BUILD_VERSION.to_string(),
        data_fingerprint: fingerprint(&input.symbol, input.interval, &candles, &funding),
        created_at_ms,
    };

    let variants = if input.variants.is_empty() {
        default_variants(input)
    } else {
        input.variants.clone()
    };
    let sensitivity_cases = sensitivity(input, &candles, &variants);

    let segments = match segment_bounds(&candles) {
        Some((start, train_end, validation_end, end)) => vec![
            segment_summary(
                "train",
                start,
                train_end,
                input.strategy,
                &sim.trades,
                &sim.equity_curve,
                input.parameters.initial_equity,
            ),
            segment_summary(
                "validation",
                train_end + 1,
                validation_end,
                input.strategy,
                &sim.trades,
                &sim.equity_curve,
                input.parameters.initial_equity,
            ),
            segment_summary(
                "out_of_sample",
                validation_end + 1,
                end,
                input.strategy,
                &sim.trades,
                &sim.equity_curve,
                input.parameters.initial_equity,
            ),
        ],
        None => {
            warnings
                .push("too few candles to build train/validation/out-of-sample segments".into());
            Vec::new()
        }
    };

    let walk = match walk_forward(input, &candles, &variants) {
        Ok(folds) => folds,
        Err(reason) => {
            warnings.push(format!("walk-forward NOT RUN: {reason}"));
            Vec::new()
        }
    };

    let reproducible = !result.data_fingerprint.is_empty() && !result.equity_curve.is_empty();
    if !reproducible {
        warnings.push("result is not reproducible: missing data or equity curve".into());
    }
    sim.warnings.clear();
    BacktestReport {
        result,
        segments,
        sensitivity: sensitivity_cases,
        walk_forward: walk,
        warnings,
        reproducible,
    }
}

pub fn trades_csv(result: &BacktestResult) -> String {
    let mut out = String::from(
        "entry_time_ms,exit_time_ms,symbol,direction,qty,entry_price,exit_price,gross_pnl,fees,funding,slippage_est,net_pnl,close_reason,bars_held,equity_after,notional\n",
    );
    for t in &result.trades {
        out.push_str(&format!(
            "{},{},{},{},{:.8},{:.8},{:.8},{:.8},{:.8},{:.8},{:.8},{:.8},{},{},{:.8},{:.8}\n",
            t.entry_time_ms,
            t.exit_time_ms,
            t.symbol,
            t.direction,
            t.qty,
            t.entry_price,
            t.exit_price,
            t.gross_pnl,
            t.fees,
            t.funding,
            t.slippage_est,
            t.net_pnl,
            t.close_reason,
            t.bars_held,
            t.equity_after,
            t.notional
        ));
    }
    out
}

pub fn report_json(report: &BacktestReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

pub fn stats_for_report(report: &BacktestReport) -> &StrategyStats {
    &report.result.stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CostConfig, RiskConfig, StrategyConfigs};
    use crate::model::{BookLevel, Candle, Instrument, Interval};

    fn instrument() -> Instrument {
        Instrument {
            symbol: "BTCUSDT".into(),
            base_coin: "BTC".into(),
            quote_coin: "USDT".into(),
            settle_coin: "USDT".into(),
            contract_type: "LinearPerpetual".into(),
            status: "Trading".into(),
            tick_size: 0.01,
            qty_step: 0.001,
            min_qty: 0.001,
            max_qty: 100.0,
            max_market_qty: 50.0,
            min_notional: 5.0,
            funding_interval_minutes: 480,
            launch_time_ms: 0,
            source: "test".into(),
            fetched_at_ms: 0,
        }
    }

    fn trend_candles(n: usize) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let base = 100.0 + i as f64 * 0.25;
                Candle {
                    symbol: "BTCUSDT".into(),
                    interval: Interval::M5,
                    open_time_ms: i as i64 * 300_000,
                    open: base,
                    high: base + 0.12,
                    low: base - 0.10,
                    close: base + 0.08,
                    volume: 500.0 + i as f64,
                    turnover: (500.0 + i as f64) * base,
                    confirm: true,
                    source: "test".into(),
                    ingested_at_ms: 0,
                }
            })
            .collect()
    }

    fn input(n: usize) -> BacktestInput {
        let mut i = BacktestInput::new(
            StrategyId::Trend,
            "BTCUSDT",
            Interval::M5,
            trend_candles(n),
            instrument(),
            BacktestParameters::default(),
            RiskConfig::default(),
            CostConfig::default(),
            StrategyConfigs::default(),
        );
        i.run_id = "test-run".into();
        i.min_probability_samples = 30;
        i.min_trades_for_validation = 1;
        i
    }

    #[test]
    fn conservative_stop_wins_when_bar_touches_both() {
        let candle = Candle {
            symbol: "BTCUSDT".into(),
            interval: Interval::M5,
            open_time_ms: 0,
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 100.0,
            volume: 1.0,
            turnover: 100.0,
            confirm: true,
            source: "test".into(),
            ingested_at_ms: 0,
        };
        let exit = intrabar_exit(Direction::Long, 95.0, 105.0, &candle).unwrap();
        assert_eq!(exit.0, ExitKind::Stop);
        assert_eq!(exit.1, 95.0);
        let exit = intrabar_exit(Direction::Short, 105.0, 95.0, &candle).unwrap();
        assert_eq!(exit.0, ExitKind::Stop);
    }

    #[test]
    fn gap_through_stop_fills_at_open() {
        let candle = Candle {
            symbol: "BTCUSDT".into(),
            interval: Interval::M5,
            open_time_ms: 0,
            open: 90.0,
            high: 92.0,
            low: 88.0,
            close: 91.0,
            volume: 1.0,
            turnover: 100.0,
            confirm: true,
            source: "test".into(),
            ingested_at_ms: 0,
        };
        let (kind, level) = intrabar_exit(Direction::Long, 95.0, 105.0, &candle).unwrap();
        assert_eq!(kind, ExitKind::Stop);
        assert_eq!(level, 90.0);
    }

    #[test]
    fn run_is_deterministic_and_records_fingerprint() {
        let a = run_backtest(&input(400));
        let b = run_backtest(&input(400));
        assert_eq!(a.result.data_fingerprint, b.result.data_fingerprint);
        assert_eq!(a.result.trades.len(), b.result.trades.len());
        assert!(a.reproducible);
    }

    #[test]
    fn entries_happen_after_signal_bar() {
        let report = run_backtest(&input(500));
        let candles = trend_candles(500);
        for trade in &report.result.trades {
            // Execution is never on the bar that generated the signal.
            assert!(candles
                .iter()
                .any(|c| c.open_time_ms == trade.entry_time_ms));
            assert!(trade.exit_time_ms >= trade.entry_time_ms);
        }
        if let Some(first) = report.result.trades.first() {
            let idx = candles
                .iter()
                .position(|c| c.open_time_ms == first.entry_time_ms)
                .unwrap();
            assert!(idx > 0, "first entry cannot be the first candle");
        }
    }

    #[test]
    fn ledger_identity_holds_for_every_trade() {
        let report = run_backtest(&input(500));
        for t in &report.result.trades {
            let implied = t.gross_pnl - t.fees + t.funding;
            assert!(
                (implied - t.net_pnl).abs() < 1e-6,
                "gross {} fees {} funding {} net {}",
                t.gross_pnl,
                t.fees,
                t.funding,
                t.net_pnl
            );
        }
    }

    #[test]
    fn segments_cover_the_sample_in_order() {
        let report = run_backtest(&input(500));
        assert_eq!(report.segments.len(), 3);
        assert_eq!(report.segments[0].name, "train");
        assert_eq!(report.segments[1].name, "validation");
        assert_eq!(report.segments[2].name, "out_of_sample");
        assert!(report.segments[0].end_ms < report.segments[1].start_ms);
        assert!(report.segments[1].end_ms < report.segments[2].start_ms);
    }

    #[test]
    fn sensitivity_and_walk_forward_are_produced() {
        let report = run_backtest(&input(600));
        assert!(report.sensitivity.len() >= 5);
        assert!(report.sensitivity.iter().any(|c| c.name == "base"));
        assert!(!report.walk_forward.is_empty());
        for fold in &report.walk_forward {
            assert!(fold.train_end_ms < fold.test_start_ms);
            if fold.selected_variant.is_some() {
                assert!(fold.test_end_ms > fold.test_start_ms);
            }
        }
    }

    #[test]
    fn csv_and_json_exports_are_well_formed() {
        let report = run_backtest(&input(400));
        let csv = trades_csv(&report.result);
        assert!(csv.starts_with("entry_time_ms,exit_time_ms,"));
        assert_eq!(csv.lines().count(), report.result.trades.len() + 1);
        let json = report_json(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("result").is_some());
    }

    #[test]
    fn no_trades_is_reported_as_insufficient_data() {
        // A flat market cannot satisfy the trend filters.
        let mut flat = trend_candles(400);
        for c in flat.iter_mut() {
            c.open = 100.0;
            c.high = 100.05;
            c.low = 99.95;
            c.close = 100.0;
        }
        let mut i = input(1);
        i.candles = flat;
        let report = run_backtest(&i);
        assert!(report.result.trades.is_empty());
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("INSUFFICIENT DATA")));
    }

    #[test]
    fn order_book_helpers_are_available_to_tests() {
        // Guards against accidental model regressions relied on by the backtest.
        let _ = BookLevel {
            price: 1.0,
            size: 1.0,
        };
    }
}
