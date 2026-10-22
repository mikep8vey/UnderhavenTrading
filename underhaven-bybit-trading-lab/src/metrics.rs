//! Shared performance statistics for independent strategy ledgers and backtests.
//!
//! Conventions:
//! * Win/loss is based on **net** P&L after fees and funding.
//! * Profit factor = sum(net wins) / abs(sum(net losses)); `None` when no losses.
//! * Sharpe/Sortino use daily equity snapshots (last snapshot per UTC day),
//!   annualized with 365 periods/year, zero risk-free rate. `None` when fewer
//!   than two daily observations or zero dispersion exist.
//! * Max drawdown is peak-to-trough on the supplied equity curve.
//! * Exposure is trade-weighted average notional / equity at entry; turnover is
//!   gross entry+exit notional / initial equity. Neither is time-weighted.
//! * Cost-to-gross = (fees + positive funding cost + slippage estimate) / gross profit.

use crate::costs::{annualized_ratio, wilson_lower_bound};
use crate::model::{EquityPoint, StrategyId, StrategyStats};
use chrono::{TimeZone, Utc};

#[derive(Debug, Clone)]
pub struct MetricTrade {
    pub gross_pnl: f64,
    pub net_pnl: f64,
    pub fees: f64,
    pub funding_cash_effect: f64,
    pub slippage_est: f64,
    pub notional: f64,
    pub equity_at_entry: f64,
    pub exit_time_ms: i64,
    pub bars_held: i64,
}

pub fn max_drawdown_pct(equity: &[EquityPoint]) -> f64 {
    let mut peak = f64::MIN;
    let mut max_dd: f64 = 0.0;
    for p in equity {
        peak = peak.max(p.equity);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - p.equity) / peak);
        }
    }
    max_dd * 100.0
}

