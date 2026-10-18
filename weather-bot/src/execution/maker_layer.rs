//! Shared maker-first order layer used by BOTH strategies.
//!
//! Rules enforced here:
//!  1. every order is posted post-only GTC first (OrderBuilder::maker);
//!  2. after the maker timeout the bot may cross the spread ONLY if --allow-taker was given and the
//!     strategy's taker rule still holds; every taker override is logged with its reason;
//!  3. maker volume and the estimated rebate accrue to SQLite on every maker fill;
//!  4. fee-aware taker edge (fee read from the market's feeSchedule);
//!  5. quotes are cancelled (by id) and repriced when fair value moves > threshold or every
//!     `reprice_secs`; no quote outlives `max_quote_age_secs`.
use super::order_builder::{OrderBuilder, OrderRequest};
use super::{Broker, OrderState};
use crate::config::ExecCfg;
use crate::polymarket::clob::ClobClient;
use crate::polymarket::gamma::FeeSchedule;
use crate::risk::exposure::{Exposure, PositionRec};
use crate::risk::kill_switch::Halt;
use crate::storage::sqlite::Storage;
use crate::util::lock;
use crate::util::math::floor_to;
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteMode {
    /// Forecast edge: improve the best bid by one tick (never crossing the ask).
    ImproveBid,
    /// NO harvester: ask - 1 tick.
    AskMinusTick,
}

#[derive(Debug, Clone)]
pub enum TakerRule {
    /// taker_edge = fair - ask - fee(ask) must exceed this (needs a known fee schedule).
    MinEdge(Decimal),
    /// ask must still sit inside [min, max].
    MinAsk { min: Decimal, max: Decimal },
}

pub type FairFn = Arc<dyn Fn() -> Option<Decimal> + Send + Sync>;

pub struct TradeIntent {
    pub signal_id: i64,
    pub strategy: &'static str,
    pub market_id: String,
    pub outcome_idx: usize,
    pub city: String,
    pub event_key: String,
    pub bracket: String,
    pub token_id: String,
    pub outcome: &'static str,
    pub neg_risk: bool,
    pub tick: Decimal,
    pub fee: Option<FeeSchedule>,
    pub size: Decimal,
    pub fair: Decimal,
    /// Re-evaluated every cycle so a stale model cannot keep a quote alive.
    pub fair_fn: Option<FairFn>,
    /// Maker price ceiling = min(fair - edge_floor, hard_cap).
    pub edge_floor: Decimal,
    pub hard_cap: Decimal,
    pub mode: QuoteMode,
    pub taker: TakerRule,
    pub maker_timeout: Duration,
    pub max_spread: Decimal,
    pub reservation: u64,
}

#[derive(Debug, Clone)]
pub enum ExecOutcome {
    Filled { size: Decimal, avg_price: Decimal },
    Partial { size: Decimal, avg_price: Decimal },
    Unfilled(String),
    Halted { size: Decimal },
    Failed(String),
}

/// Maker quote price. None if no valid post-only price exists under the ceiling.
pub fn maker_price(mode: QuoteMode, bid: Option<Decimal>, ask: Decimal, ceiling: Decimal, tick: Decimal) -> Option<Decimal> {
    let raw = match mode {
        QuoteMode::ImproveBid => {
            let b = bid?;
            (b + tick).min(ask - tick)
        }
        QuoteMode::AskMinusTick => ask - tick,
    };
    let p = floor_to(raw.min(ceiling), tick);
    if p <= Decimal::ZERO || p >= ask {
        None
    } else {
        Some(p)
    }
}

/// Fee-aware taker gate (Rule 4).
pub fn taker_allowed(rule: &TakerRule, fair: Decimal, ask: Decimal, fee: Option<&FeeSchedule>) -> bool {
    match rule {
        TakerRule::MinEdge(min) => match fee {
            Some(f) => fair - ask - f.fee_per_share(ask) > *min,
            None => false, // unknown fee => never cross
        },
        TakerRule::MinAsk { min, max } => ask >= *min && ask <= *max,
    }
}

/// Cancel/replace cadence (Rule 5).
pub fn should_reprice(
    has_live_quote: bool,
    fair_at_placement: Decimal,
    fair_now: Decimal,
    quote_age: Duration,
    threshold: Decimal,
    max_age: Duration,
) -> bool {
    !has_live_quote || (fair_now - fair_at_placement).abs() > threshold || quote_age >= max_age
}

