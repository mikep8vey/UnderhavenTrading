//! Risk limits and position sizing. This module is pure: it never contacts an
//! exchange and only consumes account/market state supplied by the paper engine.

use crate::config::{CostConfig, RiskConfig};
use crate::model::{round_down_to_step, Direction, Instrument};
use serde::{Deserialize, Serialize};

/// Inputs for a single entry decision. This is an internal, borrowed view and is
/// intentionally not serialized; only [`RiskVerdict`] crosses the API boundary.
#[derive(Debug, Clone)]
pub struct RiskContext<'a> {
    pub now_ms: i64,
    pub equity: f64,
    pub peak_equity: f64,
    pub day_start_equity: f64,
    pub daily_lockout: bool,
    pub drawdown_lockout: bool,
    pub cooldown_until_ms: Option<i64>,
    pub consecutive_losses: u32,
    pub open_positions: u32,
    pub total_notional: f64,
    pub correlated_notional: f64,
    pub paused: bool,
    pub emergency_stop: bool,
    pub feed_ok: bool,
    pub feed_detail: &'a str,
    pub spread_bps: f64,
    pub depth_usd: f64,
    pub direction: Direction,
    pub instrument: &'a Instrument,
    pub entry_price: f64,
    pub stop_price: f64,
    pub target_price: f64,
    pub taker_fee_bps: f64,
    pub slippage_bps: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskVerdict {
    pub approved: bool,
    pub reasons: Vec<String>,
    pub daily_pnl: f64,
    pub drawdown_pct: f64,
    pub risk_amount: f64,
    pub qty: f64,
    pub notional: f64,
    pub effective_risk: f64,
    pub reward_risk: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SizingResult {
    pub qty: f64,
    pub notional: f64,
    pub risk_amount: f64,
    pub effective_risk: f64,
    pub reasons: Vec<String>,
}

pub struct RiskEngine {
    pub limits: RiskConfig,
    pub costs: CostConfig,
}

pub fn correlation_group(symbol: &str) -> &'static str {
    let base = symbol.strip_suffix("USDT").unwrap_or(symbol);
    match base {
        "BTC" | "ETH" => "majors",
        "SOL" | "BNB" | "XRP" | "ADA" | "DOGE" | "AVAX" | "LINK" => "large_alts",
        _ => "alts",
    }
}

impl RiskEngine {
    pub fn new(limits: RiskConfig, costs: CostConfig) -> Self {
        Self { limits, costs }
    }

    pub fn position_size(
        &self,
        equity: f64,
        entry_price: f64,
        stop_price: f64,
        instrument: &Instrument,
        available_notional: f64,
    ) -> Result<SizingResult, String> {
        if !equity.is_finite()
            || equity <= 0.0
            || !entry_price.is_finite()
            || entry_price <= 0.0
            || !stop_price.is_finite()
            || stop_price <= 0.0
        {
            return Err("invalid equity/entry/stop for sizing".into());
        }
        let stop_distance = (entry_price - stop_price).abs();
        if stop_distance <= 0.0 {
            return Err("zero stop distance".into());
        }
        let risk_amount = equity * self.limits.risk_per_trade_pct / 100.0;
        // Include modeled round-trip costs in the per-unit loss estimate so the
        // 0.5% figure is not silently exceeded by fees/slippage at the stop.
        let cost_per_unit = entry_price
            * (2.0 * self.costs.taker_fee_bps
                + 2.0 * self.costs.slippage_bps
                + self.limits.max_spread_bps.min(20.0))
            / 10_000.0;
        let effective_loss_per_unit = stop_distance + cost_per_unit;
        let mut qty = risk_amount / effective_loss_per_unit;
        if available_notional > 0.0 {
            qty = qty.min(available_notional / entry_price);
        }
        qty = qty.min(instrument.max_qty).max(0.0);
        qty = round_down_to_step(qty, instrument.qty_step);
        let mut reasons = Vec::new();
        if qty < instrument.min_qty {
            return Err(format!(
                "size {:.8} below minimum quantity {:.8} after risk/exposure limits",
                qty, instrument.min_qty
            ));
        }
        let notional = qty * entry_price;
        if notional < instrument.min_notional {
            return Err(format!(
                "notional {:.2} below exchange minimum {:.2}",
                notional, instrument.min_notional
            ));
        }
        let effective_risk = qty * effective_loss_per_unit;
        if effective_risk > risk_amount * 1.001 {
            reasons.push(format!(
                "rounded risk {:.4} exceeds target {:.4}; reduce size",
                effective_risk, risk_amount
            ));
        }
        Ok(SizingResult {
            qty,
            notional: crate::model::q8(notional),
            risk_amount,
            effective_risk,
            reasons,
        })
    }

