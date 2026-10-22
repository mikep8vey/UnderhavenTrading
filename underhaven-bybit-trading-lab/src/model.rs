//! Core domain types shared by the market, strategy, backtest, paper and web layers.
//!
//! Numeric convention: the simulator uses `f64` for market and accounting values.
//! Every balance, fill and ledger amount is rounded to 1e-8 before persistence.
//! This is a *paper* research ledger; a future live system must use integer
//! quote-currency units or a fixed-precision decimal type.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

pub type Ts = i64;

pub const PAPER_ONLY: bool = true;
pub const INITIAL_EQUITY: f64 = 100.0;
pub const MONEY_SCALE: f64 = 1e8;

pub fn q8(v: f64) -> f64 {
    if !v.is_finite() {
        return 0.0;
    }
    (v * MONEY_SCALE).round() / MONEY_SCALE
}

pub fn round_down_to_step(value: f64, step: f64) -> f64 {
    if step <= 0.0 || !value.is_finite() {
        return q8(value.max(0.0));
    }
    let units = (value / step + 1e-9).floor();
    q8((units * step).max(0.0))
}

pub fn round_up_to_step(value: f64, step: f64) -> f64 {
    if step <= 0.0 || !value.is_finite() {
        return q8(value.max(0.0));
    }
    let units = (value / step - 1e-9).ceil();
    q8((units * step).max(0.0))
}

