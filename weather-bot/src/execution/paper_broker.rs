use super::order_builder::OrderRequest;
use super::{Broker, OrderAck, OrderState, OrderStatus};
use crate::error::Result;
use crate::polymarket::clob::ClobClient;
use crate::polymarket::trades::{Print, TradesClient};
use crate::util::lock;
use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct PaperOrder {
    req: OrderRequest,
    filled: Decimal,
    cancelled: bool,
    placed_at: i64,
    /// Prints already counted for this order (the tape is polled repeatedly).
    seen: HashSet<String>,
}

/// Paper broker against REAL books and the public trade tape.
/// - post-only orders that would cross are rejected, exactly like the exchange;
/// - a resting buy fills when a **SELL** print at or below its price appears after placement,
///   capped by the printed size (this is what actually fills a maker bid; an unchanged ask does not
///   mean the bid was not hit);
/// - taker (FAK) orders walk the visible asks and are filled at the actual ask prices.
pub struct PaperBroker {
    clob: Arc<ClobClient>,
    trades: Arc<TradesClient>,
    orders: Mutex<HashMap<String, PaperOrder>>,
    seq: AtomicU64,
}

impl PaperBroker {
    pub fn new(clob: Arc<ClobClient>, trades: Arc<TradesClient>) -> Self {
        PaperBroker { clob, trades, orders: Mutex::new(HashMap::new()), seq: AtomicU64::new(1) }
    }
}

/// Pure fill calculation, kept separate so it can be unit-tested without a network.
///
/// `seen` holds the prints already counted. Only SELL prints (taker sold into the book) at or below
/// our bid, timestamped at or after placement, can hit a resting buy. We do not add a clock-skew
/// tolerance here: a print from before the quote was placed must not fill it, and re-quotes are common.
pub fn maker_fill_from_prints(
    req: &OrderRequest,
    placed_at: i64,
    remaining: Decimal,
    prints: &[Print],
    seen: &mut HashSet<String>,
) -> Decimal {
    let mut take = Decimal::ZERO;
    if remaining <= Decimal::ZERO {
        return take;
    }
    for p in prints {
        if take >= remaining {
            break;
        }
        if p.asset != req.token_id || !p.side.eq_ignore_ascii_case("SELL") {
            continue;
        }
        if p.price > req.price || p.timestamp < placed_at {
            continue;
        }
        let key = format!("{}|{}|{}|{}|{}", p.tx, p.timestamp, p.price, p.size, p.wallet);
        if !seen.insert(key) {
            continue;
        }
        take += p.size.min(remaining - take);
    }
    take
}

#[async_trait]
impl Broker for PaperBroker {
    async fn place(&self, req: &OrderRequest) -> Result<OrderAck> {
        let book = self.clob.book(&req.token_id).await?;
        let id = format!("paper-{}", self.seq.fetch_add(1, Ordering::SeqCst));
        if req.post_only {
            if let Some(ask) = book.best_ask() {
                if req.price >= ask.price {
                    return Ok(OrderAck {
                        id,
                        state: OrderState::Rejected("post-only order would cross the book".into()),
                        filled: Decimal::ZERO,
                        avg_price: Decimal::ZERO,
                    });
                }
            }
            lock(&self.orders).insert(
                id.clone(),
                PaperOrder {
                    req: req.clone(),
                    filled: Decimal::ZERO,
                    cancelled: false,
                    placed_at: Utc::now().timestamp(),
                    seen: HashSet::new(),
                },
            );
            return Ok(OrderAck { id, state: OrderState::Open, filled: Decimal::ZERO, avg_price: Decimal::ZERO });
        }
        // Taker: walk the asks up to our limit price.
        let mut remaining = req.size;
        let mut filled = Decimal::ZERO;
        let mut notional = Decimal::ZERO;
        for lvl in book.asks.iter().filter(|l| l.price <= req.price) {
            if remaining <= Decimal::ZERO {
                break;
            }
            let take = remaining.min(lvl.size);
            filled += take;
            notional += take * lvl.price;
            remaining -= take;
        }
        let avg = if filled > Decimal::ZERO { notional / filled } else { Decimal::ZERO };
        let state = if filled >= req.size { OrderState::Filled } else { OrderState::Cancelled };
        Ok(OrderAck { id, state, filled, avg_price: avg })
    }

    async fn cancel(&self, order_id: &str) -> Result<()> {
        if let Some(o) = lock(&self.orders).get_mut(order_id) {
            o.cancelled = true;
        }
        Ok(())
    }

    async fn status(&self, order_id: &str) -> Result<OrderStatus> {
        let (req, mut filled, cancelled, placed_at, seen) = {
            let g = lock(&self.orders);
            match g.get(order_id) {
                Some(o) => (o.req.clone(), o.filled, o.cancelled, o.placed_at, o.seen.clone()),
                None => {
                    return Ok(OrderStatus { state: OrderState::Cancelled, filled: Decimal::ZERO, avg_price: Decimal::ZERO })
                }
            }
        };
        if filled >= req.size {
            return Ok(OrderStatus { state: OrderState::Filled, filled, avg_price: req.price });
        }
        // Even for a cancelled order, do one final tape check: a sell print may have hit the bid in the
        // seconds between the last poll and the cancel. Missing it would understate paper fills.
        let prints = match self.trades.recent(&req.condition_id).await {
            Ok(p) => p,
            Err(e) => {
                // Paper mode must not stall or fail the order loop because the tape is unreachable.
                tracing::warn!("paper trade tape {}: {e}", req.condition_id);
                let state = if cancelled { OrderState::Cancelled } else { OrderState::Open };
                return Ok(OrderStatus { state, filled, avg_price: req.price });
            }
        };
        let mut seen_next = seen;
        let take = maker_fill_from_prints(&req, placed_at, req.size - filled, &prints, &mut seen_next);
        if take > Decimal::ZERO {
            let mut g = lock(&self.orders);
            if let Some(o) = g.get_mut(order_id) {
                o.seen = seen_next;
                o.filled = (o.filled + take).min(o.req.size);
                filled = o.filled;
            }
        }
        let state = if filled >= req.size {
            OrderState::Filled
        } else if cancelled {
            OrderState::Cancelled
        } else {
            OrderState::Open
        };
        Ok(OrderStatus { state, filled, avg_price: req.price })
    }

    fn name(&self) -> &'static str {
        "paper"
    }
}
