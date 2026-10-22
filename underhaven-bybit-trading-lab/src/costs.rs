//! Transaction-cost accounting and the edge-validation gate.
//!
//! Cost convention (avoiding double counting):
//! * The simulated fill price already includes the bid/ask spread: a long enters
//!   at the ask and exits at the bid. `spread_cost` below is a *reporting*
//!   estimate of that embedded cost, not a second cash deduction.
//! * `slippage` is an adverse adjustment added to the fill price; the estimate is
//!   reported for analysis and is also already embedded in the fill.
//! * `fees` and `funding` are cash ledger entries and are deducted/credited
//!   exactly once.
//! * The edge gate subtracts fees + funding reserve + estimated spread + estimated
//!   slippage from expected gross value. The ledger does not subtract the
//!   reporting estimates again.

use crate::config::{CostConfig, RiskConfig};
use crate::model::{Direction, OrderBook, Side, Ticker};
use crate::strategy::SignalDraft;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CostBreakdown {
    pub notional: f64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub entry_fee: f64,
    pub exit_fee: f64,
    pub spread_cost: f64,
    pub slippage: f64,
    pub funding_reserve: f64,
    pub total: f64,
    pub fee_source: String,
    pub funding_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeSample {
    pub gross_return: f64,
    pub net_return: f64,
    pub win: bool,
    pub notional: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeStats {
    pub n: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub win_rate_lower_95: f64,
    pub avg_win_return: f64,
    pub avg_loss_return: f64,
    pub profit_factor: Option<f64>,
    pub expectancy_return: f64,
    pub max_drawdown_pct: f64,
    pub sharpe: Option<f64>,
    pub sortino: Option<f64>,
    pub exposure_avg: f64,
    pub turnover: f64,
    pub cost_to_gross: Option<f64>,
    pub total_fees: f64,
    pub total_funding: f64,
    pub net_pnl: f64,
    pub gross_profit: f64,
    pub gross_loss: f64,
}

pub fn wilson_lower_bound(wins: usize, n: usize, z: f64) -> Option<f64> {
    if n == 0 || wins > n {
        return None;
    }
    let nf = n as f64;
    let p = wins as f64 / nf;
    let z2 = z * z;
    let denom = 1.0 + z2 / nf;
    let centre = p + z2 / (2.0 * nf);
    let margin = z * ((p * (1.0 - p) / nf) + z2 / (4.0 * nf * nf)).sqrt();
    Some(((centre - margin) / denom).clamp(0.0, 1.0))
}

/// Build conservative statistics from historical outcomes.
pub fn stats_from_samples(samples: &[OutcomeSample], initial_equity: f64) -> Option<EdgeStats> {
    if samples.is_empty() {
        return None;
    }
    let n = samples.len();
    let wins = samples.iter().filter(|s| s.net_return > 0.0).count();
    let losses = n - wins;
    let win_rate = wins as f64 / n as f64;
    let win_rate_lower_95 = wilson_lower_bound(wins, n, 1.96)?;
    let win_returns: Vec<f64> = samples
        .iter()
        .filter(|s| s.net_return > 0.0)
        .map(|s| s.gross_return)
        .collect();
    let loss_returns: Vec<f64> = samples
        .iter()
        .filter(|s| s.net_return <= 0.0)
        .map(|s| s.gross_return.abs())
        .collect();
    let avg_win_return = if win_returns.is_empty() {
        0.0
    } else {
        win_returns.iter().sum::<f64>() / win_returns.len() as f64
    };
    let avg_loss_return = if loss_returns.is_empty() {
        0.0
    } else {
        loss_returns.iter().sum::<f64>() / loss_returns.len() as f64
    };
    let gross_profit: f64 = samples
        .iter()
        .filter(|s| s.gross_return > 0.0)
        .map(|s| s.gross_return * s.notional)
        .sum();
    let gross_loss: f64 = samples
        .iter()
        .filter(|s| s.gross_return < 0.0)
        .map(|s| s.gross_return.abs() * s.notional)
        .sum();
    let net_returns: Vec<f64> = samples.iter().map(|s| s.net_return).collect();
    let expectancy_return = net_returns.iter().sum::<f64>() / n as f64;
    let std = sample_std(&net_returns);
    let downside: Vec<f64> = net_returns.iter().copied().filter(|r| *r < 0.0).collect();
    let downside_std = sample_std(&downside);
    let sharpe = if std > 1e-12 {
        Some(expectancy_return / std * (n as f64).sqrt())
    } else {
        None
    };
    let sortino = if downside_std > 1e-12 {
        Some(expectancy_return / downside_std * (n as f64).sqrt())
    } else {
        None
    };

    // Equity curve from returns, normalized to initial equity, for drawdown.
    let mut equity = initial_equity.max(1.0);
    let mut peak = equity;
    let mut max_dd: f64 = 0.0;
    let mut turnover = 0.0;
    for s in samples {
        equity *= 1.0 + s.net_return;
        peak = peak.max(equity);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - equity) / peak);
        }
        turnover += s.notional / initial_equity.max(1.0);
    }
    let notional_sum: f64 = samples.iter().map(|s| s.notional).sum();
    let exposure_avg = if n > 0 {
        notional_sum / n as f64 / initial_equity.max(1.0)
    } else {
        0.0
    };
    let net_pnl = samples.iter().map(|s| s.net_return * s.notional).sum();

    Some(EdgeStats {
        n,
        wins,
        losses,
        win_rate,
        win_rate_lower_95,
        avg_win_return,
        avg_loss_return,
        profit_factor: if gross_loss > 1e-12 {
            Some(gross_profit / gross_loss)
        } else {
            None
        },
        expectancy_return,
        max_drawdown_pct: max_dd * 100.0,
        sharpe,
        sortino,
        exposure_avg,
        turnover,
        cost_to_gross: if gross_profit > 1e-12 {
            Some((gross_profit - net_pnl) / gross_profit)
        } else {
            None
        },
        total_fees: 0.0,
        total_funding: 0.0,
        net_pnl,
        gross_profit,
        gross_loss,
    })
}

