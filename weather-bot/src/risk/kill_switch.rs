use crate::data::get_text;
use crate::error::{BotError, Result};
use crate::util::lock;
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Global trading halt. Permanent (drawdown) or temporary (volatility guards).
pub struct Halt {
    permanent: AtomicBool,
    until: Mutex<Option<Instant>>,
    reason: Mutex<String>,
}

impl Default for Halt {
    fn default() -> Self {
        Halt::new()
    }
}

impl Halt {
    pub fn new() -> Halt {
        Halt { permanent: AtomicBool::new(false), until: Mutex::new(None), reason: Mutex::new(String::new()) }
    }

    pub fn trip_permanent(&self, reason: &str) {
        self.permanent.store(true, Ordering::SeqCst);
        *lock(&self.reason) = reason.to_string();
    }

    /// Pause trading for `secs`; an existing longer pause is never shortened.
    pub fn pause(&self, secs: u64, reason: &str) {
        let new = Instant::now() + Duration::from_secs(secs);
        let mut until = lock(&self.until);
        if until.map_or(true, |cur| new > cur) {
            *until = Some(new);
            *lock(&self.reason) = reason.to_string();
        }
    }

    pub fn is_permanent(&self) -> bool {
        self.permanent.load(Ordering::SeqCst)
    }

    pub fn is_halted(&self) -> bool {
        if self.is_permanent() {
            return true;
        }
        match *lock(&self.until) {
            Some(u) => Instant::now() < u,
            None => false,
        }
    }

    pub fn reason(&self) -> String {
        lock(&self.reason).clone()
    }
}

pub fn drawdown_tripped(drawdown: Decimal, limit: Decimal) -> bool {
    drawdown >= limit
}

/// The file written when an account's drawdown kill switch trips (KILL_SWITCH_paper / KILL_SWITCH_live).
/// While it exists that account stays halted after a restart; delete it after review.
pub fn kill_file(db_path: &Path, account: &str) -> PathBuf {
    db_path.with_file_name(format!("KILL_SWITCH_{account}"))
}

/// 1-minute realized-volatility proxy: the larger of (high-low)/open over the last two 1m candles.
/// Source: Coinbase Exchange candles, rows are [time, low, high, open, close, volume], newest first.
pub async fn crypto_range_vol(http: &reqwest::Client, url: &str, symbol: &str) -> Result<f64> {
    let body = get_text(http, &format!("{url}/{symbol}/candles?granularity=60"), "crypto-vol").await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let arr = v.as_array().ok_or_else(|| BotError::Parse("candles: expected array".into()))?;
    let mut worst = 0.0f64;
    for k in arr.iter().take(2) {
        let f = |i: usize| k.get(i).and_then(|x| x.as_f64());
        if let (Some(low), Some(high), Some(open)) = (f(1), f(2), f(3)) {
            if open > 0.0 {
                worst = worst.max((high - low) / open);
            }
        }
    }
    Ok(worst)
}
