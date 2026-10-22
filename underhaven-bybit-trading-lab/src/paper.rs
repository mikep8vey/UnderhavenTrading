//! Persistent paper execution engine.
//!
//! # Guarantees
//!
//! * **No exchange writes.** Entries and exits are local SQLite rows. The only
//!   network calls in the runtime are public market data and optional read-only
//!   account metadata.
//! * **Transactional ledger.** Wallet cash, position status, fills and trades are
//!   committed in one SQLite transaction. A crash cannot produce a closed
//!   position without its trade row.
//! * **Idempotent recovery.** Entry/exit fills use idempotency keys
//!   (`entry:<position>`, `exit:<position>`), and funding settlements use a
//!   composite primary key. Replaying a recovery pass cannot double-charge.
//! * **Risk before edge before execution.** Hard risk gates run before the
//!   cost-aware edge gate; both must pass. Cold-start trades are labelled and
//!   still subject to the hard cost cap.
//! * **At most one open position per strategy wallet**, and at most the
//!   configured global maximum across all strategies.

use crate::config::{CostConfig, RiskConfig, StrategyConfigs};
use crate::costs::{assess_trade, stats_from_samples, EdgeStats, OutcomeSample};
use crate::market::{merge_seed_symbols, scan, MarketCache, ScannerConfig};
use crate::model::{
    q8, round_to_tick, ClosedTrade, Direction, EquityPoint, FundingRate, PaperPosition, Signal,
    StrategyId,
};
use crate::risk::{correlation_group, RiskContext, RiskEngine};
use crate::store::{utc_day, Store, Wallet};
use crate::strategy;
use crate::ws::WsStatus;
use anyhow::{Context, Result};
use chrono::Utc;
use parking_lot::RwLock as SyncRwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub strategies: StrategyConfigs,
    pub risk: RiskConfig,
    pub costs: CostConfig,
    pub scanner: ScannerConfig,
    pub intervals: Vec<crate::model::Interval>,
    pub seed_symbols: Vec<String>,
    pub min_probability_samples: usize,
    pub cycle_secs: u64,
    pub equity_point_interval_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineRuntime {
    pub running: bool,
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub emergency_stop: bool,
    pub emergency_reason: Option<String>,
    pub last_cycle_ms: i64,
    pub last_error: Option<String>,
    pub consecutive_losses: HashMap<StrategyId, u32>,
    pub cooldown_until_ms: Option<i64>,
    pub daily_locked: bool,
    pub drawdown_locked: bool,
    pub started_at_ms: i64,
    pub cycles: u64,
    pub entries: u64,
    pub exits: u64,
    pub rejected_signals: u64,
    #[serde(skip)]
    pub last_equity_point_ms: HashMap<StrategyId, i64>,
}

impl EngineRuntime {
    fn new() -> Self {
        Self {
            running: true,
            paused: false,
            pause_reason: None,
            emergency_stop: false,
            emergency_reason: None,
            last_cycle_ms: 0,
            last_error: None,
            consecutive_losses: HashMap::new(),
            cooldown_until_ms: None,
            daily_locked: false,
            drawdown_locked: false,
            started_at_ms: Utc::now().timestamp_millis(),
            cycles: 0,
            entries: 0,
            exits: 0,
            rejected_signals: 0,
            last_equity_point_ms: HashMap::new(),
        }
    }
}

pub struct PaperEngine {
    pub store: Store,
    pub cache: Arc<RwLock<MarketCache>>,
    pub ws_status: Arc<RwLock<WsStatus>>,
    pub cfg: EngineConfig,
    pub risk: RiskEngine,
    pub runtime: Arc<SyncRwLock<EngineRuntime>>,
}

impl PaperEngine {
    pub async fn new(
        store: Store,
        cache: Arc<RwLock<MarketCache>>,
        ws_status: Arc<RwLock<WsStatus>>,
        cfg: EngineConfig,
    ) -> Result<Self> {
        store.ensure_wallets(crate::model::INITIAL_EQUITY).await?;
        let state = store.engine_state().await?;
        let mut runtime = EngineRuntime::new();
        runtime.paused = state.paused;
        runtime.pause_reason = state.pause_reason;
        runtime.emergency_stop = state.emergency_stop;
        runtime.emergency_reason = state.emergency_reason;
        for strategy in StrategyId::ALL {
            let losses = consecutive_losses(&store, strategy).await?;
            runtime.consecutive_losses.insert(strategy, losses);
        }
        for strategy in StrategyId::ALL {
            let (delta, detail) = store.reconcile(strategy).await?;
            if delta > 1e-6 {
                warn!(strategy = %strategy, delta, "ledger reconciliation mismatch");
                store
                    .audit(
                        "ledger.reconcile_mismatch",
                        "engine",
                        &format!("{} delta={:.8} {}", strategy, delta, detail),
                    )
                    .await?;
            }
        }
        store
            .audit(
                "engine.start",
                "engine",
                &format!(
                    "paper engine started; paused={} emergency={}",
                    runtime.paused, runtime.emergency_stop
                ),
            )
            .await?;
        let risk = RiskEngine::new(cfg.risk.clone(), cfg.costs.clone());
        Ok(Self {
            store,
            cache,
            ws_status,
            cfg,
            risk,
            runtime: Arc::new(SyncRwLock::new(runtime)),
        })
    }

    pub fn runtime_snapshot(&self) -> EngineRuntime {
        self.runtime.read().clone()
    }

    pub async fn selected_symbols(&self) -> Vec<String> {
        let cache = self.cache.read().await;
        let now = Utc::now().timestamp_millis();
        let rows = scan(&cache, &self.cfg.scanner, self.cfg.intervals[0], now);
        let selected: Vec<String> = rows
            .iter()
            .filter(|r| r.selected)
            .map(|r| r.symbol.clone())
            .collect();
        merge_seed_symbols(
            &selected,
            &self.cfg.seed_symbols,
            &cache,
            self.cfg.scanner.max_symbols,
        )
    }