pub fn round_to_tick(price: f64, tick: f64) -> f64 {
    if tick <= 0.0 {
        return price;
    }
    (price / tick).round() * tick
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    pub fn sign(self) -> f64 {
        match self {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        }
    }
    pub fn entry_side(self) -> Side {
        match self {
            Direction::Long => Side::Buy,
            Direction::Short => Side::Sell,
        }
    }
    pub fn exit_side(self) -> Side {
        match self {
            Direction::Long => Side::Sell,
            Direction::Short => Side::Buy,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Long => "long",
            Direction::Short => "short",
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Direction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "long" | "buy" => Ok(Direction::Long),
            "short" | "sell" => Ok(Direction::Short),
            other => Err(format!("invalid direction: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyId {
    Trend,
    MeanReversion,
    Breakout,
}

impl StrategyId {
    pub const ALL: [StrategyId; 3] = [
        StrategyId::Trend,
        StrategyId::MeanReversion,
        StrategyId::Breakout,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            StrategyId::Trend => "trend",
            StrategyId::MeanReversion => "mean_reversion",
            StrategyId::Breakout => "breakout",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            StrategyId::Trend => "Trend Following",
            StrategyId::MeanReversion => "Mean Reversion",
            StrategyId::Breakout => "Breakout / Momentum",
        }
    }
}

impl fmt::Display for StrategyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StrategyId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "trend" | "trend_following" => Ok(StrategyId::Trend),
            "mean_reversion" | "mean-reversion" | "reversion" => Ok(StrategyId::MeanReversion),
            "breakout" | "momentum" => Ok(StrategyId::Breakout),
            other => Err(format!("unknown strategy: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Interval {
    M1,
    M5,
    M15,
    H1,
}

impl Interval {
    pub const ALL: [Interval; 4] = [Interval::M1, Interval::M5, Interval::M15, Interval::H1];

    pub fn bybit(self) -> &'static str {
        match self {
            Interval::M1 => "1",
            Interval::M5 => "5",
            Interval::M15 => "15",
            Interval::H1 => "60",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::H1 => "1h",
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Interval::M1 => 60_000,
            Interval::M5 => 300_000,
            Interval::M15 => 900_000,
            Interval::H1 => 3_600_000,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Interval::M1 => "1m",
            Interval::M5 => "5m",
            Interval::M15 => "15m",
            Interval::H1 => "1h",
        }
    }
}

impl fmt::Display for Interval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl FromStr for Interval {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "1" | "1m" | "M1" => Ok(Interval::M1),
            "5" | "5m" | "M5" => Ok(Interval::M5),
            "15" | "15m" | "M15" => Ok(Interval::M15),
            "60" | "1h" | "H1" => Ok(Interval::H1),
            other => Err(format!("unsupported interval: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instrument {
    pub symbol: String,
    pub base_coin: String,
    pub quote_coin: String,
    pub settle_coin: String,
    pub contract_type: String,
    pub status: String,
    pub tick_size: f64,
    pub qty_step: f64,
    pub min_qty: f64,
    pub max_qty: f64,
    pub max_market_qty: f64,
    pub min_notional: f64,
    pub funding_interval_minutes: u32,
    pub launch_time_ms: i64,
    pub source: String,
    pub fetched_at_ms: i64,
}

impl Instrument {
    /// Eligible scanner universe: USDT-settled linear perpetual, actively trading.
    pub fn is_eligible(&self) -> bool {
        self.status == "Trading"
            && self.contract_type == "LinearPerpetual"
            && self.quote_coin == "USDT"
            && self.settle_coin == "USDT"
            && self.tick_size > 0.0
            && self.qty_step > 0.0
            && self.min_qty > 0.0
            && self.min_notional > 0.0
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.symbol.is_empty() || !self.symbol.ends_with("USDT") {
            return Err("symbol must be a USDT instrument".into());
        }
        if self.tick_size <= 0.0 || !self.tick_size.is_finite() {
            return Err("tick size must be positive".into());
        }
        if self.qty_step <= 0.0 || !self.qty_step.is_finite() {
            return Err("quantity step must be positive".into());
        }
        if self.min_qty <= 0.0 || self.min_notional <= 0.0 {
            return Err("minimum quantity/notional must be positive".into());
        }
        if self.max_qty < self.min_qty {
            return Err("max quantity below min quantity".into());
        }
        Ok(())
    }

    pub fn normalize_qty(&self, qty: f64) -> f64 {
        round_down_to_step(qty.min(self.max_qty), self.qty_step)
    }

    pub fn normalize_price(&self, price: f64) -> f64 {
        round_to_tick(price, self.tick_size)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ticker {
    pub symbol: String,
    pub last_price: f64,
    pub mark_price: f64,
    pub index_price: f64,
    pub bid_price: f64,
    pub bid_size: f64,
    pub ask_price: f64,
    pub ask_size: f64,
    pub price_24h_pct: f64,
    pub turnover_24h: f64,
    pub volume_24h: f64,
    pub open_interest_value: f64,
    pub funding_rate: f64,
    pub next_funding_time_ms: i64,
    pub funding_interval_hours: f64,
    pub ts_ms: i64,
    pub source: String,
}

impl Ticker {
    pub fn mid(&self) -> f64 {
        if self.bid_price > 0.0 && self.ask_price > 0.0 {
            (self.bid_price + self.ask_price) / 2.0
        } else {
            self.last_price
        }
    }

    pub fn spread_bps(&self) -> f64 {
        let mid = self.mid();
        if mid <= 0.0 || self.ask_price <= 0.0 || self.bid_price <= 0.0 {
            return f64::INFINITY;
        }
        ((self.ask_price - self.bid_price) / mid) * 10_000.0
    }

    pub fn is_valid(&self) -> bool {
        self.last_price > 0.0
            && self.bid_price > 0.0
            && self.ask_price > 0.0
            && self.bid_price <= self.ask_price
            && !self.bid_price.is_nan()
            && !self.ask_price.is_nan()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrderBook {
    pub symbol: String,
    /// Exchange timestamp (ms) of the last snapshot/delta.
    pub ts_ms: i64,
    /// Local receipt timestamp (ms).
    pub received_ms: i64,
    pub update_id: i64,
    pub seq: i64,
    /// Descending price order.
    pub bids: Vec<BookLevel>,
    /// Ascending price order.
    pub asks: Vec<BookLevel>,
    pub source: String,
    pub valid: bool,
    pub gap_count: u64,
}

impl OrderBook {
    pub fn best_bid(&self) -> Option<BookLevel> {
        self.bids.first().copied()
    }

    pub fn best_ask(&self) -> Option<BookLevel> {
        self.asks.first().copied()
    }

    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()?.price + self.best_ask()?.price) / 2.0)
    }

    pub fn spread_bps(&self) -> f64 {
        match (self.best_bid(), self.best_ask(), self.mid()) {
            (Some(b), Some(a), Some(mid)) if mid > 0.0 => ((a.price - b.price) / mid) * 10_000.0,
            _ => f64::INFINITY,
        }
    }

    pub fn is_crossed(&self) -> bool {
        match (self.best_bid(), self.best_ask()) {
            (Some(b), Some(a)) => b.price >= a.price,
            _ => true,
        }
    }

    /// Notional (quote currency) available within `bps` of the mid on the
    /// side that a taker would consume.
    pub fn depth_notional(&self, side: Side, bps_from_mid: f64) -> f64 {
        let Some(mid) = self.mid() else { return 0.0 };
        let limit = mid * bps_from_mid / 10_000.0;
        let levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        levels
            .iter()
            .filter(|l| (l.price - mid).abs() <= limit)
            .map(|l| l.price * l.size)
            .sum()
    }

    /// Fill a taker order pessimistically through the visible book. Returns
    /// (filled_qty, vwap, quote_notional, levels_consumed).
    pub fn simulate_taker(&self, side: Side, requested_qty: f64) -> (f64, f64, f64, usize) {
        let levels = match side {
            Side::Buy => &self.asks,
            Side::Sell => &self.bids,
        };
        let mut remaining = requested_qty;
        let mut filled = 0.0;
        let mut notional = 0.0;
        let mut used = 0usize;
        for level in levels {
            if remaining <= 0.0 || level.size <= 0.0 || level.price <= 0.0 {
                break;
            }
            let take = remaining.min(level.size);
            filled += take;
            notional += take * level.price;
            remaining -= take;
            used += 1;
        }
        let vwap = if filled > 0.0 { notional / filled } else { 0.0 };
        (q8(filled), vwap, q8(notional), used)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Candle {
    pub symbol: String,
    pub interval: Interval,
    pub open_time_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub turnover: f64,
    pub confirm: bool,
    pub source: String,
    pub ingested_at_ms: i64,
}

impl Candle {
    pub fn close_time_ms(&self) -> i64 {
        self.open_time_ms + self.interval.duration_ms() - 1
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(self.open > 0.0 && self.high > 0.0 && self.low > 0.0 && self.close > 0.0) {
            return Err("non-positive OHLC".into());
        }
        if self.high < self.open.max(self.close) || self.low > self.open.min(self.close) {
            return Err("inconsistent OHLC range".into());
        }
        if self.high < self.low {
            return Err("high below low".into());
        }
        if self.volume < 0.0 || self.turnover < 0.0 {
            return Err("negative volume/turnover".into());
        }
        if self.open_time_ms % self.interval.duration_ms() != 0 {
            return Err("unaligned candle timestamp".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingRate {
    pub symbol: String,
    pub funding_time_ms: i64,
    pub rate: f64,
    pub source: String,
    pub fetched_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanRow {
    pub symbol: String,
    pub last_price: f64,
    pub spread_bps: f64,
    pub depth_bid_usd: f64,
    pub depth_ask_usd: f64,
    pub turnover_24h: f64,
    pub volatility_pct: f64,
    pub liquidity_score: f64,
    pub eligible: bool,
    pub reject_reason: Option<String>,
    pub selected: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub timeframe: String,
    pub direction: Direction,
    pub candle_time_ms: i64,
    pub signal_time_ms: i64,
    pub entry_reference: f64,
    pub proposed_entry: f64,
    pub stop: f64,
    pub target: f64,
    pub atr: f64,
    pub snapshot_json: String,
    pub expected_fees: f64,
    pub expected_funding: f64,
    pub expected_spread_cost: f64,
    pub expected_slippage: f64,
    pub expected_costs_total: f64,
    /// `None` when there is no statistically defensible probability estimate.
    pub win_probability: Option<f64>,
    pub probability_source: String,
    pub estimated_net_edge: Option<f64>,
    pub decision: String,
    pub rejection_reason: Option<String>,
    pub strategy_version: String,
    pub created_at_ms: i64,
}

impl Signal {
    pub fn accepted(&self) -> bool {
        self.decision == "accepted"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyStats {
    pub strategy: StrategyId,
    pub trades: u64,
    pub wins: u64,
    pub losses: u64,
    pub gross_profit: f64,
    pub gross_loss: f64,
    pub net_pnl: f64,
    pub fees: f64,
    pub funding: f64,
    pub avg_win: f64,
    pub avg_loss: f64,
    pub win_rate: f64,
    pub profit_factor: Option<f64>,
    pub expectancy: f64,
    pub max_drawdown: f64,
    pub max_drawdown_pct: f64,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub exposure_avg: f64,
    pub turnover: f64,
    pub cost_to_gross: Option<f64>,
    /// Lower bound of the 95% Wilson interval for the win rate.
    pub win_rate_lower_95: Option<f64>,
    pub probability_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EquityPoint {
    pub ts_ms: i64,
    pub equity: f64,
    pub cash: f64,
    pub unrealized: f64,
    pub drawdown_pct: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPosition {
    pub id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub direction: Direction,
    pub qty: f64,
    pub entry_price: f64,
    pub entry_time_ms: i64,
    pub mark_price: f64,
    pub stop: f64,
    pub initial_stop: f64,
    pub target: f64,
    pub trailing_distance: f64,
    pub highest_price: f64,
    pub lowest_price: f64,
    pub max_hold_until_ms: i64,
    pub fees_entry: f64,
    pub funding: f64,
    pub unrealized_pnl: f64,
    pub notional: f64,
    pub signal_id: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub direction: Direction,
    pub qty: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub gross_pnl: f64,
    pub fees: f64,
    pub funding: f64,
    pub slippage_est: f64,
    pub net_pnl: f64,
    pub close_reason: String,
    pub bars_held: i64,
    pub simulated_fill: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestParameters {
    pub initial_equity: f64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
    /// Historical order-book depth is not available for most periods. This is an
    /// explicit spread assumption, not observed data.
    pub spread_bps_assumption: f64,
    pub execution_delay_bars: u32,
    pub funding_model: String,
    pub data_source: String,
}

impl Default for BacktestParameters {
    fn default() -> Self {
        Self {
            initial_equity: INITIAL_EQUITY,
            taker_fee_bps: 7.5,
            slippage_bps: 4.0,
            spread_bps_assumption: 3.0,
            execution_delay_bars: 1,
            funding_model: "bybit_funding_history_at_settlement".into(),
            data_source: "bybit_v5_rest".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub entry_time_ms: i64,
    pub exit_time_ms: i64,
    pub symbol: String,
    pub direction: Direction,
    pub qty: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub gross_pnl: f64,
    pub fees: f64,
    pub funding: f64,
    pub slippage_est: f64,
    pub net_pnl: f64,
    pub close_reason: String,
    pub bars_held: i64,
    pub equity_after: f64,
    pub notional: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub run_id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub interval: Interval,
    pub start_ms: i64,
    pub end_ms: i64,
    pub parameters: BacktestParameters,
    pub strategy_config_json: String,
    pub trades: Vec<BacktestTrade>,
    pub equity_curve: Vec<EquityPoint>,
    pub stats: StrategyStats,
    pub warnings: Vec<String>,
    pub build_version: String,
    pub data_fingerprint: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSummary {
    pub name: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub trades: usize,
    pub stats: StrategyStats,
    pub equity_curve: Vec<EquityPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensitivityCase {
    pub name: String,
    pub parameters: BacktestParameters,
    pub trades: usize,
    pub net_pnl: f64,
    pub expectancy: f64,
    pub max_drawdown_pct: f64,
    pub profit_factor: Option<f64>,
    pub passed: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalkForwardFold {
    pub fold: usize,
    pub train_start_ms: i64,
    pub train_end_ms: i64,
    pub test_start_ms: i64,
    pub test_end_ms: i64,
    pub selected_variant: Option<String>,
    pub train_net_pnl: Option<f64>,
    pub test_net_pnl: Option<f64>,
    pub test_trades: usize,
    pub test_max_drawdown_pct: f64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestReport {
    pub result: BacktestResult,
    pub segments: Vec<SegmentSummary>,
    pub sensitivity: Vec<SensitivityCase>,
    pub walk_forward: Vec<WalkForwardFold>,
    pub warnings: Vec<String>,
    pub reproducible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeOutcome {
    pub win: bool,
    pub gross_win: f64,
    pub gross_loss: f64,
    pub net_pnl: f64,
    pub fees: f64,
    pub funding: f64,
    pub bars_held: i64,
    pub symbol: String,
    pub regime: String,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PerformanceSeries {
    pub equity: Vec<EquityPoint>,
    pub trades: Vec<ClosedTrade>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub generated_at_ms: i64,
    pub paper_only: bool,
    pub version: String,
    pub started_at_ms: i64,
    pub running: bool,
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub emergency_stop: bool,
    pub emergency_reason: Option<String>,
    pub ws_state: String,
    pub ws_last_message_ms: Option<i64>,
    pub ws_reconnects: u64,
    pub feed_ok: bool,
    pub feed_detail: String,
    pub clock_offset_ms: i64,
    pub initial_equity: f64,
    pub equity: f64,
    pub cash: f64,
    pub realized: f64,
    pub unrealized: f64,
    pub daily_pnl: f64,
    pub drawdown_pct: f64,
    pub fees: f64,
    pub funding: f64,
    pub estimated_slippage: f64,
    pub exposure: f64,
    pub open_positions: usize,
    pub last_update_ms: i64,
    pub strategies: Vec<StrategySummary>,
    pub scanner: Vec<ScanRow>,
    pub positions: Vec<PaperPosition>,
    pub trades: Vec<ClosedTrade>,
    pub risk: RiskStatus,
    pub system: SystemStatus,
    pub bybit: BybitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySummary {
    pub strategy: StrategyId,
    pub name: String,
    pub enabled: bool,
    pub timeframe: String,
    pub initial_equity: f64,
    pub equity: f64,
    pub cash: f64,
    pub realized: f64,
    pub unrealized: f64,
    pub daily_pnl: f64,
    pub fees: f64,
    pub funding: f64,
    pub estimated_slippage: f64,
    pub open_positions: usize,
    pub trades: u64,
    pub win_rate: f64,
    pub profit_factor: Option<f64>,
    pub expectancy: f64,
    pub max_drawdown_pct: f64,
    pub oos_status: String,
    pub forward_status: String,
    pub equity_curve: Vec<EquityPoint>,
    pub stats: StrategyStats,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RiskStatus {
    pub risk_per_trade_pct: f64,
    pub daily_loss_limit_pct: f64,
    pub drawdown_limit_pct: f64,
    pub max_open_positions: u32,
    pub max_total_notional_pct: f64,
    pub max_correlated_notional_pct: f64,
    pub max_consecutive_losses: u32,
    pub consecutive_losses: u32,
    pub daily_pnl: f64,
    pub daily_lockout: bool,
    pub drawdown_lockout: bool,
    pub cooldown_until_ms: Option<i64>,
    pub kill_switch: bool,
    pub blocked_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SystemStatus {
    pub uptime_seconds: i64,
    pub db_ok: bool,
    pub db_path: String,
    pub db_wal: bool,
    pub last_backup_ms: Option<i64>,
    pub build_version: String,
    pub git_commit: String,
    pub api_errors: u64,
    pub rate_limit_hits: u64,
    pub db_size_bytes: u64,
    pub audit_events: Vec<AuditEvent>,
    pub ws_history: Vec<WsEventView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BybitStatus {
    pub public_rest_ok: bool,
    pub public_ws_ok: bool,
    pub private_connected: bool,
    pub private_read_only: bool,
    pub read_only_verified: bool,
    pub uid: Option<String>,
    pub key_note: Option<String>,
    pub permissions_warning: Option<String>,
    pub maker_fee: Option<f64>,
    pub taker_fee: Option<f64>,
    pub fee_source: String,
    pub last_checked_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub ts_ms: i64,
    pub event_type: String,
    pub actor: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsEventView {
    pub ts_ms: i64,
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategySummaryRow {
    pub strategy: StrategyId,
    pub equity: f64,
    pub cash: f64,
    pub realized: f64,
    pub fees: f64,
    pub funding: f64,
    pub trades: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DailyPnl {
    pub day_start_equity: f64,
    pub equity: f64,
    pub pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigView {
    pub risk: RiskConfigView,
    pub costs: CostConfigView,
    pub strategies: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RiskConfigView {
    pub risk_per_trade_pct: f64,
    pub daily_loss_limit_pct: f64,
    pub drawdown_limit_pct: f64,
    pub max_open_positions: u32,
    pub max_total_notional_pct: f64,
    pub max_correlated_notional_pct: f64,
    pub max_consecutive_losses: u32,
    pub max_spread_bps: f64,
    pub min_depth_usd: f64,
    pub max_data_staleness_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostConfigView {
    pub taker_fee_bps: f64,
    pub maker_fee_bps: f64,
    pub slippage_bps: f64,
    pub fee_source: String,
    pub funding_source: String,
}
