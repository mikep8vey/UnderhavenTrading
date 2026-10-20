use crate::error::{BotError, Result};
use crate::util::math::{floor_dp, floor_to};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    /// Good-til-cancelled (resting maker order).
    Gtc,
    /// Fill-and-kill (taker fallback).
    Fak,
}

/// The bot only ever BUYS (YES or NO tokens) and holds to settlement.
#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub token_id: String,
    /// Polymarket condition id, used by the paper broker to read the public trade tape.
    pub condition_id: String,
    pub price: Decimal,
    pub size: Decimal,
    pub post_only: bool,
    pub order_type: OrderType,
    pub neg_risk: bool,
}

pub struct OrderBuilder {
    allow_taker: bool,
}

impl OrderBuilder {
    pub fn new(allow_taker: bool) -> Self {
        OrderBuilder { allow_taker }
    }

    pub fn allow_taker(&self) -> bool {
        self.allow_taker
    }

    /// Every order starts as a post-only GTC. post_only is ALWAYS true here.
    pub fn maker(&self, token_id: &str, condition_id: &str, price: Decimal, size: Decimal, neg_risk: bool, tick: Decimal) -> OrderRequest {
        OrderRequest {
            token_id: token_id.to_string(),
            condition_id: condition_id.to_string(),
            price: floor_to(price, tick),
            size: floor_dp(size, 2),
            post_only: true,
            order_type: OrderType::Gtc,
            neg_risk,
        }
    }

    /// post_only=false is only possible when the operator started the bot with --allow-taker.
    pub fn taker(&self, token_id: &str, condition_id: &str, price: Decimal, size: Decimal, neg_risk: bool, tick: Decimal) -> Result<OrderRequest> {
        if !self.allow_taker {
            return Err(BotError::Execution("taker orders are disabled (start with --allow-taker to enable)".into()));
        }
        Ok(OrderRequest {
            token_id: token_id.to_string(),
            condition_id: condition_id.to_string(),
            price: floor_to(price, tick),
            size: floor_dp(size, 2),
            post_only: false,
            order_type: OrderType::Fak,
            neg_risk,
        })
    }
}

/// Guard for two-sided quoting (not enabled in this build): never quote both sides at a combined price > 0.99.
pub fn two_sided_sum_ok(bid_a: Decimal, bid_b: Decimal) -> bool {
    bid_a + bid_b <= Decimal::new(99, 2)
}