    pub fn evaluate_entry(&self, ctx: &RiskContext<'_>) -> RiskVerdict {
        let mut reasons = Vec::new();
        let daily_pnl = ctx.equity - ctx.day_start_equity;
        let daily_limit = -ctx.day_start_equity * self.limits.daily_loss_limit_pct / 100.0;
        let drawdown_pct = if ctx.peak_equity > 0.0 {
            ((ctx.peak_equity - ctx.equity) / ctx.peak_equity * 100.0).max(0.0)
        } else {
            0.0
        };

        if ctx.emergency_stop {
            reasons.push("emergency stop is active: new paper entries are blocked".to_string());
        }
        if ctx.paused {
            reasons.push("trading is manually paused".to_string());
        }
        if ctx.daily_lockout || daily_pnl <= daily_limit {
            reasons.push(format!(
                "daily loss lockout: P&L {:.4} USDT vs limit {:.4} USDT ({:.2}%)",
                daily_pnl, daily_limit, self.limits.daily_loss_limit_pct
            ));
        }
        if ctx.drawdown_lockout || drawdown_pct >= self.limits.drawdown_limit_pct {
            reasons.push(format!(
                "drawdown lockout: {:.2}% >= {:.2}%",
                drawdown_pct, self.limits.drawdown_limit_pct
            ));
        }
        if let Some(until) = ctx.cooldown_until_ms {
            if ctx.now_ms < until {
                reasons.push(format!(
                    "cooldown active until {} ({} min remaining)",
                    until,
                    (until - ctx.now_ms) / 60_000
                ));
            }
        }
        if ctx.consecutive_losses >= self.limits.max_consecutive_losses {
            reasons.push(format!(
                "{} consecutive losses >= limit {}",
                ctx.consecutive_losses, self.limits.max_consecutive_losses
            ));
        }
        if ctx.open_positions >= self.limits.max_open_positions {
            reasons.push(format!(
                "maximum open positions {} reached",
                self.limits.max_open_positions
            ));
        }
        if !ctx.feed_ok {
            reasons.push(format!("market data unhealthy: {}", ctx.feed_detail));
        }
        if ctx.spread_bps > self.limits.max_spread_bps {
            reasons.push(format!(
                "spread {:.3} bps > {:.3} bps limit",
                ctx.spread_bps, self.limits.max_spread_bps
            ));
        }
        if ctx.depth_usd < self.limits.min_depth_usd {
            reasons.push(format!(
                "visible depth {:.0} USDT < {:.0} USDT limit",
                ctx.depth_usd, self.limits.min_depth_usd
            ));
        }
        if !ctx.instrument.is_eligible() {
            reasons.push("instrument is not an eligible USDT linear perpetual".to_string());
        }
        if let Err(e) = ctx.instrument.validate() {
            reasons.push(format!("instrument validation failed: {e}"));
        }
        if self.limits.require_stop_loss && ctx.stop_price <= 0.0 {
            reasons.push("a positive stop loss is mandatory".to_string());
        }
        if ctx.direction == Direction::Long && ctx.stop_price >= ctx.entry_price {
            reasons.push("long stop must be below entry".to_string());
        }
        if ctx.direction == Direction::Short && ctx.stop_price <= ctx.entry_price {
            reasons.push("short stop must be above entry".to_string());
        }
        let max_notional =
            ctx.equity * self.limits.leverage * self.limits.max_total_notional_pct / 100.0;
        let available_notional = (max_notional - ctx.total_notional).max(0.0);
        if available_notional <= 0.0 {
            reasons.push(format!(
                "total notional exposure {:.2} at limit {:.2}",
                ctx.total_notional, max_notional
            ));
        }
        let corr_limit =
            ctx.equity * self.limits.leverage * self.limits.max_correlated_notional_pct / 100.0;
        if ctx.correlated_notional >= corr_limit {
            reasons.push(format!(
                "correlated ({}) notional {:.2} at limit {:.2}",
                correlation_group(&ctx.instrument.symbol),
                ctx.correlated_notional,
                corr_limit
            ));
        }

        let sizing = self
            .position_size(
                ctx.equity,
                ctx.entry_price,
                ctx.stop_price,
                ctx.instrument,
                available_notional,
            )
            .map_err(|e| reasons.push(e))
            .ok();
        let (qty, notional, risk_amount, effective_risk) = match sizing {
            Some(s) => (s.qty, s.notional, s.risk_amount, s.effective_risk),
            None => (0.0, 0.0, 0.0, 0.0),
        };
        if notional > available_notional + 1e-9 {
            reasons.push("sized notional exceeds available exposure".to_string());
        }
        let stop_distance = (ctx.entry_price - ctx.stop_price).abs();
        let target_distance = (ctx.target_price - ctx.entry_price).abs();
        let reward_risk = if stop_distance > 1e-12 {
            target_distance / stop_distance
        } else {
            0.0
        };
        if ctx.target_price > 0.0 {
            let target_on_correct_side = match ctx.direction {
                Direction::Long => ctx.target_price > ctx.entry_price,
                Direction::Short => ctx.target_price < ctx.entry_price,
            };
            if !target_on_correct_side {
                reasons.push("target is on the wrong side of entry".to_string());
            }
        }
        RiskVerdict {
            approved: reasons.is_empty() && qty > 0.0,
            reasons,
            daily_pnl,
            drawdown_pct,
            risk_amount,
            qty,
            notional,
            effective_risk,
            reward_risk,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Instrument;

    fn instrument() -> Instrument {
        Instrument {
            symbol: "BTCUSDT".into(),
            base_coin: "BTC".into(),
            quote_coin: "USDT".into(),
            settle_coin: "USDT".into(),
            contract_type: "LinearPerpetual".into(),
            status: "Trading".into(),
            tick_size: 0.1,
            qty_step: 0.001,
            min_qty: 0.001,
            max_qty: 100.0,
            max_market_qty: 10.0,
            min_notional: 5.0,
            funding_interval_minutes: 480,
            launch_time_ms: 0,
            source: "test".into(),
            fetched_at_ms: 0,
        }
    }

    fn ctx() -> RiskContext<'static> {
        // Constructing an owned instrument with a 'static lifetime for the test.
        let inst: &'static Instrument = Box::leak(Box::new(instrument()));
        RiskContext {
            now_ms: 1_000_000,
            equity: 100.0,
            peak_equity: 100.0,
            day_start_equity: 100.0,
            daily_lockout: false,
            drawdown_lockout: false,
            cooldown_until_ms: None,
            consecutive_losses: 0,
            open_positions: 0,
            total_notional: 0.0,
            correlated_notional: 0.0,
            paused: false,
            emergency_stop: false,
            feed_ok: true,
            feed_detail: "ok",
            spread_bps: 1.0,
            depth_usd: 1_000_000.0,
            direction: Direction::Long,
            instrument: inst,
            entry_price: 50_000.0,
            stop_price: 49_000.0,
            target_price: 52_000.0,
            taker_fee_bps: 7.5,
            slippage_bps: 4.0,
        }
    }

    #[test]
    fn sizing_respects_risk_and_instrument_steps() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let s = e
            .position_size(100.0, 50_000.0, 49_900.0, &instrument(), 200.0)
            .unwrap();
        assert!(s.qty <= 0.001 + 1e-12, "qty={}", s.qty);
        assert!(s.notional <= 100.0);
    }