struct LiveOrder {
    exch_id: String,
    db_id: i64,
    price: Decimal,
    placed: Instant,
    fair_at_place: Decimal,
    filled_seen: Decimal,
    fee: Decimal,
    is_maker: bool,
}

struct Acc {
    total: Decimal,
    cost: Decimal,
}

pub struct MakerLayer {
    account: &'static str,
    broker: Arc<dyn Broker>,
    clob: Arc<ClobClient>,
    storage: Arc<Storage>,
    exposure: Arc<Mutex<Exposure>>,
    halt: Arc<Halt>,
    builder: OrderBuilder,
    cfg: ExecCfg,
    open: Mutex<HashSet<String>>,
}

impl MakerLayer {
    pub fn new(
        account: &'static str,
        broker: Arc<dyn Broker>,
        clob: Arc<ClobClient>,
        storage: Arc<Storage>,
        exposure: Arc<Mutex<Exposure>>,
        halt: Arc<Halt>,
        allow_taker: bool,
        cfg: ExecCfg,
    ) -> MakerLayer {
        MakerLayer {
            account,
            broker,
            clob,
            storage,
            exposure,
            halt,
            builder: OrderBuilder::new(allow_taker),
            cfg,
            open: Mutex::new(HashSet::new()),
        }
    }

    fn log(&self, level: &str, kind: &str, msg: &str) {
        if level == "WARN" {
            tracing::warn!("[{}] {kind}: {msg}", self.account);
        } else {
            tracing::info!("[{}] {kind}: {msg}", self.account);
        }
        let _ = self.storage.log_event(level, kind, &format!("[{}] {msg}", self.account));
    }

    /// Cancel every working order, by id.
    pub async fn cancel_all(&self) {
        let ids: Vec<String> = lock(&self.open).iter().cloned().collect();
        for id in ids {
            match self.broker.cancel(&id).await {
                Ok(()) => {
                    lock(&self.open).remove(&id);
                }
                Err(e) => self.log("WARN", "CANCEL_FAILED", &format!("{id}: {e}")),
            }
        }
        if !lock(&self.open).is_empty() {
            self.log("WARN", "ORPHAN_ORDERS", "some orders could not be cancelled; will retry");
        }
    }

    fn record_fill(&self, it: &TradeIntent, o: &LiveOrder, delta: Decimal, price: Decimal, is_maker: bool) -> Decimal {
        let fee = if is_maker { Decimal::ZERO } else { it.fee.as_ref().map(|f| f.fee(price, delta)).unwrap_or(Decimal::ZERO) };
        let _ = self.storage.insert_fill(o.db_id, price, delta, is_maker);
        let rec = PositionRec {
            market_id: it.market_id.clone(),
            token_id: it.token_id.clone(),
            outcome_idx: it.outcome_idx,
            city: it.city.clone(),
            event_key: it.event_key.clone(),
            bracket: it.bracket.clone(),
            outcome: it.outcome.to_string(),
            size: delta,
            avg_price: price,
        };
        let merged = lock(&self.exposure).add_fill(&rec, price * delta + fee);
        let _ = self.storage.upsert_position(&merged);
        if is_maker {
            // Rule 3: maker volume + estimated rebate accrual.
            let accrued = it.fee.as_ref().map(|f| f.maker_rebate(price, delta)).unwrap_or(Decimal::ZERO);
            let _ = self.storage.add_rebate(price * delta, accrued);
        }
        fee
    }

    async fn sync_order(&self, it: &TradeIntent, o: &mut LiveOrder, acc: &mut Acc) -> crate::error::Result<OrderState> {
        let st = self.broker.status(&o.exch_id).await?;
        if st.filled > o.filled_seen {
            let delta = st.filled - o.filled_seen;
            let price = if st.avg_price > Decimal::ZERO { st.avg_price } else { o.price };
            let is_maker = o.is_maker;
            let fee = self.record_fill(it, &*o, delta, price, is_maker);
            o.fee += fee;
            o.filled_seen = st.filled;
            acc.total += delta;
            acc.cost += delta * price;
        }
        let label = match &st.state {
            OrderState::Open => "open",
            OrderState::Filled => "filled",
            OrderState::Cancelled => "cancelled",
            OrderState::Rejected(_) => "rejected",
        };
        let _ = self.storage.update_order(o.db_id, label, o.filled_seen, o.fee);
        Ok(st.state)
    }

