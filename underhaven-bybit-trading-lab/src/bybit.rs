//! Small, typed Bybit V5 adapter.
//!
//! # Why not the official `bybit-api` crate?
//!
//! The official Rust SDK (`bybit-api` 0.1.2, MIT) was reviewed before this module
//! was written. Its public market methods are a useful reference, but it does not
//! expose the pagination or resilience this platform needs:
//!
//! * `get_klines` has no `start`/`end` pagination.
//! * `get_funding_history` has no `startTime`/`endTime` pagination.
//! * `get_instruments_info` has no `status`, `limit` or cursor arguments, and its
//!   instrument models fall back to `serde_json::Value` for key fields.
//! * its WebSocket client has no reconnect/resubscribe/heartbeat watchdog and its
//!   callbacks silently default unparseable payloads.
//! * there is no rate-limit-aware retry policy, and it uses native-tls while this
//!   project pins rustls.
//!
//! This adapter therefore implements only the handful of V5 endpoints the paper
//! platform needs, with explicit retries, cursor/range pagination and typed
//! models. It is not a general-purpose SDK.
//!
//! # Paper-only
//!
//! There is **no** order-entry method here. The signed methods are restricted to
//! read-only account metadata (`/v5/account/fee-rate`, `/v5/user/query-api`).
//! No order-entry, cancellation or position-management endpoint exists in this
//! crate.

use crate::model::{q8, BookLevel, Candle, FundingRate, Instrument, Interval, OrderBook, Ticker};
use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use url::form_urlencoded;

type HmacSha256 = Hmac<Sha256>;

const RECV_WINDOW: &str = "5000";
const MAX_RETRIES: usize = 4;

#[derive(Debug, Error)]
pub enum BybitError {
    #[error("HTTP transport error: {0}")]
    Http(String),
    #[error("Bybit API error {code}: {message}")]
    Api { code: i64, message: String },
    #[error("rate limited by Bybit (retCode {code})")]
    RateLimited { code: i64 },
    #[error("could not parse Bybit response: {0}")]
    Parse(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("read-only credentials rejected: {0}")]
    Credentials(String),
}

#[derive(Debug, Clone, Default)]
pub struct ApiMetrics {
    pub api_errors: Arc<AtomicU64>,
    pub rate_limit_hits: Arc<AtomicU64>,
    pub requests: Arc<AtomicU64>,
}

impl ApiMetrics {
    pub fn errors(&self) -> u64 {
        self.api_errors.load(Ordering::Relaxed)
    }
    pub fn rate_limits(&self) -> u64 {
        self.rate_limit_hits.load(Ordering::Relaxed)
    }
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }
}

/// Conservative client-side pacing. Bybit's global limit is 600 requests per 5s
/// per IP, but per-endpoint limits are lower; 100ms spacing keeps market polling
/// well inside the published envelope without a fragile per-endpoint table.
struct RateLimiter {
    next: Mutex<Instant>,
    min_gap: Duration,
}

impl RateLimiter {
    fn new(min_gap: Duration) -> Self {
        Self {
            next: Mutex::new(Instant::now()),
            min_gap,
        }
    }

    async fn wait(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep(*next - now).await;
        }
        *next = Instant::now() + self.min_gap;
    }
}

#[derive(Clone)]
pub struct PublicClient {
    http: reqwest::Client,
    base: String,
    limiter: Arc<RateLimiter>,
    pub metrics: ApiMetrics,
}

impl fmt::Debug for PublicClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicClient")
            .field("base", &self.base)
            .field("requests", &self.metrics.requests())
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiEnvelope<T> {
    #[serde(rename = "retCode")]
    pub ret_code: i64,
    #[serde(rename = "retMsg", default)]
    pub ret_msg: String,
    pub result: Option<T>,
    #[serde(default)]
    pub time: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiCredentials {
    pub api_key: String,
    pub api_secret: String,
}

impl Drop for ApiCredentials {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.api_secret.zeroize();
    }
}

impl PublicClient {
    pub fn new(base: &str) -> Result<Self, BybitError> {
        let http = reqwest::Client::builder()
            .user_agent(format!(
                "{}/{} (paper research; public market data)",
                crate::APP_NAME,
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(8)
            .build()
            .map_err(|e| BybitError::Http(e.to_string()))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            limiter: Arc::new(RateLimiter::new(Duration::from_millis(100))),
            metrics: ApiMetrics::default(),
        })
    }