    #[test]
    fn minimum_notional_rejected() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let err = e
            .position_size(1.0, 50_000.0, 49_000.0, &instrument(), 1.0)
            .unwrap_err();
        assert!(err.contains("minimum quantity") || err.contains("minimum"));
    }

    #[test]
    fn daily_loss_blocks() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let mut c = ctx();
        c.equity = 97.9;
        let v = e.evaluate_entry(&c);
        assert!(!v.approved);
        assert!(v.reasons.iter().any(|r| r.contains("daily loss")));
    }

    #[test]
    fn drawdown_blocks() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let mut c = ctx();
        c.peak_equity = 120.0;
        c.equity = 100.0;
        let v = e.evaluate_entry(&c);
        assert!(!v.approved);
        assert!(v.reasons.iter().any(|r| r.contains("drawdown")));
    }

    #[test]
    fn max_positions_blocks() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let mut c = ctx();
        c.open_positions = 2;
        let v = e.evaluate_entry(&c);
        assert!(!v.approved);
        assert!(v
            .reasons
            .iter()
            .any(|r| r.contains("maximum open positions")));
    }

    #[test]
    fn emergency_stop_blocks() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let mut c = ctx();
        c.emergency_stop = true;
        let v = e.evaluate_entry(&c);
        assert!(!v.approved);
        assert!(v.reasons.iter().any(|r| r.contains("emergency")));
    }

    #[test]
    fn stale_feed_blocks() {
        let e = RiskEngine::new(RiskConfig::default(), CostConfig::default());
        let mut c = ctx();
        c.feed_ok = false;
        c.feed_detail = "stale";
        let v = e.evaluate_entry(&c);
        assert!(!v.approved);
    }

    #[test]
    fn correlation_groups_are_stable() {
        assert_eq!(correlation_group("BTCUSDT"), "majors");
        assert_eq!(correlation_group("SOLUSDT"), "large_alts");
        assert_eq!(correlation_group("PEPEUSDT"), "alts");
    }
}
