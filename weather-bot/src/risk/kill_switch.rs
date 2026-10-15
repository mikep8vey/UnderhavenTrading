use crate::data::get_text;
use crate::error::{BotError, Result};
use crate::util::lock;
use crate::util::math::parse_dec;
use rust_decimal::prelude::ToPrimitive;
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

    pub fn pause(&self, secs: u64, reason: &str) {
        *lock(&self.until) = Some(Instant::now() + Duration::from_secs(secs));
        *lock(&self.reason) = reason.to_string();
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

/// The file written when the drawdown kill switch trips. While it exists the bot refuses to start.
pub fn kill_file(db_path: &Path) -> PathBuf {
    db_path.with_file_name("KILL_SWITCH")
}

/// 1-minute realized-volatility proxy: the larger of (high-low)/open over the last two 1m candles.
pub async fn crypto_range_vol(http: &reqwest::Client, url: &str, symbol: &str) -> Result<f64> {
    let body = get_text(http, &format!("{url}?symbol={symbol}&interval=1m&limit=2"), "crypto-vol").await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let arr = v.as_array().ok_or_else(|| BotError::Parse("klines: expected array".into()))?;
    let mut worst = 0.0f64;
    for k in arr {
        let f = |i: usize| k.get(i).and_then(|x| x.as_str()).and_then(parse_dec).and_then(|d| d.to_f64());
        if let (Some(open), Some(high), Some(low)) = (f(1), f(2), f(3)) {
            if open > 0.0 {
                worst = worst.max((high - low) / open);
            }
        }
    }
    Ok(worst)
}
