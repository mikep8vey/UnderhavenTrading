//! Public Bybit WebSocket client with explicit supervision.
//!
//! The official SDK's stream helper does not reconnect, resubscribe or enforce a
//! heartbeat watchdog, so this module implements the small subset the lab needs:
//!
//! * subscribe in batches of at most 10 topics;
//! * send an application-level `{"op":"ping"}` every 20 seconds and reconnect if
//!   no frame arrives for 40 seconds;
//! * exponential backoff with jitter, capped at 30 seconds;
//! * order-book snapshot/delta sequence validation with a REST resync on a gap;
//! * resubscribe (via a clean reconnect) when the selected symbol set changes;
//! * never write to the exchange. Only `subscribe`, `ping` and `pong` frames are
//!   ever sent.

use crate::bybit::{merge_book_levels, PublicClient};
use crate::market::MarketCache;
use crate::model::{BookLevel, Candle, Interval, Ticker};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsStatus {
    pub state: String,
    pub detail: String,
    pub last_message_ms: Option<i64>,
    pub connected_at_ms: Option<i64>,
    pub reconnects: u64,
    pub messages: u64,
    pub orderbook_resyncs: u64,
}

impl Default for WsStatus {
    fn default() -> Self {
        Self {
            state: "starting".into(),
            detail: "initialising public market stream".into(),
            last_message_ms: None,
            connected_at_ms: None,
            reconnects: 0,
            messages: 0,
            orderbook_resyncs: 0,
        }
    }
}

impl WsStatus {
    pub fn healthy(&self, now_ms: i64, max_age_ms: i64) -> bool {
        self.state == "connected"
            && self
                .last_message_ms
                .map(|ts| now_ms.saturating_sub(ts) <= max_age_ms)
                .unwrap_or(false)
    }
}

pub fn subscription_args(symbols: &[String], intervals: &[Interval], depth: u16) -> Vec<String> {
    let mut out = Vec::new();
    for symbol in symbols {
        out.push(format!("tickers.{symbol}"));
        out.push(format!("orderbook.{depth}.{symbol}"));
    }
    let mut unique: Vec<Interval> = Vec::new();
    for interval in intervals {
        if !unique.contains(interval) {
            unique.push(*interval);
        }
    }
    for interval in unique {
        for symbol in symbols {
            out.push(format!("kline.{}.{symbol}", interval.bybit()));
        }
    }
    out
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    op: String,
    #[serde(default)]
    success: Option<bool>,
    #[serde(default)]
    ret_msg: Option<String>,
    #[serde(default)]
    topic: String,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    data: Option<Value>,
}

