use crate::data::get_text;
use crate::error::{BotError, Result};
use crate::util::math::json_dec;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Level {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Default)]
pub struct Book {
    /// Sorted best (highest) first.
    pub bids: Vec<Level>,
    /// Sorted best (lowest) first.
    pub asks: Vec<Level>,
}

impl Book {
    pub fn new(mut bids: Vec<Level>, mut asks: Vec<Level>) -> Book {
        bids.sort_by(|a, b| b.price.cmp(&a.price));
        asks.sort_by(|a, b| a.price.cmp(&b.price));
        Book { bids, asks }
    }

    pub fn from_json(v: &serde_json::Value) -> Book {
        let side = |k: &str| -> Vec<Level> {
            v.get(k)
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|l| {
                            Some(Level { price: json_dec(l.get("price")?)?, size: json_dec(l.get("size")?)? })
                        })
                        .filter(|l| l.size > Decimal::ZERO)
                        .collect()
                })
                .unwrap_or_default()
        };
        Book::new(side("bids"), side("asks"))
    }

    pub fn best_bid(&self) -> Option<Level> {
        self.bids.first().copied()
    }
    pub fn best_ask(&self) -> Option<Level> {
        self.asks.first().copied()
    }
    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }
    pub fn mid(&self) -> Option<Decimal> {
        Some((self.best_ask()?.price + self.best_bid()?.price) / Decimal::new(2, 0))
    }
    /// Total ask size at prices <= `price` (liquidity available at or inside our ceiling).
    pub fn ask_depth_up_to(&self, price: Decimal) -> Decimal {
        self.asks.iter().filter(|l| l.price <= price).map(|l| l.size).sum()
    }
}

pub struct ClobClient {
    http: reqwest::Client,
    base: String,
}

impl ClobClient {
    pub fn new(http: reqwest::Client, base: &str) -> Self {
        ClobClient { http, base: base.trim_end_matches('/').to_string() }
    }

    pub async fn book(&self, token_id: &str) -> Result<Book> {
        let body = get_text(&self.http, &format!("{}/book?token_id={}", self.base, token_id), "clob-book").await?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(Book::from_json(&v))
    }

    /// Base fee in bps the exchange expects on signed orders for this token.
    pub async fn fee_rate_bps(&self, token_id: &str) -> Result<u64> {
        let body = get_text(&self.http, &format!("{}/fee-rate?token_id={}", self.base, token_id), "clob-fee").await?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        v.get("base_fee")
            .or_else(|| v.get("fee_rate_bps"))
            .and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
            .ok_or_else(|| BotError::Parse("fee-rate response had no base_fee".into()))
    }
}
