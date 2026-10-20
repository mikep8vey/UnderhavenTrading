//! Public trade tape (`data-api.polymarket.com/trades`).
//!
//! Paper mode uses it to decide whether a resting maker bid would have been hit. The book alone cannot
//! tell you this: a passive buy fills when a seller crosses the spread, which leaves the ask unchanged.
//! The tape reports the taker side, so a `SELL` print at or below our bid is the fill signal.
use crate::data::get_text;
use crate::error::Result;
use crate::util::lock;
use crate::util::math::json_dec;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CACHE_MS: u64 = 50;
const ERROR_COOLDOWN_SECS: u64 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct Print {
    pub condition_id: String,
    pub asset: String,
    /// Taker side ("BUY" or "SELL").
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub timestamp: i64,
    pub tx: String,
    pub wallet: String,
}

struct Cache {
    by_market: HashMap<String, (Instant, Vec<Print>)>,
    error_until: Option<Instant>,
}

/// Fetch the last ~100 prints for a market. Short cache because `PaperBroker::status` polls every few
/// seconds; short error cooldown so a data-api outage cannot stall the order loop.
pub struct TradesClient {
    http: reqwest::Client,
    base: String,
    cache: Mutex<Cache>,
}

impl TradesClient {
    pub fn new(http: reqwest::Client, base: &str) -> Self {
        TradesClient {
            http,
            base: base.trim_end_matches('/').to_string(),
            cache: Mutex::new(Cache { by_market: HashMap::new(), error_until: None }),
        }
    }

    pub async fn recent(&self, condition_id: &str) -> Result<Vec<Print>> {
        {
            let g = lock(&self.cache);
            if let Some(until) = g.error_until {
                if until > Instant::now() {
                    return Ok(Vec::new());
                }
            }
            if let Some((t, v)) = g.by_market.get(condition_id) {
                if t.elapsed() < Duration::from_millis(CACHE_MS) {
                    return Ok(v.clone());
                }
            }
        }
        let url = format!("{}/trades?market={}&limit=100", self.base, condition_id);
        match get_text(&self.http, &url, "data-trades").await {
            Ok(body) => {
                let v: serde_json::Value = serde_json::from_str(&body)?;
                // The API is supposed to filter by `market`, but keep only this condition in case it
                // ever ignores the parameter: a print from another market must not fill this order.
                let prints: Vec<Print> = parse_prints(&v)
                    .into_iter()
                    .filter(|p| p.condition_id.is_empty() || p.condition_id.eq_ignore_ascii_case(condition_id))
                    .collect();
                let mut g = lock(&self.cache);
                g.by_market.insert(condition_id.to_string(), (Instant::now(), prints.clone()));
                g.error_until = None;
                Ok(prints)
            }
            Err(e) => {
                lock(&self.cache).error_until = Some(Instant::now() + Duration::from_secs(ERROR_COOLDOWN_SECS));
                Err(e)
            }
        }
    }
}

pub fn parse_prints(v: &serde_json::Value) -> Vec<Print> {
    let Some(arr) = v.as_array() else { return Vec::new() };
    let mut out: Vec<Print> = arr
        .iter()
        .filter_map(|t| {
            let condition_id = t.get("conditionId").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let asset = t.get("asset")?.as_str()?.to_string();
            let side = t.get("side")?.as_str()?.to_string();
            let price = t.get("price").and_then(json_dec)?;
            let size = t.get("size").and_then(json_dec)?;
            let timestamp = t
                .get("timestamp")
                .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))?;
            let tx = t.get("transactionHash").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let wallet = t.get("proxyWallet").and_then(|x| x.as_str()).unwrap_or("").to_string();
            Some(Print { condition_id, asset, side, price, size, timestamp, tx, wallet })
        })
        .filter(|p| p.price > Decimal::ZERO && p.size > Decimal::ZERO)
        .collect();
    out.sort_by_key(|p| p.timestamp);
    out
}