    /// Cancel by id, then pull the final fill state. Returns true once the order is definitely gone.
    async fn cancel_sync(&self, it: &TradeIntent, o: &mut LiveOrder, acc: &mut Acc) -> bool {
        if let Err(e) = self.broker.cancel(&o.exch_id).await {
            self.log("WARN", "CANCEL_FAILED", &format!("{}: {e}", o.exch_id));
        }
        match self.sync_order(it, o, acc).await {
            Ok(OrderState::Open) | Err(_) => false,
            Ok(_) => {
                lock(&self.open).remove(&o.exch_id);
                true
            }
        }
    }

    fn register(&self, it: &TradeIntent, req: &OrderRequest, exch_id: &str, status: &str, fair: Decimal) -> LiveOrder {
        let db_id = self
            .storage
            .insert_order(it.signal_id, exch_id, &format!("BUY {}", it.outcome), req.price, req.size, req.post_only, status)
            .unwrap_or(0);
        LiveOrder {
            exch_id: exch_id.to_string(),
            db_id,
            price: req.price,
            placed: Instant::now(),
            fair_at_place: fair,
            filled_seen: Decimal::ZERO,
            fee: Decimal::ZERO,
            is_maker: req.post_only,
        }
    }

    async fn place_maker(&self, it: &TradeIntent, price: Decimal, size: Decimal, fair: Decimal) -> Option<LiveOrder> {
        let req = self.builder.maker(&it.token_id, price, size, it.neg_risk, it.tick);
        if req.size <= Decimal::ZERO {
            return None;
        }
        match self.broker.place(&req).await {
            Ok(ack) => match ack.state {
                OrderState::Rejected(why) => {
                    self.log("WARN", "ORDER_REJECTED", &format!("{} {} @ {}: {why}", it.city, it.bracket, req.price));
                    None
                }
                _ => {
                    lock(&self.open).insert(ack.id.clone());
                    Some(self.register(it, &req, &ack.id, "open", fair))
                }
            },
            Err(e) => {
                self.log("WARN", "ORDER_ERROR", &format!("{} {}: {e}", it.city, it.bracket));
                None
            }
        }
    }