    fn sign_query(secret: &str, timestamp: i64, api_key: &str, query: &str) -> String {
        let payload = format!("{timestamp}{api_key}{RECV_WINDOW}{query}");
        let mut mac =
            HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    async fn get_raw(
        &self,
        path: &str,
        params: &[(&str, String)],
        credentials: Option<&ApiCredentials>,
    ) -> Result<String, BybitError> {
        let query = {
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            for (k, v) in params {
                serializer.append_pair(k, v);
            }
            serializer.finish()
        };
        let url = if query.is_empty() {
            format!("{}{}", self.base, path)
        } else {
            format!("{}{}?{}", self.base, path, query)
        };

        let mut last_error = None;
        for attempt in 0..=MAX_RETRIES {
            self.limiter.wait().await;
            self.metrics.requests.fetch_add(1, Ordering::Relaxed);
            let mut request = self.http.get(&url);
            if let Some(creds) = credentials {
                let ts = chrono::Utc::now().timestamp_millis();
                let signature = Self::sign_query(&creds.api_secret, ts, &creds.api_key, &query);
                request = request
                    .header("X-BAPI-API-KEY", &creds.api_key)
                    .header("X-BAPI-TIMESTAMP", ts.to_string())
                    .header("X-BAPI-RECV-WINDOW", RECV_WINDOW)
                    .header("X-BAPI-SIGN", signature);
            }
            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    self.metrics.api_errors.fetch_add(1, Ordering::Relaxed);
                    last_error = Some(BybitError::Http(e.to_string()));
                    if attempt < MAX_RETRIES {
                        backoff(attempt).await;
                        continue;
                    }
                    break;
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(t) => t,
                Err(e) => {
                    self.metrics.api_errors.fetch_add(1, Ordering::Relaxed);
                    last_error = Some(BybitError::Http(e.to_string()));
                    if attempt < MAX_RETRIES {
                        backoff(attempt).await;
                        continue;
                    }
                    break;
                }
            };
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                self.metrics.rate_limit_hits.fetch_add(1, Ordering::Relaxed);
                last_error = Some(BybitError::Http(format!(
                    "HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                )));
                if attempt < MAX_RETRIES {
                    backoff(attempt + 1).await;
                    continue;
                }
                break;
            }
            if !status.is_success() {
                self.metrics.api_errors.fetch_add(1, Ordering::Relaxed);
                last_error = Some(BybitError::Http(format!(
                    "HTTP {status}: {}",
                    text.chars().take(200).collect::<String>()
                )));
                if status.is_client_error() {
                    break;
                }
                if attempt < MAX_RETRIES {
                    backoff(attempt).await;
                    continue;
                }
                break;
            }
            // Bybit sometimes returns HTTP 200 with a retCode error.
            if let Ok(envelope) = serde_json::from_str::<ApiEnvelope<serde_json::Value>>(&text) {
                if envelope.ret_code == 10006 {
                    self.metrics.rate_limit_hits.fetch_add(1, Ordering::Relaxed);
                    last_error = Some(BybitError::RateLimited { code: 10006 });
                    if attempt < MAX_RETRIES {
                        backoff(attempt + 1).await;
                        continue;
                    }
                    break;
                }
                if envelope.ret_code == 0 {
                    return Ok(text);
                }
                if envelope.ret_code != 0 {
                    self.metrics.api_errors.fetch_add(1, Ordering::Relaxed);
                    return Err(BybitError::Api {
                        code: envelope.ret_code,
                        message: envelope.ret_msg,
                    });
                }
            }
            return Ok(text);
        }
        Err(last_error.unwrap_or_else(|| BybitError::Http("unknown transport failure".into())))
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
        credentials: Option<&ApiCredentials>,
    ) -> Result<T, BybitError> {
        let text = self.get_raw(path, params, credentials).await?;
        let envelope: ApiEnvelope<T> =
            serde_json::from_str(&text).map_err(|e| BybitError::Parse(e.to_string()))?;
        if envelope.ret_code != 0 {
            return Err(BybitError::Api {
                code: envelope.ret_code,
                message: envelope.ret_msg,
            });
        }
        envelope
            .result
            .ok_or_else(|| BybitError::Parse("response has no result".into()))
    }

    pub async fn server_time_ms(&self) -> Result<i64, BybitError> {
        #[derive(Deserialize)]
        struct ServerTime {
            #[serde(rename = "timeSecond")]
            time_second: String,
            #[serde(rename = "timeNano")]
            time_nano: String,
        }
        let r: ServerTime = self
            .get("/v5/market/time", &[], None)
            .await
            .map_err(|e| BybitError::Parse(format!("time: {e}")))?;
        let seconds = r.time_second.parse::<i64>().unwrap_or(0);
        let nanos = r.time_nano.parse::<i64>().unwrap_or(0);
        Ok(seconds * 1000 + nanos / 1_000_000)
    }

    pub async fn instruments(&self) -> Result<Vec<Instrument>, BybitError> {
        #[derive(Deserialize)]
        struct RawInstrument {
            symbol: String,
            #[serde(rename = "baseCoin", default)]
            base_coin: String,
            #[serde(rename = "quoteCoin", default)]
            quote_coin: String,
            #[serde(rename = "settleCoin", default)]
            settle_coin: String,
            #[serde(rename = "contractType", default)]
            contract_type: String,
            #[serde(default)]
            status: String,
            #[serde(rename = "launchTime", default)]
            launch_time: String,
            #[serde(rename = "priceFilter", default)]
            price_filter: Option<PriceFilter>,
            #[serde(rename = "lotSizeFilter", default)]
            lot_filter: Option<LotFilter>,
            #[serde(rename = "fundingInterval", default)]
            funding_interval: u32,
        }
        #[derive(Deserialize, Default)]
        struct PriceFilter {
            #[serde(rename = "tickSize", default)]
            tick_size: String,
        }
        #[derive(Deserialize, Default)]
        struct LotFilter {
            #[serde(rename = "qtyStep", default)]
            qty_step: String,
            #[serde(rename = "minOrderQty", default)]
            min_qty: String,
            #[serde(rename = "maxOrderQty", default)]
            max_qty: String,
            #[serde(rename = "maxMktOrderQty", default)]
            max_market_qty: String,
            #[serde(rename = "minNotionalValue", default)]
            min_notional: String,
        }
        #[derive(Deserialize)]
        struct Result {
            list: Vec<RawInstrument>,
            #[serde(rename = "nextPageCursor", default)]
            next_cursor: String,
        }

        let now = chrono::Utc::now().timestamp_millis();
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..20 {
            let mut params = vec![
                ("category", "linear".to_string()),
                ("limit", "500".to_string()),
            ];
            if !cursor.is_empty() {
                params.push(("cursor", cursor.clone()));
            }
            let result: Result = self
                .get("/v5/market/instruments-info", &params, None)
                .await?;
            for raw in result.list {
                let base = raw.base_coin;
                let quote = raw.quote_coin;
                let settle = raw.settle_coin;
                let contract_type = raw.contract_type;
                let status = raw.status;
                let symbol = raw.symbol;
                let launch_time_ms = raw.launch_time.parse::<i64>().unwrap_or(0);
                let price_filter = raw.price_filter.unwrap_or_default();
                let lot_filter = raw.lot_filter.unwrap_or_default();
                out.push(Instrument {
                    symbol,
                    base_coin: base,
                    quote_coin: quote,
                    settle_coin: settle,
                    contract_type,
                    status,
                    tick_size: price_filter.tick_size.parse().unwrap_or(0.0),
                    qty_step: lot_filter.qty_step.parse().unwrap_or(0.0),
                    min_qty: lot_filter.min_qty.parse().unwrap_or(0.0),
                    max_qty: lot_filter.max_qty.parse().unwrap_or(0.0),
                    max_market_qty: lot_filter.max_market_qty.parse().unwrap_or(0.0),
                    min_notional: lot_filter.min_notional.parse().unwrap_or(0.0),
                    funding_interval_minutes: if raw.funding_interval == 0 {
                        480
                    } else {
                        raw.funding_interval
                    },
                    launch_time_ms,
                    source: "bybit_v5_rest".into(),
                    fetched_at_ms: now,
                });
            }
            if result.next_cursor.is_empty() {
                break;
            }
            cursor = result.next_cursor;
        }
        debug!(count = out.len(), "loaded Bybit instruments");
        Ok(out)
    }

    pub async fn tickers(&self) -> Result<Vec<Ticker>, BybitError> {
        #[derive(Deserialize)]
        struct RawTicker {
            symbol: String,
            #[serde(rename = "lastPrice", default)]
            last_price: String,
            #[serde(rename = "markPrice", default)]
            mark_price: String,
            #[serde(rename = "indexPrice", default)]
            index_price: String,
            #[serde(rename = "bid1Price", default)]
            bid_price: String,
            #[serde(rename = "bid1Size", default)]
            bid_size: String,
            #[serde(rename = "ask1Price", default)]
            ask_price: String,
            #[serde(rename = "ask1Size", default)]
            ask_size: String,
            #[serde(rename = "price24hPcnt", default)]
            price_24h_pct: String,
            #[serde(rename = "turnover24h", default)]
            turnover_24h: String,
            #[serde(rename = "volume24h", default)]
            volume_24h: String,
            #[serde(rename = "openInterestValue", default)]
            open_interest_value: String,
            #[serde(rename = "fundingRate", default)]
            funding_rate: String,
            #[serde(rename = "nextFundingTime", default)]
            next_funding_time: String,
        }
        #[derive(Deserialize)]
        struct Result {
            list: Vec<RawTicker>,
        }
        let now = chrono::Utc::now().timestamp_millis();
        let result: Result = self
            .get(
                "/v5/market/tickers",
                &[("category", "linear".to_string())],
                None,
            )
            .await?;
        Ok(result
            .list
            .into_iter()
            .map(|r| Ticker {
                symbol: r.symbol,
                last_price: r.last_price.parse().unwrap_or(0.0),
                mark_price: r.mark_price.parse().unwrap_or(0.0),
                index_price: r.index_price.parse().unwrap_or(0.0),
                bid_price: r.bid_price.parse().unwrap_or(0.0),
                bid_size: r.bid_size.parse().unwrap_or(0.0),
                ask_price: r.ask_price.parse().unwrap_or(0.0),
                ask_size: r.ask_size.parse().unwrap_or(0.0),
                price_24h_pct: r.price_24h_pct.parse().unwrap_or(0.0),
                turnover_24h: r.turnover_24h.parse().unwrap_or(0.0),
                volume_24h: r.volume_24h.parse().unwrap_or(0.0),
                open_interest_value: r.open_interest_value.parse().unwrap_or(0.0),
                funding_rate: r.funding_rate.parse().unwrap_or(0.0),
                next_funding_time_ms: r.next_funding_time.parse().unwrap_or(0),
                funding_interval_hours: 8.0,
                ts_ms: now,
                source: "bybit_v5_rest".into(),
            })
            .collect())
    }

    /// Fetch one page of klines. Bybit returns newest-first; this method reverses
    /// them so callers always receive ascending time order.
    pub async fn klines_page(
        &self,
        symbol: &str,
        interval: Interval,
        start_ms: Option<i64>,
        end_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Candle>, BybitError> {
        #[derive(Deserialize)]
        struct Result {
            list: Vec<Vec<String>>,
        }
        let mut params = vec![
            ("category", "linear".to_string()),
            ("symbol", symbol.to_string()),
            ("interval", interval.bybit().to_string()),
            ("limit", limit.min(1000).to_string()),
        ];
        if let Some(start) = start_ms {
            params.push(("start", start.to_string()));
        }
        if let Some(end) = end_ms {
            params.push(("end", end.to_string()));
        }
        let result: Result = self.get("/v5/market/kline", &params, None).await?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut candles: Vec<Candle> = result
            .list
            .into_iter()
            .filter_map(|row| {
                if row.len() < 7 {
                    return None;
                }
                Some(Candle {
                    symbol: symbol.to_string(),
                    interval,
                    open_time_ms: row[0].parse().ok()?,
                    open: row[1].parse().ok()?,
                    high: row[2].parse().ok()?,
                    low: row[3].parse().ok()?,
                    close: row[4].parse().ok()?,
                    volume: row[5].parse().ok()?,
                    turnover: row[6].parse().ok()?,
                    confirm: true,
                    source: "bybit_v5_rest".into(),
                    ingested_at_ms: now,
                })
            })
            .collect();
        candles.sort_by_key(|c| c.open_time_ms);
        candles.dedup_by_key(|c| c.open_time_ms);
        Ok(candles)
    }

    /// Paginate `[start_ms, end_ms]` and return ascending, deduplicated candles.
    pub async fn klines_range(
        &self,
        symbol: &str,
        interval: Interval,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>, BybitError> {
        if start_ms >= end_ms {
            return Err(BybitError::Invalid("start must be before end".into()));
        }
        let mut out: Vec<Candle> = Vec::new();
        let mut cursor_end = end_ms;
        for _ in 0..200 {
            let page = self
                .klines_page(symbol, interval, Some(start_ms), Some(cursor_end), 1000)
                .await?;
            if page.is_empty() {
                break;
            }
            let oldest = page.first().map(|c| c.open_time_ms).unwrap_or(cursor_end);
            out.extend(page);
            if oldest <= start_ms || oldest == cursor_end {
                break;
            }
            cursor_end = oldest - 1;
            if out.len() > 200_000 {
                break;
            }
        }
        out.sort_by_key(|c| c.open_time_ms);
        out.dedup_by_key(|c| c.open_time_ms);
        out.retain(|c| c.open_time_ms >= start_ms && c.open_time_ms <= end_ms);
        Ok(out)
    }

    pub async fn orderbook(&self, symbol: &str, depth: u16) -> Result<OrderBook, BybitError> {
        #[derive(Deserialize)]
        struct Result {
            #[serde(default)]
            s: String,
            #[serde(default)]
            b: Vec<Vec<String>>,
            #[serde(default)]
            a: Vec<Vec<String>>,
            #[serde(default)]
            ts: i64,
            #[serde(default)]
            u: i64,
            #[serde(default)]
            seq: i64,
        }
        let depth = depth.clamp(1, 200);
        let result: Result = self
            .get(
                "/v5/market/orderbook",
                &[
                    ("category", "linear".to_string()),
                    ("symbol", symbol.to_string()),
                    ("limit", depth.to_string()),
                ],
                None,
            )
            .await?;
        let parse_levels = |rows: Vec<Vec<String>>| -> Vec<BookLevel> {
            rows.into_iter()
                .filter_map(|r| {
                    if r.len() < 2 {
                        return None;
                    }
                    Some(BookLevel {
                        price: r[0].parse().ok()?,
                        size: r[1].parse().ok()?,
                    })
                })
                .collect()
        };
        let mut bids = parse_levels(result.b);
        let mut asks = parse_levels(result.a);
        bids.sort_by(|a, b| b.price.total_cmp(&a.price));
        asks.sort_by(|a, b| a.price.total_cmp(&b.price));
        let mut book = OrderBook {
            symbol: if result.s.is_empty() {
                symbol.to_string()
            } else {
                result.s
            },
            ts_ms: result.ts,
            received_ms: chrono::Utc::now().timestamp_millis(),
            update_id: result.u,
            seq: result.seq,
            bids,
            asks,
            source: "bybit_v5_rest".into(),
            valid: true,
            gap_count: 0,
        };
        book.valid = !book.is_crossed() && book.mid().is_some();
        Ok(book)
    }

    pub async fn funding_history(
        &self,
        symbol: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingRate>, BybitError> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "fundingRate", default)]
            rate: String,
            #[serde(rename = "fundingRateTimestamp", default)]
            ts: String,
        }
        #[derive(Deserialize)]
        struct Result {
            list: Vec<Raw>,
        }
        let now = chrono::Utc::now().timestamp_millis();
        let mut out = Vec::new();
        let mut cursor_end = end_ms;
        for _ in 0..100 {
            let result: Result = self
                .get(
                    "/v5/market/funding/history",
                    &[
                        ("category", "linear".to_string()),
                        ("symbol", symbol.to_string()),
                        ("startTime", start_ms.to_string()),
                        ("endTime", cursor_end.to_string()),
                        ("limit", "200".to_string()),
                    ],
                    None,
                )
                .await?;
            if result.list.is_empty() {
                break;
            }
            let mut oldest = i64::MAX;
            for r in result.list {
                let Some(ts) = r.ts.parse::<i64>().ok() else {
                    continue;
                };
                oldest = oldest.min(ts);
                out.push(FundingRate {
                    symbol: symbol.to_string(),
                    funding_time_ms: ts,
                    rate: r.rate.parse().unwrap_or(0.0),
                    source: "bybit_v5_rest".into(),
                    fetched_at_ms: now,
                });
            }
            if oldest <= start_ms || oldest == i64::MAX || oldest == cursor_end {
                break;
            }
            cursor_end = oldest - 1;
        }
        out.sort_by_key(|f| f.funding_time_ms);
        out.dedup_by_key(|f| f.funding_time_ms);
        Ok(out)
    }

    /// Read-only. Returns `(taker_bps, maker_bps)` for a linear symbol.
    pub async fn fee_rate(
        &self,
        credentials: &ApiCredentials,
        symbol: &str,
    ) -> Result<(f64, f64), BybitError> {
        #[derive(Deserialize)]
        struct Item {
            #[serde(rename = "takerFeeRate", default)]
            taker: String,
            #[serde(rename = "makerFeeRate", default)]
            maker: String,
        }
        #[derive(Deserialize)]
        struct Result {
            list: Vec<Item>,
        }
        let result: Result = self
            .get(
                "/v5/account/fee-rate",
                &[
                    ("category", "linear".to_string()),
                    ("symbol", symbol.to_string()),
                ],
                Some(credentials),
            )
            .await?;
        let item = result
            .list
            .first()
            .ok_or_else(|| BybitError::Parse("empty fee-rate list".into()))?;
        let taker = item.taker.parse::<f64>().unwrap_or(0.0) * 10_000.0;
        let maker = item.maker.parse::<f64>().unwrap_or(0.0) * 10_000.0;
        Ok((taker, maker))
    }

    /// Read-only key verification. Rejects keys that are not read-only.
    pub async fn verify_read_only_key(
        &self,
        credentials: &ApiCredentials,
    ) -> Result<ReadOnlyKeyInfo, BybitError> {
        let value: serde_json::Value = self
            .get("/v5/user/query-api", &[], Some(credentials))
            .await?;
        let read_only = value
            .get("readOnly")
            .and_then(|v| {
                v.as_i64().map(|n| n == 1).or_else(|| {
                    v.as_str()
                        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                })
            })
            .unwrap_or(false);
        let note = value
            .get("note")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let uid = value
            .get("userID")
            .or_else(|| value.get("uid"))
            .and_then(|v| v.as_str().or_else(|| v.as_i64().map(|_| "")))
            .map(|s| s.to_string());
        if !read_only {
            return Err(BybitError::Credentials(
                "the supplied key is not marked read-only; refusing to store it".into(),
            ));
        }
        Ok(ReadOnlyKeyInfo {
            read_only,
            note,
            uid,
            permissions: value.get("permissions").cloned().unwrap_or_default(),
        })
    }

    /// Read-only balance summary used only by the admin "connection" panel.
    pub async fn wallet_balance_available(
        &self,
        credentials: &ApiCredentials,
    ) -> Result<f64, BybitError> {
        #[derive(Deserialize)]
        struct Result {
            #[serde(default)]
            list: Vec<serde_json::Value>,
        }
        let result: Result = self
            .get(
                "/v5/account/wallet-balance",
                &[("accountType", "UNIFIED".to_string())],
                Some(credentials),
            )
            .await?;
        let total = result
            .list
            .first()
            .and_then(|v| v.get("totalAvailableBalance"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        Ok(total)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadOnlyKeyInfo {
    pub read_only: bool,
    pub note: String,
    pub uid: Option<String>,
    pub permissions: serde_json::Value,
}

async fn backoff(attempt: usize) {
    let base = 250u64.saturating_mul(1 << attempt.min(5));
    let jitter = rand::random::<u64>() % 200;
    tokio::time::sleep(Duration::from_millis(base + jitter)).await;
}

/// Reconcile local time against Bybit. A large positive offset means the local
/// clock is behind; signed requests use Bybit's server time only for display and
/// diagnostics, not to silently rewrite the system clock.
pub async fn clock_offset_ms(client: &PublicClient) -> Result<i64, BybitError> {
    let server = client.server_time_ms().await?;
    let local = chrono::Utc::now().timestamp_millis();
    Ok(server - local)
}

/// Parse a private WebSocket `orderbook` payload into the shared model. Kept here
/// so both REST and WS paths use one representation.
pub fn merge_book_levels(existing: &mut Vec<BookLevel>, updates: &[(f64, f64)], bids: bool) {
    for (price, size) in updates {
        if let Some(idx) = existing
            .iter()
            .position(|l| (l.price - price).abs() <= f64::EPSILON)
        {
            if *size <= 0.0 {
                existing.remove(idx);
            } else {
                existing[idx].size = *size;
            }
        } else if *size > 0.0 {
            existing.push(BookLevel {
                price: *price,
                size: *size,
            });
        }
    }
    if bids {
        existing.sort_by(|a, b| b.price.total_cmp(&a.price));
    } else {
        existing.sort_by(|a, b| a.price.total_cmp(&b.price));
    }
}

/// Best-effort book integrity check used by scanner and paper engine.
pub fn book_is_tradeable(book: &OrderBook, now_ms: i64, max_age_ms: i64) -> bool {
    book.valid
        && !book.is_crossed()
        && now_ms.saturating_sub(book.received_ms) <= max_age_ms
        && book.best_bid().is_some()
        && book.best_ask().is_some()
}

/// Convert raw strings from Bybit to clean numbers without panicking.
pub fn parse_f64(value: &str) -> f64 {
    value.parse::<f64>().unwrap_or(0.0)
}

/// Depth-weighted notional available in the book, used for scanner ranking.
pub fn depth_usd(book: &OrderBook, side: crate::model::Side, bps: f64) -> f64 {
    q8(book.depth_notional(side, bps))
}

pub fn funding_map(rates: &[FundingRate]) -> HashMap<String, f64> {
    let mut map = HashMap::new();
    for r in rates {
        map.insert(r.symbol.clone(), r.rate);
    }
    map
}

pub fn tickers_by_symbol(tickers: &[Ticker]) -> HashMap<String, Ticker> {
    tickers
        .iter()
        .cloned()
        .map(|t| (t.symbol.clone(), t))
        .collect()
}

pub fn log_api_error(context: &str, error: &BybitError) {
    match error {
        BybitError::RateLimited { .. } => warn!(context, %error, "Bybit rate limit"),
        _ => debug!(context, %error, "Bybit request failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_is_deterministic_and_hex() {
        let sig = PublicClient::sign_query("secret", 1_700_000_000_000, "key", "category=linear");
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        let sig2 = PublicClient::sign_query("secret", 1_700_000_000_000, "key", "category=linear");
        assert_eq!(sig, sig2);
    }

    #[test]
    fn book_merge_respects_deletions_and_order() {
        let mut bids = vec![
            BookLevel {
                price: 100.0,
                size: 1.0,
            },
            BookLevel {
                price: 99.0,
                size: 2.0,
            },
        ];
        merge_book_levels(&mut bids, &[(100.5, 3.0), (99.0, 0.0)], true);
        assert_eq!(bids.len(), 2);
        assert_eq!(bids[0].price, 100.5);
        assert_eq!(bids[1].price, 100.0);
    }

    #[test]
    fn paper_only_adapter_has_no_order_entry_endpoint() {
        // Static guard: the private API surface is limited to read-only metadata.
        // The needles are assembled at runtime so this assertion does not match
        // its own source text.
        let source = include_str!("bybit.rs");
        let create = concat!("/v5/order", "/create");
        let cancel = concat!("/v5/order", "/cancel");
        let position = concat!("/v5/position", "/");
        assert!(!source.contains(create));
        assert!(!source.contains(cancel));
        assert!(!source.contains(position));
        assert!(source.contains("/v5/account/fee-rate"));
        assert!(source.contains("/v5/user/query-api"));
    }

    #[test]
    fn api_envelope_parses_bybit_camel_case() {
        let raw = r#"{"retCode":0,"retMsg":"OK","result":{"list":[]},"time":1}"#;
        let env: ApiEnvelope<serde_json::Value> = serde_json::from_str(raw).unwrap();
        assert_eq!(env.ret_code, 0);
    }
}