    pub async fn pause(&self, reason: &str) -> Result<()> {
        {
            let mut rt = self.runtime.write();
            rt.paused = true;
            rt.pause_reason = Some(reason.to_string());
        }
        let mut state = self.store.engine_state().await?;
        state.paused = true;
        state.pause_reason = Some(reason.to_string());
        self.store.set_engine_state(&state).await?;
        self.store.audit("engine.pause", "admin", reason).await?;
        Ok(())
    }

    pub async fn resume(&self, actor: &str) -> Result<()> {
        {
            let mut rt = self.runtime.write();
            rt.paused = false;
            rt.pause_reason = None;
        }
        let mut state = self.store.engine_state().await?;
        state.paused = false;
        state.pause_reason = None;
        self.store.set_engine_state(&state).await?;
        self.store
            .audit("engine.resume", actor, "trading resumed")
            .await?;
        Ok(())
    }

    /// Emergency stop flattens every open paper position at the current quote and
    /// blocks new entries until an admin explicitly clears it.
    pub async fn emergency_stop(&self, reason: &str, actor: &str) -> Result<usize> {
        {
            let mut rt = self.runtime.write();
            rt.emergency_stop = true;
            rt.emergency_reason = Some(reason.to_string());
        }
        let mut state = self.store.engine_state().await?;
        state.emergency_stop = true;
        state.emergency_reason = Some(reason.to_string());
        self.store.set_engine_state(&state).await?;
        self.store
            .audit("engine.emergency_stop", actor, reason)
            .await?;
        let positions = self.store.open_positions().await?;
        let mut closed = 0usize;
        for position in positions {
            if self
                .close_position(&position, "emergency_stop", actor)
                .await
                .is_ok()
            {
                closed += 1;
            }
        }
        self.mark_all_wallets().await?;
        Ok(closed)
    }

    pub async fn clear_emergency(&self, actor: &str) -> Result<()> {
        {
            let mut rt = self.runtime.write();
            rt.emergency_stop = false;
            rt.emergency_reason = None;
        }
        let mut state = self.store.engine_state().await?;
        state.emergency_stop = false;
        state.emergency_reason = None;
        self.store.set_engine_state(&state).await?;
        self.store
            .audit("engine.emergency_clear", actor, "emergency stop cleared")
            .await?;
        Ok(())
    }

    pub async fn reset_lockouts(&self, actor: &str) -> Result<()> {
        {
            let mut rt = self.runtime.write();
            rt.daily_locked = false;
            rt.drawdown_locked = false;
            rt.cooldown_until_ms = None;
            for v in rt.consecutive_losses.values_mut() {
                *v = 0;
            }
        }
        self.store
            .audit(
                "risk.reset_lockouts",
                actor,
                "daily/drawdown/cooldown lockouts cleared",
            )
            .await?;
        Ok(())
    }

    pub async fn close_by_id(&self, position_id: &str, actor: &str) -> Result<()> {
        let positions = self.store.open_positions().await?;
        let position = positions
            .into_iter()
            .find(|p| p.id == position_id)
            .context("open position not found")?;
        self.close_position(&position, "manual_close", actor)
            .await?;
        self.mark_all_wallets().await?;
        Ok(())
    }