    pub async fn execute(&self, it: TradeIntent) -> ExecOutcome {
        let started = Instant::now();
        let poll = Duration::from_secs(self.cfg.poll_secs.max(1));
        // Rule 5: a quote may never outlive max_quote_age, so repricing must happen well inside it.
        let reprice_after = Duration::from_secs(
            self.cfg.reprice_secs.min(self.cfg.max_quote_age_secs.saturating_sub(self.cfg.poll_secs).max(1)),
        );
        let mut live: Option<LiveOrder> = None;
        let mut acc = Acc { total: Decimal::ZERO, cost: Decimal::ZERO };
        let mut fair = it.fair;
        let mut book_failures = 0u32;

        let outcome = loop {
            if self.halt.is_halted() {
                if let Some(mut o) = live.take() {
                    self.cancel_sync(&it, &mut o, &mut acc).await;
                }
                break ExecOutcome::Halted { size: acc.total };
            }
            if let Some(f) = it.fair_fn.as_ref().and_then(|f| f()) {
                fair = f;
            }
            if let Some(o) = live.as_mut() {
                match self.sync_order(&it, o, &mut acc).await {
                    Ok(OrderState::Open) => {}
                    Ok(_) => live = None,
                    Err(e) => self.log("WARN", "STATUS_ERROR", &format!("{}: {e}", it.token_id)),
                }
            }
            if acc.total >= it.size {
                break ExecOutcome::Filled { size: acc.total, avg_price: avg(&acc) };
            }
            let remaining = it.size - acc.total;

            // ---- time limit: maker route has failed; maybe cross the spread (Rule 2) ----
            if started.elapsed() >= it.maker_timeout {
                if let Some(mut o) = live.take() {
                    self.cancel_sync(&it, &mut o, &mut acc).await;
                }
                if acc.total >= it.size {
                    break ExecOutcome::Filled { size: acc.total, avg_price: avg(&acc) };
                }
                let remaining = it.size - acc.total;
                if self.builder.allow_taker() {
                    if let Ok(book) = self.clob.book(&it.token_id).await {
                        if let Some(ask) = book.best_ask() {
                            if taker_allowed(&it.taker, fair, ask.price, it.fee.as_ref()) {
                                if let Ok(req) = self.builder.taker(&it.token_id, ask.price, remaining, it.neg_risk, it.tick) {
                                    let fee_ps = it.fee.as_ref().map(|f| f.fee_per_share(ask.price)).unwrap_or(Decimal::ZERO);
                                    self.log(
                                        "INFO",
                                        "TAKER_OVERRIDE",
                                        &format!(
                                            "{} {} {} @ {}: maker unfilled after {}s; fair {:.3}, fee/share {:.4}, rule {:?}",
                                            it.city, it.bracket, it.outcome, ask.price, it.maker_timeout.as_secs(), fair, fee_ps, it.taker
                                        ),
                                    );
                                    match self.broker.place(&req).await {
                                        Ok(ack) if !matches!(ack.state, OrderState::Rejected(_)) => {
                                            let mut o = self.register(&it, &req, &ack.id, "taker", fair);
                                            if ack.filled > Decimal::ZERO {
                                                let price = if ack.avg_price > Decimal::ZERO { ack.avg_price } else { ask.price };
                                                let fee = self.record_fill(&it, &o, ack.filled, price, false);
                                                o.fee += fee;
                                                o.filled_seen = ack.filled;
                                                acc.total += ack.filled;
                                                acc.cost += ack.filled * price;
                                                let _ = self.storage.update_order(o.db_id, "filled", o.filled_seen, o.fee);
                                            }
                                        }
                                        Ok(ack) => self.log("WARN", "TAKER_REJECTED", &format!("{:?}", ack.state)),
                                        Err(e) => self.log("WARN", "TAKER_ERROR", &e.to_string()),
                                    }
                                }
                            }
                        }
                    }
                }
                break if acc.total >= it.size {
                    ExecOutcome::Filled { size: acc.total, avg_price: avg(&acc) }
                } else if acc.total > Decimal::ZERO {
                    ExecOutcome::Partial { size: acc.total, avg_price: avg(&acc) }
                } else {
                    ExecOutcome::Unfilled("maker timeout".into())
                };
            }

            // ---- order-book protection (Section 8) ----
            let book = match self.clob.book(&it.token_id).await {
                Ok(b) => {
                    book_failures = 0;
                    b
                }
                Err(e) => {
                    book_failures += 1;
                    self.log("WARN", "BOOK_ERROR", &e.to_string());
                    if book_failures >= 3 {
                        if let Some(mut o) = live.take() {
                            self.cancel_sync(&it, &mut o, &mut acc).await;
                        }
                        break ExecOutcome::Failed("order book unavailable".into());
                    }
                    tokio::time::sleep(poll).await;
                    continue;
                }
            };
            if let Some(mid) = book.mid() {
                lock(&self.exposure).mark(&it.token_id, mid);
            }
            let ceiling = (fair - it.edge_floor).min(it.hard_cap);
            let spread_ok = book.spread().map_or(false, |s| s <= it.max_spread);
            let depth_ok = book.ask_depth_up_to(ceiling) >= remaining * self.cfg.depth_multiple;
            let quote = if spread_ok && depth_ok {
                book.best_ask().and_then(|a| {
                    maker_price(it.mode, book.best_bid().map(|b| b.price), a.price, ceiling, it.tick)
                })
            } else {
                None
            };
            let Some(price) = quote else {
                // No safe quote right now: pull any resting order rather than leave it stale.
                if let Some(mut o) = live.take() {
                    if !self.cancel_sync(&it, &mut o, &mut acc).await {
                        live = Some(o);
                    }
                }
                tokio::time::sleep(poll).await;
                continue;
            };

            let (age, fair_at) = match &live {
                Some(o) => (o.placed.elapsed(), o.fair_at_place),
                None => (Duration::ZERO, fair),
            };
            if should_reprice(live.is_some(), fair_at, fair, age, self.cfg.reprice_threshold, reprice_after) {
                if let Some(mut o) = live.take() {
                    if !self.cancel_sync(&it, &mut o, &mut acc).await {
                        // Could not confirm the cancel: keep tracking it, never stack a second quote.
                        live = Some(o);
                        tokio::time::sleep(poll).await;
                        continue;
                    }
                    if acc.total >= it.size {
                        break ExecOutcome::Filled { size: acc.total, avg_price: avg(&acc) };
                    }
                }
                let size_left = it.size - acc.total;
                live = self.place_maker(&it, price, size_left, fair).await;
            }
            tokio::time::sleep(poll).await;
        };

        if let Some(mut o) = live.take() {
            if !self.cancel_sync(&it, &mut o, &mut acc).await {
                self.log("WARN", "ORPHAN_ORDER", &format!("{} could not be confirmed cancelled", o.exch_id));
            }
        }
        lock(&self.exposure).release(it.reservation);
        outcome
    }
}

fn avg(acc: &Acc) -> Decimal {
    if acc.total > Decimal::ZERO {
        acc.cost / acc.total
    } else {
        Decimal::ZERO
    }
}