pub fn sample_std(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (values.len() as f64 - 1.0);
    var.sqrt()
}

/// Annualized Sharpe/Sortino from per-period returns. `periods_per_year` must be
/// documented by the caller (e.g. 365 for daily equity snapshots, or trade count
/// annualization explicitly labelled as such). Risk-free rate is zero.
pub fn annualized_ratio(
    returns: &[f64],
    periods_per_year: f64,
    downside_only: bool,
) -> Option<f64> {
    if returns.len() < 2 || periods_per_year <= 0.0 {
        return None;
    }
    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let denom = if downside_only {
        let downside: Vec<f64> = returns.iter().copied().filter(|r| *r < 0.0).collect();
        if downside.len() < 2 {
            return None;
        }
        sample_std(&downside)
    } else {
        sample_std(returns)
    };
    if denom <= 1e-12 {
        return None;
    }
    Some(mean / denom * periods_per_year.sqrt())
}

fn side_for_exit(direction: Direction) -> Side {
    direction.exit_side()
}

/// Worst-case observation price on the correct side of the book. Long entries
/// consume asks; long exits consume bids. This is a visible-depth estimate only.
fn reference_price(book: &OrderBook, side: Side) -> Option<f64> {
    match side {
        Side::Buy => book.best_ask().map(|l| l.price),
        Side::Sell => book.best_bid().map(|l| l.price),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeAssessment {
    pub accepted: bool,
    pub rejection_reason: Option<String>,
    pub checks: Vec<EdgeCheck>,
    pub costs: CostBreakdown,
    pub win_probability: Option<f64>,
    pub probability_source: String,
    pub expected_net_edge: Option<f64>,
    pub expected_gross_profit: f64,
    pub expected_gross_loss: f64,
    pub reward_risk: f64,
    pub cost_fraction_of_profit: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeCheck {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

#[allow(clippy::too_many_arguments)]
pub fn assess_trade(
    draft: &SignalDraft,
    qty: f64,
    book: &OrderBook,
    ticker: &Ticker,
    now_ms: i64,
    cost_cfg: &CostConfig,
    risk_cfg: &RiskConfig,
    stats: Option<&EdgeStats>,
    funding_rate: f64,
    expected_holding_hours: f64,
    min_probability_samples: usize,
) -> EdgeAssessment {
    let mut checks = Vec::new();
    let stale = now_ms - book.received_ms > risk_cfg.max_data_staleness_ms;
    checks.push(EdgeCheck {
        name: "book_freshness".into(),
        passed: !stale && book.valid,
        detail: format!(
            "book age {} ms, valid={}, source={}",
            now_ms - book.received_ms,
            book.valid,
            book.source
        ),
    });
    let ticker_stale = now_ms - ticker.ts_ms > risk_cfg.max_data_staleness_ms;
    checks.push(EdgeCheck {
        name: "ticker_freshness".into(),
        passed: !ticker_stale && ticker.is_valid(),
        detail: format!(
            "ticker age {} ms, bid={}, ask={}",
            now_ms - ticker.ts_ms,
            ticker.bid_price,
            ticker.ask_price
        ),
    });
    let spread = book.spread_bps();
    checks.push(EdgeCheck {
        name: "spread".into(),
        passed: spread <= risk_cfg.max_spread_bps,
        detail: format!(
            "spread {:.3} bps (limit {:.3} bps)",
            spread, risk_cfg.max_spread_bps
        ),
    });
    checks.push(EdgeCheck {
        name: "not_crossed".into(),
        passed: !book.is_crossed(),
        detail: format!("crossed={}", book.is_crossed()),
    });
    let rr = draft.reward_risk();
    checks.push(EdgeCheck {
        name: "reward_risk".into(),
        passed: rr >= cost_cfg.min_reward_risk,
        detail: format!("R:R {:.2} (minimum {:.2})", rr, cost_cfg.min_reward_risk),
    });

    let entry_side = draft.direction.entry_side();
    let exit_side = side_for_exit(draft.direction);
    let entry_fill = reference_price(book, entry_side).unwrap_or(draft.entry_reference);
    let exit_at_target = reference_price(book, exit_side).unwrap_or(draft.target);
    let notional = qty * entry_fill;
    let exit_notional = qty * exit_at_target;

    let fee_rate = cost_cfg.taker_fee_bps / 10_000.0;
    let entry_fee = notional * fee_rate;
    let exit_fee = exit_notional * fee_rate;
    let spread_bps = if spread.is_finite() {
        spread.max(0.0)
    } else {
        0.0
    };
    let spread_cost = notional * spread_bps / 10_000.0;
    let slippage = (notional + exit_notional) * (cost_cfg.slippage_bps / 10_000.0);
    let funding_intervals = if expected_holding_hours > 0.0 {
        (expected_holding_hours / 8.0).max(0.0)
    } else {
        0.0
    };
    let funding_reserve = notional * funding_rate.abs() * funding_intervals;
    let total = entry_fee + exit_fee + spread_cost + slippage + funding_reserve;

    let mut costs = CostBreakdown {
        notional,
        entry_price: entry_fill,
        exit_price: exit_at_target,
        entry_fee,
        exit_fee,
        spread_cost,
        slippage,
        funding_reserve,
        total,
        fee_source: cost_cfg.fee_source.clone(),
        funding_source: cost_cfg.funding_source.clone(),
    };

    let gross_profit = qty * (draft.target - entry_fill).abs();
    let gross_loss = qty * (entry_fill - draft.stop).abs();
    // Correct the exit reference for edge math: the target/stop defines the
    // projected exit, not the current opposite best quote.
    let projected_exit = draft.target;
    costs.exit_price = projected_exit;
    costs.exit_fee = qty * projected_exit * fee_rate;
    costs.slippage = (notional + qty * projected_exit) * (cost_cfg.slippage_bps / 10_000.0);
    costs.total = costs.entry_fee
        + costs.exit_fee
        + costs.spread_cost
        + costs.slippage
        + costs.funding_reserve;

    let cost_fraction = if gross_profit > 1e-12 {
        costs.total / gross_profit
    } else {
        f64::INFINITY
    };
    checks.push(EdgeCheck {
        name: "cost_budget".into(),
        passed: cost_fraction <= cost_cfg.max_cost_fraction_of_target,
        detail: format!(
            "costs {:.2} USD = {:.1}% of projected gross profit (limit {:.0}%); fees {:.2}, spread {:.2}, slippage {:.2}, funding reserve {:.2}",
            costs.total,
            cost_fraction * 100.0,
            cost_cfg.max_cost_fraction_of_target * 100.0,
            costs.entry_fee + costs.exit_fee,
            costs.spread_cost,
            costs.slippage,
            costs.funding_reserve
        ),
    });

    let (probability, probability_source, expected_net_edge) = match stats {
        Some(s) if s.n >= min_probability_samples => {
            let p = s.win_rate_lower_95;
            let exp_win = s.avg_win_return * notional;
            let exp_loss = s.avg_loss_return * notional;
            let edge = p * exp_win - (1.0 - p) * exp_loss - costs.total;
            (
                Some(p),
                format!(
                    "wilson_lower_95 from {} closed paper/backtest trades; avg win {:.3}%, avg loss {:.3}%",
                    s.n,
                    s.avg_win_return * 100.0,
                    s.avg_loss_return * 100.0
                ),
                Some(edge),
            )
        }
        Some(s) => (
            None,
            format!(
                "unavailable: only {} closed trades (minimum {}); cold-start experimental admission",
                s.n, min_probability_samples
            ),
            None,
        ),
        None => (
            None,
            format!(
                "unavailable: no closed trades yet (minimum {min_probability_samples}); cold-start experimental admission"
            ),
            None,
        ),
    };

    if let Some(edge) = expected_net_edge {
        checks.push(EdgeCheck {
            name: "positive_net_edge".into(),
            passed: edge > 0.0,
            detail: format!(
                "expected net edge {:.4} USD after all modeled costs; lower-bound win probability {:.2}%",
                edge,
                probability.unwrap_or(0.0) * 100.0
            ),
        });
    } else {
        checks.push(EdgeCheck {
            name: "positive_net_edge".into(),
            passed: true,
            detail: "NOT EVALUABLE: probability unavailable; trade is accepted only as a cold-start experiment with a hard cost cap".into(),
        });
    }

    let failures: Vec<String> = checks
        .iter()
        .filter(|c| !c.passed)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .collect();
    let accepted = failures.is_empty();
    EdgeAssessment {
        accepted,
        rejection_reason: if accepted {
            None
        } else {
            Some(failures.join("; "))
        },
        checks,
        costs,
        win_probability: probability,
        probability_source,
        expected_net_edge,
        expected_gross_profit: gross_profit,
        expected_gross_loss: gross_loss,
        reward_risk: rr,
        cost_fraction_of_profit: cost_fraction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BookLevel, Direction, Interval, StrategyId};
    use crate::strategy::SignalDraft;

    fn draft() -> SignalDraft {
        SignalDraft {
            strategy: StrategyId::Trend,
            symbol: "BTCUSDT".into(),
            timeframe: Interval::M5,
            direction: Direction::Long,
            candle_time_ms: 1_000_000,
            signal_time_ms: 1_000_001,
            entry_reference: 100.0,
            stop: 99.0,
            target: 102.0,
            atr: 1.0,
            trailing_distance: 1.0,
            max_hold_bars: 10,
            reason: "test".into(),
            features: serde_json::json!({}),
        }
    }

    fn book() -> OrderBook {
        OrderBook {
            symbol: "BTCUSDT".into(),
            ts_ms: 1_000_000,
            received_ms: 1_000_000,
            update_id: 1,
            seq: 1,
            bids: vec![BookLevel {
                price: 99.99,
                size: 1_000_000.0,
            }],
            asks: vec![BookLevel {
                price: 100.01,
                size: 1_000_000.0,
            }],
            source: "test".into(),
            valid: true,
            gap_count: 0,
        }
    }

    fn ticker() -> Ticker {
        Ticker {
            symbol: "BTCUSDT".into(),
            last_price: 100.0,
            bid_price: 99.99,
            ask_price: 100.01,
            ts_ms: 1_000_000,
            source: "test".into(),
            ..Ticker::default()
        }
    }

    #[test]
    fn cost_breakdown_is_complete_and_not_double_counted_in_ledger_terms() {
        let a = assess_trade(
            &draft(),
            1.0,
            &book(),
            &ticker(),
            1_000_000,
            &CostConfig::default(),
            &RiskConfig::default(),
            None,
            0.0001,
            4.0,
            30,
        );
        assert!(a.accepted, "{:?}", a.rejection_reason);
        assert!(a.costs.entry_fee > 0.0);
        assert!(a.costs.exit_fee > 0.0);
        assert!(a.costs.spread_cost > 0.0);
        assert!(a.costs.slippage > 0.0);
        assert!(a.costs.funding_reserve > 0.0);
        let sum = a.costs.entry_fee
            + a.costs.exit_fee
            + a.costs.spread_cost
            + a.costs.slippage
            + a.costs.funding_reserve;
        assert!((sum - a.costs.total).abs() < 1e-9);
    }

    #[test]
    fn stale_book_rejected() {
        let a = assess_trade(
            &draft(),
            1.0,
            &book(),
            &ticker(),
            1_100_000,
            &CostConfig::default(),
            &RiskConfig::default(),
            None,
            0.0,
            4.0,
            30,
        );
        assert!(!a.accepted);
        assert!(a.rejection_reason.unwrap().contains("book_freshness"));
    }

    #[test]
    fn expensive_trade_rejected_by_cost_budget() {
        let mut t = ticker();
        t.ask_price = 100.5;
        t.bid_price = 99.5;
        let b = OrderBook {
            bids: vec![BookLevel {
                price: 99.5,
                size: 1e9,
            }],
            asks: vec![BookLevel {
                price: 100.5,
                size: 1e9,
            }],
            ..book()
        };
        let a = assess_trade(
            &draft(),
            1.0,
            &b,
            &t,
            1_000_000,
            &CostConfig::default(),
            &RiskConfig::default(),
            None,
            0.0,
            4.0,
            30,
        );
        assert!(!a.accepted, "wide spread of 100 bps must be rejected");
    }

    #[test]
    fn cold_start_is_labelled_unavailable() {
        let a = assess_trade(
            &draft(),
            1.0,
            &book(),
            &ticker(),
            1_000_000,
            &CostConfig::default(),
            &RiskConfig::default(),
            None,
            0.0,
            4.0,
            30,
        );
        assert!(a.win_probability.is_none());
        assert!(a.probability_source.contains("unavailable"));
        assert!(a.expected_net_edge.is_none());
    }

    #[test]
    fn negative_expected_edge_rejected_with_probability() {
        let stats = EdgeStats {
            n: 100,
            wins: 25,
            losses: 75,
            win_rate: 0.25,
            win_rate_lower_95: 0.18,
            avg_win_return: 0.002,
            avg_loss_return: 0.002,
            profit_factor: Some(0.33),
            expectancy_return: -0.001,
            max_drawdown_pct: 20.0,
            sharpe: None,
            sortino: None,
            exposure_avg: 1.0,
            turnover: 100.0,
            cost_to_gross: None,
            total_fees: 0.0,
            total_funding: 0.0,
            net_pnl: -1.0,
            gross_profit: 1.0,
            gross_loss: 3.0,
        };
        let a = assess_trade(
            &draft(),
            1.0,
            &book(),
            &ticker(),
            1_000_000,
            &CostConfig::default(),
            &RiskConfig::default(),
            Some(&stats),
            0.0,
            4.0,
            30,
        );
        assert!(!a.accepted);
        assert!(a
            .rejection_reason
            .as_deref()
            .unwrap_or_default()
            .contains("positive_net_edge"));
    }

    #[test]
    fn wilson_lower_bound_is_below_point_estimate() {
        let p = wilson_lower_bound(50, 100, 1.96).unwrap();
        assert!(p < 0.5 && p > 0.39);
        assert!(wilson_lower_bound(0, 0, 1.96).is_none());
    }
}
