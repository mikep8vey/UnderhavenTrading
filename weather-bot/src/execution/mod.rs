pub mod live_broker;
pub mod maker_layer;
pub mod order_builder;
pub mod paper_broker;

use crate::error::Result;
use async_trait::async_trait;
use order_builder::OrderRequest;
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub enum OrderState {
    Open,
    Filled,
    Cancelled,
    Rejected(String),
}

#[derive(Debug, Clone)]
pub struct OrderAck {
    pub id: String,
    pub state: OrderState,
    /// Shares filled so far.
    pub filled: Decimal,
    pub avg_price: Decimal,
}

#[derive(Debug, Clone)]
pub struct OrderStatus {
    pub state: OrderState,
    pub filled: Decimal,
    pub avg_price: Decimal,
}

#[async_trait]
pub trait Broker: Send + Sync {
    async fn place(&self, req: &OrderRequest) -> Result<OrderAck>;
    /// Cancel by order id (CLOB cancel-by-id endpoint in live mode).
    async fn cancel(&self, order_id: &str) -> Result<()>;
    async fn status(&self, order_id: &str) -> Result<OrderStatus>;
    fn name(&self) -> &'static str;
}