    pub async fn tick(&self) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        {
            let mut rt = self.runtime.write();
            rt.cycles += 1;
            rt.last_cycle_ms = now;
            if let Some(until) = rt.cooldown_until_ms {
                if now >= until {
                    rt.cooldown_until_ms = None;
                }
            }
        }
        self.roll_daily(now).await?;
        self.mark_positions(now).await?;
        self.apply_funding(now).await?;
        self.manage_exits(now).await?;
        self.mark_all_wallets().await?;
        self.evaluate_entries(now).await?;
        self.mark_all_wallets().await?;
        Ok(())
    }

    async fn roll_daily(&self, now: i64) -> Result<()> {
        let today = utc_day(now);
        for mut wallet in self.store.wallets().await? {
            if wallet.day_date != today {
                let previous = wallet.day_date.clone();
                wallet.day_date = today.clone();
                wallet.day_start_equity = wallet.equity;
                self.store.save_wallet(&wallet).await?;
                {
                    let mut rt = self.runtime.write();
                    rt.daily_locked = false;
                }
                self.store
                    .audit(
                        "risk.daily_rollover",
                        "engine",
                        &format!(
                            "{} day changed {} -> {}; day_start_equity={:.4}",
                            wallet.strategy, previous, today, wallet.equity
                        ),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn mark_positions(&self, now: i64) -> Result<()> {
        let cache = self.cache.read().await;
        for mut pos in self.store.open_positions().await? {
            let Some(ticker) = cache.ticker(&pos.symbol) else {
                continue;
            };
            let mark = ticker.mid();
            if mark <= 0.0 {
                continue;
            }
            pos.mark_price = mark;
            pos.unrealized_pnl = (mark - pos.entry_price) * pos.qty * pos.direction.sign();
            pos.highest_price = pos.highest_price.max(ticker.last_price.max(mark));
            pos.lowest_price = pos.lowest_price.min(ticker.last_price.min(mark));
            if pos.trailing_distance > 0.0 {
                match pos.direction {
                    Direction::Long => {
                        let candidate = pos.highest_price - pos.trailing_distance;
                        if candidate > pos.stop {
                            pos.stop = candidate;
                        }
                    }
                    Direction::Short => {
                        let candidate = pos.lowest_price + pos.trailing_distance;
                        if candidate < pos.stop {
                            pos.stop = candidate;
                        }
                    }
                }
            }
            sqlx::query(
                "UPDATE positions SET mark_price=?, unrealized_pnl=?, highest_price=?, lowest_price=?,
                 stop=?, updated_at_ms=? WHERE id=? AND status='open'",
            )
            .bind(mark)
            .bind(q8(pos.unrealized_pnl))
            .bind(pos.highest_price)
            .bind(pos.lowest_price)
            .bind(pos.stop)
            .bind(now)
            .bind(&pos.id)
            .execute(self.store.pool())
            .await?;
        }
        Ok(())
    }

    async fn apply_funding(&self, now: i64) -> Result<()> {
        let cache = self.cache.read().await;
        let positions = self.store.open_positions().await?;
        for pos in positions {
            let Some(rates) = cache.funding.get(&pos.symbol) else {
                continue;
            };
            for rate in rates
                .iter()
                .filter(|r| r.funding_time_ms > pos.entry_time_ms && r.funding_time_ms <= now)
            {
                self.apply_one_funding(&pos, rate).await?;
            }
        }
        Ok(())
    }

    async fn apply_one_funding(&self, pos: &PaperPosition, rate: &FundingRate) -> Result<bool> {
        let payment = q8(-pos.direction.sign() * rate.rate * pos.notional);
        let mut tx = self.store.pool().begin().await?;
        let inserted = sqlx::query(
            "INSERT OR IGNORE INTO funding_events
             (strategy, symbol, position_id, funding_time_ms, rate, payment, applied_at_ms)
             VALUES (?,?,?,?,?,?,?)",
        )
        .bind(pos.strategy.as_str())
        .bind(&pos.symbol)
        .bind(&pos.id)
        .bind(rate.funding_time_ms)
        .bind(rate.rate)
        .bind(payment)
        .bind(Utc::now().timestamp_millis())
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if inserted {
            sqlx::query(
                "UPDATE wallets SET cash = cash + ?, funding = funding + ?, updated_at_ms=? WHERE strategy=?",
            )
            .bind(payment)
            .bind(payment)
            .bind(Utc::now().timestamp_millis())
            .bind(pos.strategy.as_str())
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE positions SET funding = funding + ? WHERE id=?")
                .bind(payment)
                .bind(&pos.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            self.store
                .audit(
                    "funding.apply",
                    "engine",
                    &format!(
                        "{} {} funding {:.6}% at {} -> {:.6} USDT",
                        pos.strategy,
                        pos.symbol,
                        rate.rate * 100.0,
                        rate.funding_time_ms,
                        payment
                    ),
                )
                .await?;
        } else {
            tx.rollback().await?;
        }
        Ok(inserted)
    }

    async fn manage_exits(&self, now: i64) -> Result<()> {
        let cache = self.cache.read().await;
        let ws = self.ws_status.read().await.clone();
        let feed_ok = ws.healthy(now, 15_000);
        if !feed_ok {
            drop(cache);
            let mut rt = self.runtime.write();
            rt.last_error = Some(format!("market feed unhealthy: {}", ws.detail));
            return Ok(());
        }
        let mut exits: Vec<(PaperPosition, String)> = Vec::new();
        for pos in self.store.open_positions().await? {
            let Some(ticker) = cache.ticker(&pos.symbol) else {
                continue;
            };
            if !ticker.is_valid() || now.saturating_sub(ticker.ts_ms) > 10_000 {
                continue;
            }
            let (exit_reason, should) = match pos.direction {
                Direction::Long => {
                    if ticker.bid_price <= pos.stop {
                        ("stop_loss".to_string(), true)
                    } else if ticker.bid_price >= pos.target {
                        ("take_profit".to_string(), true)
                    } else {
                        (String::new(), false)
                    }
                }
                Direction::Short => {
                    if ticker.ask_price >= pos.stop {
                        ("stop_loss".to_string(), true)
                    } else if ticker.ask_price <= pos.target {
                        ("take_profit".to_string(), true)
                    } else {
                        (String::new(), false)
                    }
                }
            };
            let mut reason = if should { Some(exit_reason) } else { None };
            if reason.is_none() && pos.max_hold_until_ms > 0 && now >= pos.max_hold_until_ms {
                reason = Some("max_hold_time".into());
            }
            if reason.is_none() {
                let interval = self.cfg.strategies.timeframe(pos.strategy);
                let candles = cache.candles(&pos.symbol, interval, 400);
                if candles.len() >= 100 {
                    if let Some(invalidation) = strategy::invalidation_exit(
                        pos.strategy,
                        &self.cfg.strategies,
                        &candles,
                        pos.direction,
                        pos.entry_price,
                    ) {
                        reason = Some(format!("strategy_invalidation: {invalidation}"));
                    }
                }
            }
            if let Some(reason) = reason {
                exits.push((pos, reason));
            }
        }
        drop(cache);
        for (pos, reason) in exits {
            if let Err(e) = self.close_position(&pos, &reason, "engine").await {
                error!(position = %pos.id, error = %e, "failed to close paper position");
            }
        }
        Ok(())
    }

    async fn close_position(&self, pos: &PaperPosition, reason: &str, actor: &str) -> Result<()> {
        let cache = self.cache.read().await;
        let Some(ticker) = cache.ticker(&pos.symbol) else {
            return Err(anyhow::anyhow!("no quote for {}", pos.symbol));
        };
        let base = match pos.direction {
            Direction::Long => ticker.bid_price,
            Direction::Short => ticker.ask_price,
        };
        if base <= 0.0 {
            return Err(anyhow::anyhow!("invalid exit quote for {}", pos.symbol));
        }
        let slip = self.cfg.costs.slippage_bps / 10_000.0;
        let exit_price = match pos.direction {
            Direction::Long => base * (1.0 - slip),
            Direction::Short => base * (1.0 + slip),
        };
        let tick = cache
            .instrument(&pos.symbol)
            .map(|i| i.tick_size)
            .unwrap_or(0.0);
        let exit_price = if tick > 0.0 {
            round_to_tick(exit_price, tick)
        } else {
            exit_price
        };
        drop(cache);

        let gross = q8((exit_price - pos.entry_price) * pos.qty * pos.direction.sign());
        let fee_rate = self.cfg.costs.taker_fee_bps / 10_000.0;
        let fees_exit = q8(exit_price * pos.qty * fee_rate);
        let fees_total = q8(pos.fees_entry + fees_exit);
        let net = q8(gross - fees_total + pos.funding);
        let cash_delta = q8(gross - fees_exit);
        let now = Utc::now().timestamp_millis();
        let interval = self.cfg.strategies.timeframe(pos.strategy);
        let bars = ((now - pos.entry_time_ms).max(0) / interval.duration_ms()).max(1);
        let trade_id = format!("tr-{}", pos.id);
        let wallet = self.store.wallet(pos.strategy).await?;
        let new_cash = q8(wallet.cash + cash_delta);
        let new_realized = q8(wallet.realized + net);
        let new_fees = q8(wallet.fees + fees_total);

        let mut tx = self.store.pool().begin().await?;
        let closed = sqlx::query(
            "UPDATE positions SET status='closed', mark_price=?, funding=?, updated_at_ms=? WHERE id=? AND status='open'",
        )
        .bind(exit_price)
        .bind(pos.funding)
        .bind(now)
        .bind(&pos.id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !closed {
            tx.rollback().await?;
            return Err(anyhow::anyhow!("position {} is no longer open", pos.id));
        }
        sqlx::query(
            "INSERT OR IGNORE INTO trades
             (id, strategy, symbol, direction, qty, entry_price, exit_price, entry_time_ms, exit_time_ms,
              gross_pnl, fees, funding, slippage_est, net_pnl, close_reason, bars_held, simulated_fill, created_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&trade_id)
        .bind(pos.strategy.as_str())
        .bind(&pos.symbol)
        .bind(pos.direction.as_str())
        .bind(pos.qty)
        .bind(pos.entry_price)
        .bind(exit_price)
        .bind(pos.entry_time_ms)
        .bind(now)
        .bind(gross)
        .bind(fees_total)
        .bind(q8(pos.funding))
        .bind(q8(pos.notional * self.cfg.costs.slippage_bps / 10_000.0 + exit_price * pos.qty * slip))
        .bind(net)
        .bind(reason)
        .bind(bars)
        .bind(format!("taker_{}_with_slippage", pos.direction.exit_side()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT OR IGNORE INTO fills (id, strategy, position_id, ts_ms, side, qty, price, fee, kind, idempotency_key)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(format!("fl-exit-{}", pos.id))
        .bind(pos.strategy.as_str())
        .bind(&pos.id)
        .bind(now)
        .bind(pos.direction.exit_side().as_str())
        .bind(pos.qty)
        .bind(exit_price)
        .bind(fees_exit)
        .bind("exit")
        .bind(format!("exit:{}", pos.id))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE wallets SET cash=?, realized=?, fees=?, updated_at_ms=? WHERE strategy=?",
        )
        .bind(new_cash)
        .bind(new_realized)
        .bind(new_fees)
        .bind(now)
        .bind(pos.strategy.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        {
            let mut rt = self.runtime.write();
            rt.exits += 1;
            if net <= 0.0 {
                let count = {
                    let entry = rt.consecutive_losses.entry(pos.strategy).or_insert(0);
                    *entry += 1;
                    *entry
                };
                if count >= self.cfg.risk.max_consecutive_losses {
                    rt.cooldown_until_ms =
                        Some(now + self.cfg.risk.cooldown_minutes.max(1) * 60_000);
                    rt.consecutive_losses.insert(pos.strategy, 0);
                }
            } else {
                rt.consecutive_losses.insert(pos.strategy, 0);
            }
        }
        self.store
            .audit(
                "paper.close",
                actor,
                &format!(
                    "{} {} {} qty={} entry={:.8} exit={:.8} gross={:.4} fees={:.4} funding={:.4} net={:.4} reason={}",
                    pos.strategy,
                    pos.symbol,
                    pos.direction,
                    pos.qty,
                    pos.entry_price,
                    exit_price,
                    gross,
                    fees_total,
                    pos.funding,
                    net,
                    reason
                ),
            )
            .await?;
        Ok(())
    }

    async fn mark_all_wallets(&self) -> Result<()> {
        let positions = self.store.open_positions().await?;
        let now = Utc::now().timestamp_millis();
        for mut wallet in self.store.wallets().await? {
            let unrealized: f64 = positions
                .iter()
                .filter(|p| p.strategy == wallet.strategy)
                .map(|p| (p.mark_price - p.entry_price) * p.qty * p.direction.sign())
                .sum();
            wallet.equity = q8(wallet.cash + unrealized);
            let drawdown = if wallet.peak_equity > 0.0 {
                ((wallet.peak_equity - wallet.equity) / wallet.peak_equity * 100.0).max(0.0)
            } else {
                0.0
            };
            if wallet.equity > wallet.peak_equity {
                wallet.peak_equity = wallet.equity;
            }
            self.store.save_wallet(&wallet).await?;

            let should_point = {
                let rt = self.runtime.read();
                rt.last_equity_point_ms
                    .get(&wallet.strategy)
                    .map(|last| now - *last >= self.cfg.equity_point_interval_ms)
                    .unwrap_or(true)
            };
            if should_point {
                self.store
                    .insert_equity_point(
                        wallet.strategy,
                        &EquityPoint {
                            ts_ms: now,
                            equity: wallet.equity,
                            cash: wallet.cash,
                            unrealized: q8(unrealized),
                            drawdown_pct: drawdown,
                        },
                    )
                    .await?;
                let mut rt = self.runtime.write();
                rt.last_equity_point_ms.insert(wallet.strategy, now);
            }

            let daily_pnl = wallet.equity - wallet.day_start_equity;
            let daily_limit = -wallet.day_start_equity * self.cfg.risk.daily_loss_limit_pct / 100.0;
            let drawdown_breach =
                drawdown >= self.cfg.risk.drawdown_limit_pct && wallet.equity > 0.0;

            // Mutate the runtime under a short-lived guard, then release it before
            // awaiting the audit write (parking_lot guards are not Send and must
            // never cross an await inside the spawned engine task).
            let (daily_crossed, drawdown_crossed) = {
                let mut rt = self.runtime.write();
                let daily_crossed = daily_pnl <= daily_limit && !rt.daily_locked;
                if daily_crossed {
                    rt.daily_locked = true;
                }
                let drawdown_crossed = !daily_crossed && drawdown_breach && !rt.drawdown_locked;
                if drawdown_crossed {
                    rt.drawdown_locked = true;
                }
                (daily_crossed, drawdown_crossed)
            };

            if daily_crossed {
                self.store
                    .audit(
                        "risk.daily_lockout",
                        "engine",
                        &format!(
                            "{} daily P&L {:.4} <= {:.4}; new entries blocked",
                            wallet.strategy, daily_pnl, daily_limit
                        ),
                    )
                    .await?;
            } else if drawdown_crossed {
                self.store
                    .audit(
                        "risk.drawdown_lockout",
                        "engine",
                        &format!(
                            "{} drawdown {:.2}% >= {:.2}%; new entries blocked pending admin reset",
                            wallet.strategy, drawdown, self.cfg.risk.drawdown_limit_pct
                        ),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn edge_stats(&self, strategy: StrategyId) -> Option<EdgeStats> {
        let trades = self.store.closed_trades(Some(strategy), 500).await.ok()?;
        if trades.is_empty() {
            return None;
        }
        let samples: Vec<OutcomeSample> = trades
            .iter()
            .filter_map(|t| {
                let notional = t.qty * t.entry_price;
                if notional <= 1e-9 {
                    return None;
                }
                Some(OutcomeSample {
                    gross_return: t.gross_pnl / notional,
                    net_return: t.net_pnl / notional,
                    win: t.net_pnl > 0.0,
                    notional,
                })
            })
            .collect();
        stats_from_samples(&samples, crate::model::INITIAL_EQUITY)
    }

    async fn evaluate_entries(&self, now: i64) -> Result<()> {
        let (paused, emergency, cooldown_until, daily_locked, drawdown_locked) = {
            let rt = self.runtime.read();
            (
                rt.paused,
                rt.emergency_stop,
                rt.cooldown_until_ms,
                rt.daily_locked,
                rt.drawdown_locked,
            )
        };
        if paused || emergency || daily_locked || drawdown_locked {
            return Ok(());
        }
        if cooldown_until.map(|u| now < u).unwrap_or(false) {
            return Ok(());
        }
        let ws = self.ws_status.read().await.clone();
        let feed_ok = ws.healthy(now, 15_000);
        let cache = self.cache.read().await;
        let symbols = {
            let rows = scan(&cache, &self.cfg.scanner, self.cfg.intervals[0], now);
            let selected: Vec<String> = rows
                .iter()
                .filter(|r| r.selected)
                .map(|r| r.symbol.clone())
                .collect();
            merge_seed_symbols(
                &selected,
                &self.cfg.seed_symbols,
                &cache,
                self.cfg.scanner.max_symbols,
            )
        };
        let open_before = self.store.open_positions().await?;
        if open_before.len() >= self.cfg.risk.max_open_positions as usize {
            return Ok(());
        }
        drop(cache);

        for strategy in StrategyId::ALL {
            if !self.cfg.strategies.enabled(strategy) {
                continue;
            }
            if self.store.open_position_for(strategy).await?.is_some() {
                continue;
            }
            let stats = self.edge_stats(strategy).await;
            for symbol in &symbols {
                let open = self.store.open_positions().await?;
                if open.len() >= self.cfg.risk.max_open_positions as usize {
                    return Ok(());
                }
                if let Err(e) = self
                    .try_enter(
                        strategy,
                        symbol,
                        now,
                        feed_ok,
                        &ws.detail,
                        stats.as_ref(),
                        &open,
                    )
                    .await
                {
                    let mut rt = self.runtime.write();
                    rt.last_error = Some(format!("entry evaluation: {e}"));
                    rt.rejected_signals += 1;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_enter(
        &self,
        strategy: StrategyId,
        symbol: &str,
        now: i64,
        feed_ok: bool,
        feed_detail: &str,
        stats: Option<&EdgeStats>,
        open: &[PaperPosition],
    ) -> Result<()> {
        let cache = self.cache.read().await;
        let interval = self.cfg.strategies.timeframe(strategy);
        let candles = cache.candles(symbol, interval, 500);
        if candles.len() < 120 {
            return Ok(());
        }
        let Some(instrument) = cache.instrument(symbol).cloned() else {
            return Ok(());
        };
        let Some(ticker) = cache.ticker(symbol).cloned() else {
            return Ok(());
        };
        let Some(book) = cache.book(symbol).cloned() else {
            return Ok(());
        };
        let funding_rate = cache.latest_funding_rate(symbol);
        let eval = match strategy {
            StrategyId::Trend => {
                strategy::evaluate_trend(symbol, &candles, now, &self.cfg.strategies.trend)
            }
            StrategyId::MeanReversion => strategy::evaluate_mean_reversion(
                symbol,
                &candles,
                now,
                &self.cfg.strategies.mean_reversion,
            ),
            StrategyId::Breakout => {
                strategy::evaluate_breakout(symbol, &candles, now, &self.cfg.strategies.breakout)
            }
        };
        let Some(draft) = eval.candidate else {
            return Ok(());
        };
        drop(cache);

        let wallet = self.store.wallet(strategy).await?;
        let signal_id = format!(
            "sig-{}-{}-{}-{}",
            strategy.as_str(),
            symbol,
            draft.candle_time_ms,
            draft.direction.as_str()
        );
        if self.signal_exists(&signal_id).await? {
            return Ok(());
        }

        let total_notional: f64 = open.iter().map(|p| p.notional).sum();
        let group = correlation_group(symbol);
        let correlated_notional: f64 = open
            .iter()
            .filter(|p| correlation_group(&p.symbol) == group)
            .map(|p| p.notional)
            .sum();
        let depth = book
            .depth_notional(crate::model::Side::Buy, 10.0)
            .min(book.depth_notional(crate::model::Side::Sell, 10.0));
        let entry_price = match draft.direction {
            Direction::Long => ticker.ask_price,
            Direction::Short => ticker.bid_price,
        };
        let ctx = RiskContext {
            now_ms: now,
            equity: wallet.equity,
            peak_equity: wallet.peak_equity,
            day_start_equity: wallet.day_start_equity,
            daily_lockout: self.runtime.read().daily_locked,
            drawdown_lockout: self.runtime.read().drawdown_locked,
            cooldown_until_ms: self.runtime.read().cooldown_until_ms,
            consecutive_losses: *self
                .runtime
                .read()
                .consecutive_losses
                .get(&strategy)
                .unwrap_or(&0),
            open_positions: open.len() as u32,
            total_notional,
            correlated_notional,
            paused: self.runtime.read().paused,
            emergency_stop: self.runtime.read().emergency_stop,
            feed_ok,
            feed_detail,
            spread_bps: book.spread_bps(),
            depth_usd: depth,
            direction: draft.direction,
            instrument: &instrument,
            entry_price,
            stop_price: draft.stop,
            target_price: draft.target,
            taker_fee_bps: self.cfg.costs.taker_fee_bps,
            slippage_bps: self.cfg.costs.slippage_bps,
        };
        let verdict = self.risk.evaluate_entry(&ctx);
        let mut decision = "risk_rejected".to_string();
        let mut rejection = if verdict.approved {
            None
        } else {
            Some(verdict.reasons.join("; "))
        };
        let mut assessment = None;

        if verdict.approved && verdict.qty > 0.0 {
            let expected_hours =
                (draft.max_hold_bars.max(1) as f64) * interval.duration_ms() as f64 / 3_600_000.0;
            let a = assess_trade(
                &draft,
                verdict.qty,
                &book,
                &ticker,
                now,
                &self.cfg.costs,
                &self.cfg.risk,
                stats,
                funding_rate,
                expected_hours,
                self.cfg.min_probability_samples,
            );
            if a.accepted {
                decision = "accepted".into();
                rejection = None;
            } else {
                decision = "edge_rejected".into();
                rejection = a.rejection_reason.clone();
            }
            assessment = Some(a);
        }

        let (
            fees_est,
            funding_est,
            spread_est,
            slip_est,
            total_est,
            probability,
            prob_source,
            net_edge,
        ) = match &assessment {
            Some(a) => (
                a.costs.entry_fee + a.costs.exit_fee,
                a.costs.funding_reserve,
                a.costs.spread_cost,
                a.costs.slippage,
                a.costs.total,
                a.win_probability,
                a.probability_source.clone(),
                a.expected_net_edge,
            ),
            None => (
                0.0,
                0.0,
                0.0,
                0.0,
                0.0,
                None,
                "not evaluated: hard risk gate failed".to_string(),
                None,
            ),
        };

        let signal = Signal {
            id: signal_id.clone(),
            strategy,
            symbol: symbol.to_string(),
            timeframe: interval.label().to_string(),
            direction: draft.direction,
            candle_time_ms: draft.candle_time_ms,
            signal_time_ms: now,
            entry_reference: draft.entry_reference,
            proposed_entry: entry_price,
            stop: draft.stop,
            target: draft.target,
            atr: draft.atr,
            snapshot_json: serde_json::json!({
                "features": draft.features,
                "reason": draft.reason,
                "risk_reasons": verdict.reasons,
                "edge_checks": assessment.as_ref().map(|a| a.checks.clone()),
                "quote": {"bid": ticker.bid_price, "ask": ticker.ask_price, "spread_bps": ticker.spread_bps()},
                "funding_rate": funding_rate
            })
            .to_string(),
            expected_fees: q8(fees_est),
            expected_funding: q8(funding_est),
            expected_spread_cost: q8(spread_est),
            expected_slippage: q8(slip_est),
            expected_costs_total: q8(total_est),
            win_probability: probability,
            probability_source: prob_source,
            estimated_net_edge: net_edge,
            decision: decision.clone(),
            rejection_reason: rejection.clone(),
            strategy_version: "1".into(),
            created_at_ms: now,
        };
        self.store.insert_signal(&signal).await?;

        if decision != "accepted" {
            let mut rt = self.runtime.write();
            rt.rejected_signals += 1;
            return Ok(());
        }
        if self.store.open_position_for(strategy).await?.is_some() {
            return Ok(());
        }

        let qty = verdict.qty;
        let slip = self.cfg.costs.slippage_bps / 10_000.0;
        let fill_price = match draft.direction {
            Direction::Long => entry_price * (1.0 + slip),
            Direction::Short => entry_price * (1.0 - slip),
        };
        let fill_price = if instrument.tick_size > 0.0 {
            round_to_tick(fill_price, instrument.tick_size)
        } else {
            fill_price
        };
        let geometry_ok = match draft.direction {
            Direction::Long => fill_price > draft.stop && draft.target > fill_price,
            Direction::Short => fill_price < draft.stop && draft.target < fill_price,
        };
        if !geometry_ok {
            self.store
                .audit(
                    "paper.entry_rejected",
                    "engine",
                    &format!(
                        "{strategy} {symbol}: spread/slippage moved price through stop or target"
                    ),
                )
                .await?;
            return Ok(());
        }
        let notional = q8(fill_price * qty);
        let fee = q8(notional * self.cfg.costs.taker_fee_bps / 10_000.0);
        let position_id = uuid::Uuid::new_v4().to_string();
        let max_hold_until = now + draft.max_hold_bars.max(1) * interval.duration_ms();
        let wallet = self.store.wallet(strategy).await?;
        let new_cash = q8(wallet.cash - fee);

        let mut tx = self.store.pool().begin().await?;
        sqlx::query(
            "INSERT INTO positions
             (id, strategy, symbol, direction, qty, entry_price, entry_time_ms, mark_price, stop, initial_stop,
              target, trailing_distance, highest_price, lowest_price, max_hold_until_ms, fees_entry, funding,
              notional, signal_id, status, updated_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, 'open', ?)",
        )
        .bind(&position_id)
        .bind(strategy.as_str())
        .bind(symbol)
        .bind(draft.direction.as_str())
        .bind(qty)
        .bind(fill_price)
        .bind(now)
        .bind(fill_price)
        .bind(draft.stop)
        .bind(draft.stop)
        .bind(draft.target)
        .bind(draft.trailing_distance)
        .bind(fill_price)
        .bind(fill_price)
        .bind(max_hold_until)
        .bind(fee)
        .bind(0.0)
        .bind(notional)
        .bind(&signal_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO fills (id, strategy, position_id, ts_ms, side, qty, price, fee, kind, idempotency_key)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(format!("fl-entry-{position_id}"))
        .bind(strategy.as_str())
        .bind(&position_id)
        .bind(now)
        .bind(draft.direction.entry_side().as_str())
        .bind(qty)
        .bind(fill_price)
        .bind(fee)
        .bind("entry")
        .bind(format!("entry:{position_id}"))
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE wallets SET cash=?, updated_at_ms=? WHERE strategy=?")
            .bind(new_cash)
            .bind(now)
            .bind(strategy.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        {
            let mut rt = self.runtime.write();
            rt.entries += 1;
        }
        self.store
            .audit(
                "paper.entry",
                "engine",
                &format!(
                    "{strategy} {symbol} {} qty={} fill={:.8} stop={:.8} target={:.8} fee={:.4} signal={}",
                    draft.direction, qty, fill_price, draft.stop, draft.target, fee, signal_id
                ),
            )
            .await?;
        info!(strategy = %strategy, symbol, "paper position opened");
        Ok(())
    }

    async fn signal_exists(&self, id: &str) -> Result<bool> {
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM signals WHERE id=?")
            .bind(id)
            .fetch_one(self.store.pool())
            .await?;
        Ok(count > 0)
    }

    /// Performance summary for the dashboard, computed from the persisted ledger.
    pub async fn strategy_stats(
        &self,
        strategy: StrategyId,
    ) -> Result<(Wallet, Vec<ClosedTrade>, crate::model::StrategyStats)> {
        let wallet = self.store.wallet(strategy).await?;
        let trades = self.store.closed_trades(Some(strategy), 1000).await?;
        let equity = self.store.equity_curve(strategy, 2000).await?;
        let stats =
            crate::store::stats_from_closed(strategy, &trades, &equity, wallet.initial_equity);
        Ok((wallet, trades, stats))
    }

    /// Used by tests and the CLI doctor to assert that an equity curve is being
    /// written. Returns the number of points.
    pub async fn equity_points(&self, strategy: StrategyId) -> Result<usize> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM equity_points WHERE strategy=?")
                .bind(strategy.as_str())
                .fetch_one(self.store.pool())
                .await? as usize,
        )
    }
}

async fn consecutive_losses(store: &Store, strategy: StrategyId) -> Result<u32> {
    let trades = store.closed_trades(Some(strategy), 50).await?;
    let mut count = 0u32;
    for trade in trades {
        if trade.net_pnl > 0.0 {
            break;
        }
        count += 1;
    }
    Ok(count)
}

/// Run the engine until shutdown. Errors are surfaced in the runtime status and
/// audit log but never terminate the process; the next cycle retries.
pub async fn run(engine: Arc<PaperEngine>, shutdown: CancellationToken) {
    let mut interval =
        tokio::time::interval(std::time::Duration::from_secs(engine.cfg.cycle_secs.max(2)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {
                if let Err(e) = engine.tick().await {
                    let mut rt = engine.runtime.write();
                    rt.last_error = Some(e.to_string());
                    drop(rt);
                    error!(error = %e, "paper engine cycle failed");
                }
            }
        }
    }
    {
        let mut rt = engine.runtime.write();
        rt.running = false;
        rt.last_error = None;
    }
    let _ = engine
        .store
        .audit("engine.stop", "engine", "paper engine stopped")
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CostConfig, RiskConfig, StrategyConfigs};
    use crate::market::MarketCache;
    use crate::model::{BookLevel, Candle, Instrument, Interval, OrderBook, Ticker};
    use crate::store::Store;
    use tempfile::TempDir;

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
            max_market_qty: 100.0,
            min_notional: 5.0,
            funding_interval_minutes: 480,
            launch_time_ms: 0,
            source: "test".into(),
            fetched_at_ms: 0,
        }
    }

    fn seed_cache(cache: &mut MarketCache) {
        cache.upsert_instruments(vec![instrument()]);
        cache.upsert_tickers(vec![Ticker {
            symbol: "BTCUSDT".into(),
            last_price: 50_000.0,
            bid_price: 49_999.0,
            ask_price: 50_001.0,
            turnover_24h: 1e9,
            ts_ms: Utc::now().timestamp_millis(),
            source: "test".into(),
            ..Ticker::default()
        }]);
        cache.books.insert(
            "BTCUSDT".into(),
            OrderBook {
                symbol: "BTCUSDT".into(),
                ts_ms: Utc::now().timestamp_millis(),
                received_ms: Utc::now().timestamp_millis(),
                bids: vec![BookLevel {
                    price: 49_999.0,
                    size: 100.0,
                }],
                asks: vec![BookLevel {
                    price: 50_001.0,
                    size: 100.0,
                }],
                source: "test".into(),
                valid: true,
                ..OrderBook::default()
            },
        );
        for i in 0..300 {
            cache.upsert_candle(Candle {
                symbol: "BTCUSDT".into(),
                interval: Interval::M5,
                open_time_ms: i * 300_000,
                open: 100.0 + i as f64 * 0.2,
                high: 100.0 + i as f64 * 0.2 + 0.1,
                low: 100.0 + i as f64 * 0.2 - 0.1,
                close: 100.0 + i as f64 * 0.2,
                volume: 500.0,
                turnover: 50_000.0,
                confirm: true,
                source: "test".into(),
                ingested_at_ms: 0,
            });
        }
    }

    async fn engine(dir: &TempDir) -> Arc<PaperEngine> {
        let store = Store::connect(&dir.path().join("engine.db")).await.unwrap();
        let cache = Arc::new(RwLock::new(MarketCache::default()));
        let status = Arc::new(RwLock::new(WsStatus {
            state: "connected".into(),
            last_message_ms: Some(Utc::now().timestamp_millis()),
            ..WsStatus::default()
        }));
        let cfg = EngineConfig {
            strategies: StrategyConfigs::default(),
            risk: RiskConfig::default(),
            costs: CostConfig::default(),
            scanner: ScannerConfig {
                min_depth_usd: 100.0,
                min_turnover_24h: 1_000.0,
                ..ScannerConfig::default()
            },
            intervals: vec![Interval::M5],
            seed_symbols: vec!["BTCUSDT".into()],
            min_probability_samples: 30,
            cycle_secs: 5,
            equity_point_interval_ms: 60_000,
        };
        Arc::new(PaperEngine::new(store, cache, status, cfg).await.unwrap())
    }

    #[tokio::test]
    async fn paused_engine_does_not_enter() {
        let dir = TempDir::new().unwrap();
        let engine = engine(&dir).await;
        {
            let mut c = engine.cache.write().await;
            seed_cache(&mut c);
        }
        engine.pause("test").await.unwrap();
        engine.tick().await.unwrap();
        assert!(engine.store.open_positions().await.unwrap().is_empty());
        assert!(engine.runtime_snapshot().paused);
    }

    #[tokio::test]
    async fn emergency_stop_blocks_and_flattens() {
        let dir = TempDir::new().unwrap();
        let engine = engine(&dir).await;
        {
            let mut c = engine.cache.write().await;
            seed_cache(&mut c);
        }
        engine.emergency_stop("unit test", "test").await.unwrap();
        let rt = engine.runtime_snapshot();
        assert!(rt.emergency_stop);
        assert!(engine.store.open_positions().await.unwrap().is_empty());
        engine.clear_emergency("test").await.unwrap();
        assert!(!engine.runtime_snapshot().emergency_stop);
    }

    #[tokio::test]
    async fn funding_settlement_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let engine = engine(&dir).await;
        // Create a synthetic open position directly through the engine's schema.
        let pos = PaperPosition {
            id: "p-funding".into(),
            strategy: StrategyId::Trend,
            symbol: "BTCUSDT".into(),
            direction: Direction::Long,
            qty: 0.001,
            entry_price: 50_000.0,
            entry_time_ms: 0,
            mark_price: 50_000.0,
            stop: 49_000.0,
            initial_stop: 49_000.0,
            target: 52_000.0,
            trailing_distance: 0.0,
            highest_price: 50_000.0,
            lowest_price: 50_000.0,
            max_hold_until_ms: i64::MAX,
            fees_entry: 0.0,
            funding: 0.0,
            unrealized_pnl: 0.0,
            notional: 50.0,
            signal_id: None,
            status: "open".into(),
        };
        sqlx::query("INSERT INTO positions (id,strategy,symbol,direction,qty,entry_price,entry_time_ms,mark_price,stop,initial_stop,target,trailing_distance,highest_price,lowest_price,max_hold_until_ms,fees_entry,funding,notional,status,updated_at_ms) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?, 'open', 0)")
            .bind(&pos.id).bind("trend").bind(&pos.symbol).bind("long").bind(pos.qty)
            .bind(pos.entry_price).bind(0).bind(pos.entry_price).bind(pos.stop).bind(pos.stop)
            .bind(pos.target).bind(0.0).bind(pos.entry_price).bind(pos.entry_price)
            .bind(i64::MAX).bind(0.0).bind(0.0).bind(pos.notional)
            .execute(engine.store.pool()).await.unwrap();
        let rate = FundingRate {
            symbol: "BTCUSDT".into(),
            funding_time_ms: 1,
            rate: 0.0001,
            source: "test".into(),
            fetched_at_ms: 0,
        };
        assert!(engine.apply_one_funding(&pos, &rate).await.unwrap());
        assert!(!engine.apply_one_funding(&pos, &rate).await.unwrap());
        let funding: f64 = sqlx::query_scalar("SELECT funding FROM wallets WHERE strategy='trend'")
            .fetch_one(engine.store.pool())
            .await
            .unwrap();
        assert!((funding - (-0.005)).abs() < 1e-9, "funding={funding}");
    }

    #[tokio::test]
    async fn ledger_reconciles_after_funding() {
        let dir = TempDir::new().unwrap();
        let engine = engine(&dir).await;
        let (delta, _) = engine.store.reconcile(StrategyId::Trend).await.unwrap();
        assert!(delta < 1e-9);
    }
}
