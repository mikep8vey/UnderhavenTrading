use super::order_builder::OrderRequest;
use super::{Broker, OrderAck, OrderState, OrderStatus};
use crate::error::Result;
use crate::polymarket::clob::ClobClient;
use crate::util::lock;
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct PaperOrder {
    req: OrderRequest,
    filled: Decimal,
    cancelled: bool,
}

/// Paper broker against REAL books. Conservative fill model:
/// - post-only orders that would cross are rejected, exactly like the exchange;
/// - a resting buy fills in full only once the best ask has traded down to its price;
/// - taker (FAK) orders walk the visible asks and are filled at the actual ask prices.
pub struct PaperBroker {
    clob: Arc<ClobClient>,
    orders: Mutex<HashMap<String, PaperOrder>>,
    seq: AtomicU64,
}

impl PaperBroker {
    pub fn new(clob: Arc<ClobClient>) -> Self {
        PaperBroker { clob, orders: Mutex::new(HashMap::new()), seq: AtomicU64::new(1) }
    }
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
            lock(&self.orders).insert(id.clone(), PaperOrder { req: req.clone(), filled: Decimal::ZERO, cancelled: false });
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
        let (req, filled, cancelled) = match lock(&self.orders).get(order_id) {
            Some(o) => (o.req.clone(), o.filled, o.cancelled),
            None => {
                return Ok(OrderStatus { state: OrderState::Cancelled, filled: Decimal::ZERO, avg_price: Decimal::ZERO })
            }
        };
        if filled >= req.size {
            return Ok(OrderStatus { state: OrderState::Filled, filled, avg_price: req.price });
        }
        if cancelled {
            return Ok(OrderStatus { state: OrderState::Cancelled, filled, avg_price: req.price });
        }
        let book = self.clob.book(&req.token_id).await?;
        if let Some(ask) = book.best_ask() {
            if ask.price <= req.price {
                if let Some(o) = lock(&self.orders).get_mut(order_id) {
                    o.filled = req.size;
                }
                return Ok(OrderStatus { state: OrderState::Filled, filled: req.size, avg_price: req.price });
            }
        }
        Ok(OrderStatus { state: OrderState::Open, filled, avg_price: req.price })
    }

    fn name(&self) -> &'static str {
        "paper"
    }
}