pub fn compute_stats(
    strategy: StrategyId,
    trades: &[MetricTrade],
    equity: &[EquityPoint],
    initial_equity: f64,
) -> StrategyStats {
    let n = trades.len();
    let wins = trades.iter().filter(|t| t.net_pnl > 0.0).count() as u64;
    let losses = trades.iter().filter(|t| t.net_pnl <= 0.0).count() as u64;
    let gross_profit: f64 = trades
        .iter()
        .filter(|t| t.gross_pnl > 0.0)
        .map(|t| t.gross_pnl)
        .sum();
    let gross_loss: f64 = trades
        .iter()
        .filter(|t| t.gross_pnl < 0.0)
        .map(|t| t.gross_pnl.abs())
        .sum();
    let net_win: f64 = trades
        .iter()
        .filter(|t| t.net_pnl > 0.0)
        .map(|t| t.net_pnl)
        .sum();
    let net_loss: f64 = trades
        .iter()
        .filter(|t| t.net_pnl <= 0.0)
        .map(|t| t.net_pnl.abs())
        .sum();
    let total_fees: f64 = trades.iter().map(|t| t.fees).sum();
    let total_funding_cost: f64 = trades.iter().map(|t| -t.funding_cash_effect).sum();
    let total_slippage: f64 = trades.iter().map(|t| t.slippage_est).sum();
    let net_pnl: f64 = trades.iter().map(|t| t.net_pnl).sum();
    let avg_win = if wins > 0 { net_win / wins as f64 } else { 0.0 };
    let avg_loss = if losses > 0 {
        net_loss / losses as f64
    } else {
        0.0
    };
    let win_rate = if n > 0 { wins as f64 / n as f64 } else { 0.0 };
    let expectancy = if n > 0 { net_pnl / n as f64 } else { 0.0 };
    let exposure_avg = if n > 0 {
        trades
            .iter()
            .map(|t| {
                if t.equity_at_entry > 0.0 {
                    t.notional / t.equity_at_entry
                } else {
                    0.0
                }
            })
            .sum::<f64>()
            / n as f64
    } else {
        0.0
    };
    let turnover = if initial_equity > 0.0 {
        trades.iter().map(|t| t.notional * 2.0).sum::<f64>() / initial_equity
    } else {
        0.0
    };

    // Daily equity returns for annualized ratios.
    let mut daily: Vec<(i64, f64)> = Vec::new();
    for p in equity {
        let day = Utc
            .timestamp_millis_opt(p.ts_ms)
            .single()
            .map(|d| d.date_naive().and_hms_opt(0, 0, 0).unwrap_or_default())
            .map(|d| Utc.from_utc_datetime(&d).timestamp_millis())
            .unwrap_or(0);
        if let Some(last) = daily.last_mut() {
            if last.0 == day {
                last.1 = p.equity;
                continue;
            }
        }
        daily.push((day, p.equity));
    }
    let daily_returns: Vec<f64> = daily
        .windows(2)
        .filter(|w| w[0].1 > 0.0)
        .map(|w| (w[1].1 - w[0].1) / w[0].1)
        .collect();
    let sharpe = annualized_ratio(&daily_returns, 365.0, false);
    let sortino = annualized_ratio(&daily_returns, 365.0, true);
    let max_dd = max_drawdown_pct(equity);

    let probability_note = if n < 30 {
        format!("INSUFFICIENT DATA: {n} closed trades; probability not estimated")
    } else if n < 100 {
        format!("EXPERIMENTAL: {n} closed trades (<100); lower-bound estimate only")
    } else {
        format!("{n} closed trades; Wilson 95% lower bound reported")
    };

    StrategyStats {
        strategy,
        trades: n as u64,
        wins,
        losses,
        gross_profit,
        gross_loss,
        net_pnl,
        fees: total_fees,
        funding: total_funding_cost,
        avg_win,
        avg_loss,
        win_rate,
        profit_factor: if net_loss > 1e-12 {
            Some(net_win / net_loss)
        } else {
            None
        },
        expectancy,
        max_drawdown: {
            let mut peak = f64::MIN;
            let mut max_abs = 0.0f64;
            for p in equity {
                peak = peak.max(p.equity);
                if peak.is_finite() {
                    max_abs = max_abs.max(peak - p.equity);
                }
            }
            max_abs
        },
        max_drawdown_pct: max_dd,
        sharpe,
        sortino,
        exposure_avg,
        turnover,
        cost_to_gross: if gross_profit > 1e-12 {
            Some((total_fees + total_funding_cost.max(0.0) + total_slippage) / gross_profit)
        } else {
            None
        },
        win_rate_lower_95: wilson_lower_bound(wins as usize, n, 1.96),
        probability_note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq(ts: i64, e: f64) -> EquityPoint {
        EquityPoint {
            ts_ms: ts,
            equity: e,
            cash: e,
            unrealized: 0.0,
            drawdown_pct: 0.0,
        }
    }

    #[test]
    fn drawdown_is_peak_to_trough() {
        let curve = vec![eq(0, 100.0), eq(1, 120.0), eq(2, 90.0), eq(3, 110.0)];
        assert!((max_drawdown_pct(&curve) - 25.0).abs() < 1e-9);
    }

    #[test]
    fn profit_factor_and_expectancy() {
        let trades = vec![
            MetricTrade {
                gross_pnl: 2.0,
                net_pnl: 1.8,
                fees: 0.2,
                funding_cash_effect: 0.0,
                slippage_est: 0.05,
                notional: 100.0,
                equity_at_entry: 100.0,
                exit_time_ms: 1,
                bars_held: 2,
            },
            MetricTrade {
                gross_pnl: -1.0,
                net_pnl: -1.2,
                fees: 0.2,
                funding_cash_effect: 0.0,
                slippage_est: 0.05,
                notional: 100.0,
                equity_at_entry: 100.0,
                exit_time_ms: 2,
                bars_held: 2,
            },
        ];
        let curve = vec![eq(0, 100.0), eq(86_400_000, 100.6)];
        let s = compute_stats(StrategyId::Trend, &trades, &curve, 100.0);
        assert_eq!(s.trades, 2);
        assert!((s.win_rate - 0.5).abs() < 1e-9);
        assert!((s.net_pnl - 0.6).abs() < 1e-9);
        assert!((s.expectancy - 0.3).abs() < 1e-9);
        assert!(s.profit_factor.unwrap() > 1.0);
        assert!(s.win_rate_lower_95.unwrap() < 0.5);
    }

    #[test]
    fn no_losses_gives_none_profit_factor() {
        let trades = vec![MetricTrade {
            gross_pnl: 1.0,
            net_pnl: 0.9,
            fees: 0.1,
            funding_cash_effect: 0.0,
            slippage_est: 0.0,
            notional: 100.0,
            equity_at_entry: 100.0,
            exit_time_ms: 1,
            bars_held: 1,
        }];
        let s = compute_stats(StrategyId::Trend, &trades, &[eq(0, 100.0)], 100.0);
        assert_eq!(s.profit_factor, None);
        assert!(s.sharpe.is_none());
    }
}