fn parse_levels(value: &Value) -> Vec<(f64, f64)> {
    value
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    let arr = row.as_array()?;
                    if arr.len() < 2 {
                        return None;
                    }
                    let price = arr[0].as_str()?.parse::<f64>().ok()?;
                    let size = arr[1].as_str()?.parse::<f64>().ok()?;
                    Some((price, size))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn to_levels(updates: &[(f64, f64)]) -> Vec<BookLevel> {
    updates
        .iter()
        .map(|(price, size)| BookLevel {
            price: *price,
            size: *size,
        })
        .collect()
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn number(v: &Value, key: &str) -> f64 {
    v.get(key)
        .and_then(|x| {
            x.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| x.as_f64())
        })
        .unwrap_or(0.0)
}

pub fn parse_ticker(symbol: &str, value: &Value, now_ms: i64) -> Option<Ticker> {
    let ticker = Ticker {
        symbol: symbol.to_string(),
        last_price: number(value, "lastPrice"),
        mark_price: number(value, "markPrice"),
        index_price: number(value, "indexPrice"),
        bid_price: number(value, "bid1Price"),
        bid_size: number(value, "bid1Size"),
        ask_price: number(value, "ask1Price"),
        ask_size: number(value, "ask1Size"),
        price_24h_pct: number(value, "price24hPcnt"),
        turnover_24h: number(value, "turnover24h"),
        volume_24h: number(value, "volume24h"),
        open_interest_value: number(value, "openInterestValue"),
        funding_rate: number(value, "fundingRate"),
        next_funding_time_ms: text(value, "nextFundingTime").parse::<i64>().unwrap_or(0),
        funding_interval_hours: number(value, "fundingIntervalHour"),
        ts_ms: value.get("ts").and_then(|v| v.as_i64()).unwrap_or(now_ms),
        source: "bybit_v5_ws".into(),
    };
    ticker.is_valid().then_some(ticker)
}

fn parse_kline(interval: Interval, symbol: &str, value: &Value, now_ms: i64) -> Option<Candle> {
    let candle = Candle {
        symbol: symbol.to_string(),
        interval,
        open_time_ms: text(value, "start").parse::<i64>().ok()?,
        open: number(value, "open"),
        high: number(value, "high"),
        low: number(value, "low"),
        close: number(value, "close"),
        volume: number(value, "volume"),
        turnover: number(value, "turnover"),
        confirm: value
            .get("confirm")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        source: "bybit_v5_ws".into(),
        ingested_at_ms: now_ms,
    };
    candle.validate().ok().map(|_| candle)
}

/// Apply one decoded WebSocket text frame to the cache.
pub async fn apply_frame(
    cache: &Arc<RwLock<MarketCache>>,
    client: &PublicClient,
    status: &Arc<RwLock<WsStatus>>,
    raw: &str,
) {
    let Ok(envelope) = serde_json::from_str::<Envelope>(raw) else {
        debug!("unparseable public WS frame ignored");
        return;
    };
    if !envelope.op.is_empty() {
        if envelope.op == "pong" {
            let mut s = status.write().await;
            s.last_message_ms = Some(chrono::Utc::now().timestamp_millis());
            return;
        }
        if envelope.success == Some(false) {
            let mut s = status.write().await;
            s.detail = format!(
                "subscription rejected: {}",
                envelope.ret_msg.unwrap_or_default()
            );
            s.state = "degraded".into();
        }
        return;
    }
    let Some(data) = envelope.data else { return };
    let parts: Vec<&str> = envelope.topic.split('.').collect();
    if parts.len() < 2 {
        return;
    }
    let now = chrono::Utc::now().timestamp_millis();
    match parts[0] {
        "tickers" => {
            let symbol = parts[1];
            if let Some(ticker) = parse_ticker(symbol, &data, now) {
                let mut c = cache.write().await;
                c.tickers.insert(symbol.to_string(), ticker);
            }
        }
        "kline" => {
            if parts.len() < 3 {
                return;
            }
            let Ok(interval) = parts[1].parse::<Interval>() else {
                return;
            };
            let symbol = parts[2];
            let items = data.as_array().cloned().unwrap_or_default();
            let mut c = cache.write().await;
            let mut count = 0usize;
            for item in items {
                if let Some(candle) = parse_kline(interval, symbol, &item, now) {
                    c.upsert_candle(candle);
                    count += 1;
                }
            }
            drop(c);
            if count == 0 {
                debug!(
                    symbol,
                    ?interval,
                    "ignored kline frame without valid candles"
                );
            }
        }
        "orderbook" => {
            if parts.len() < 3 {
                return;
            }
            let symbol = parts[2].to_string();
            let mut gap = false;
            {
                let mut c = cache.write().await;
                let entry = c.books.entry(symbol.clone()).or_default();
                let bids = data.get("b").map(parse_levels).unwrap_or_default();
                let asks = data.get("a").map(parse_levels).unwrap_or_default();
                let update_id = data.get("u").and_then(|v| v.as_i64()).unwrap_or(0);
                let seq = data.get("seq").and_then(|v| v.as_i64()).unwrap_or(0);
                let exchange_ts = data
                    .get("cts")
                    .and_then(|v| v.as_i64())
                    .or_else(|| data.get("ts").and_then(|v| v.as_i64()))
                    .unwrap_or(now);
                if envelope.kind == "snapshot" || entry.bids.is_empty() {
                    entry.bids = to_levels(&bids);
                    entry.asks = to_levels(&asks);
                    entry.bids.sort_by(|a, b| b.price.total_cmp(&a.price));
                    entry.asks.sort_by(|a, b| a.price.total_cmp(&b.price));
                    entry.update_id = update_id;
                    entry.seq = seq;
                    entry.valid = true;
                    entry.gap_count = 0;
                } else {
                    if update_id <= entry.update_id {
                        // Stale duplicate.
                        return;
                    }
                    if update_id != entry.update_id + 1 {
                        entry.gap_count += 1;
                        entry.valid = false;
                        gap = true;
                    } else {
                        merge_book_levels(&mut entry.bids, &bids, true);
                        merge_book_levels(&mut entry.asks, &asks, false);
                        entry.update_id = update_id;
                        entry.seq = seq;
                    }
                }
                entry.symbol = symbol.clone();
                entry.ts_ms = exchange_ts;
                entry.received_ms = now;
                entry.source = "bybit_v5_ws".into();
                if !gap {
                    entry.valid = !entry.is_crossed() && entry.mid().is_some();
                }
            }
            if gap {
                warn!(symbol, "orderbook sequence gap; resynchronising from REST");
                if let Ok(book) = client.orderbook(&symbol, 50).await {
                    let mut c = cache.write().await;
                    c.books.insert(symbol.clone(), book);
                    let mut s = status.write().await;
                    s.orderbook_resyncs += 1;
                    s.detail = format!("orderbook resynchronised for {symbol}");
                }
            }
        }
        _ => {}
    }
    let mut s = status.write().await;
    s.messages += 1;
    s.last_message_ms = Some(now);
}

async fn connect_once(
    url: &str,
    cache: &Arc<RwLock<MarketCache>>,
    status: &Arc<RwLock<WsStatus>>,
    client: &PublicClient,
    args: &[String],
    shutdown: &CancellationToken,
) -> Result<(), String> {
    let (ws, _response) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    info!(topics = args.len(), "public market WebSocket connected");
    {
        let mut s = status.write().await;
        s.state = "connected".into();
        s.detail = format!("connected to {url}");
        s.connected_at_ms = Some(chrono::Utc::now().timestamp_millis());
        s.last_message_ms = Some(chrono::Utc::now().timestamp_millis());
    }
    let (mut sink, mut stream) = ws.split();

    for chunk in args.chunks(10) {
        let msg = serde_json::json!({ "op": "subscribe", "args": chunk });
        sink.send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| format!("subscribe send failed: {e}"))?;
    }

    let mut ping = tokio::time::interval(Duration::from_secs(20));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = ping.tick() => {
                sink.send(Message::Text("{\"op\":\"ping\"}".to_string().into()))
                    .await
                    .map_err(|e| format!("ping send failed: {e}"))?;
            }
            frame = tokio::time::timeout(Duration::from_secs(40), stream.next()) => {
                match frame {
                    Err(_) => return Err("heartbeat watchdog: no frame for 40s".into()),
                    Ok(None) => return Err("stream closed by server".into()),
                    Ok(Some(Err(e))) => return Err(format!("stream error: {e}")),
                    Ok(Some(Ok(message))) => match message {
                        Message::Text(text) => {
                            apply_frame(cache, client, status, text.as_str()).await;
                        }
                        Message::Binary(bin) => {
                            if let Ok(text) = String::from_utf8(bin.to_vec()) {
                                apply_frame(cache, client, status, &text).await;
                            }
                        }
                        Message::Ping(payload) => {
                            sink.send(Message::Pong(payload))
                                .await
                                .map_err(|e| format!("pong send failed: {e}"))?;
                        }
                        Message::Close(frame) => {
                            return Err(format!("server closed stream: {frame:?}"));
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Supervised stream loop. `symbols_rx` changes trigger a clean reconnect so the
/// server-side subscription matches the scanner selection.
pub async fn run(
    url: String,
    cache: Arc<RwLock<MarketCache>>,
    status: Arc<RwLock<WsStatus>>,
    client: PublicClient,
    mut symbols_rx: watch::Receiver<Vec<String>>,
    intervals: Vec<Interval>,
    shutdown: CancellationToken,
) {
    let mut backoff_secs: u64 = 1;
    let mut attempts: u64 = 0;
    while !shutdown.is_cancelled() {
        let symbols = symbols_rx.borrow().clone();
        if symbols.is_empty() {
            {
                let mut s = status.write().await;
                s.state = "idle".into();
                s.detail = "waiting for eligible symbols from the scanner".into();
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                changed = symbols_rx.changed() => {
                    if changed.is_err() { break; }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            continue;
        }
        let args = subscription_args(&symbols, &intervals, 50);
        {
            let mut s = status.write().await;
            s.state = "connecting".into();
            s.detail = format!("connecting with {} topics", args.len());
            if attempts > 0 {
                s.reconnects += 1;
            }
        }
        attempts += 1;
        let started = std::time::Instant::now();
        let outcome = tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = symbols_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                Err("symbol selection changed; resubscribing".to_string())
            }
            result = connect_once(&url, &cache, &status, &client, &args, &shutdown) => result,
        };
        match outcome {
            Ok(()) if shutdown.is_cancelled() => break,
            Ok(()) => {}
            Err(reason) => {
                {
                    let mut s = status.write().await;
                    s.state = "reconnecting".into();
                    s.detail = reason;
                }
                warn!("public WebSocket reconnecting");
            }
        }
        if started.elapsed() > Duration::from_secs(90) {
            backoff_secs = 1;
        }
        let jitter = rand::random::<u64>() % 500;
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter)) => {}
        }
        backoff_secs = (backoff_secs * 2).min(30);
    }
    let mut s = status.write().await;
    s.state = "stopped".into();
    s.detail = "public market stream stopped".into();
}

/// REST fallback used at startup to populate tickers and books before the first
/// WebSocket snapshot arrives (and to recover from a failed stream).
pub async fn prime_cache(
    client: &PublicClient,
    cache: &Arc<RwLock<MarketCache>>,
) -> Result<(), String> {
    let tickers = client
        .tickers()
        .await
        .map_err(|e| format!("tickers: {e}"))?;
    let symbols: Vec<String> = tickers.iter().map(|t| t.symbol.clone()).take(30).collect();
    {
        let mut c = cache.write().await;
        c.upsert_tickers(tickers);
    }
    for symbol in symbols {
        if let Ok(book) = client.orderbook(&symbol, 50).await {
            let mut c = cache.write().await;
            c.books.insert(symbol, book);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_batched_and_deduplicated() {
        let args = subscription_args(
            &["BTCUSDT".into(), "ETHUSDT".into()],
            &[Interval::M5, Interval::M5, Interval::H1],
            50,
        );
        assert!(args.contains(&"tickers.BTCUSDT".to_string()));
        assert!(args.contains(&"orderbook.50.ETHUSDT".to_string()));
        assert!(args.contains(&"kline.5.BTCUSDT".to_string()));
        assert!(args.contains(&"kline.60.ETHUSDT".to_string()));
        assert_eq!(args.iter().filter(|a| *a == "kline.5.BTCUSDT").count(), 1);
    }

    #[test]
    fn status_health_requires_recent_message() {
        let mut s = WsStatus::default();
        assert!(!s.healthy(10_000, 5_000));
        s.state = "connected".into();
        s.last_message_ms = Some(8_000);
        assert!(s.healthy(10_000, 5_000));
        assert!(!s.healthy(14_000, 5_000));
    }

    #[test]
    fn ticker_parser_rejects_crossed_quotes() {
        let value = serde_json::json!({
            "lastPrice": "100",
            "bid1Price": "101",
            "ask1Price": "100"
        });
        assert!(parse_ticker("BTCUSDT", &value, 1).is_none());
    }
}
