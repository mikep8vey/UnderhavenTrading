use crate::util::math::floor_bucket;
use crate::util::time::now_rfc3339;
use once_cell::sync::Lazy;
use regex::Regex;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarketKind {
    High,
    Low,
}

/// An integer-degree bracket in the market's unit. `lo`/`hi` are inclusive bucket indices,
/// where a bucket is `floor(temperature)`. `None` = open-ended.
/// "70-71°F" => lo 70, hi 71 (covers 70.0 <= t < 72.0). "23°C" => lo = hi = 23.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bracket {
    pub lo: Option<i32>,
    pub hi: Option<i32>,
}

static BELOW: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(-?\d+)\s*°?\s*[CF]?\s*(?:or\s+(?:below|lower|less)|and\s+below)").expect("valid regex")
});
static HIGHER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(-?\d+)\s*°?\s*[CF]?\s*(?:or\s+(?:higher|above|more)|and\s+above)").expect("valid regex")
});
static LE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:≤|<=)\s*(-?\d+)").expect("valid regex"));
static GE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:≥|>=)\s*(-?\d+)").expect("valid regex"));
static RANGE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(-?\d+)\s*°?\s*[CF]?\s*(?:-|–|—|to)\s*(-?\d+)\s*°").expect("valid regex")
});
static SINGLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(-?\d+)\s*°").expect("valid regex"));

fn num(c: &regex::Captures<'_>, i: usize) -> Option<i32> {
    c.get(i).and_then(|m| m.as_str().parse::<i32>().ok())
}

impl Bracket {
    pub fn parse(s: &str) -> Option<Bracket> {
        if let Some(c) = BELOW.captures(s).or_else(|| LE.captures(s)) {
            return Some(Bracket { lo: None, hi: Some(num(&c, 1)?) });
        }
        if let Some(c) = HIGHER.captures(s).or_else(|| GE.captures(s)) {
            return Some(Bracket { lo: Some(num(&c, 1)?), hi: None });
        }
        if let Some(c) = RANGE.captures(s) {
            let (a, b) = (num(&c, 1)?, num(&c, 2)?);
            if a > b {
                return None;
            }
            return Some(Bracket { lo: Some(a), hi: Some(b) });
        }
        if let Some(c) = SINGLE.captures(s) {
            let a = num(&c, 1)?;
            return Some(Bracket { lo: Some(a), hi: Some(a) });
        }
        None
    }

    /// floor() assignment: a 23.9 reading resolves in bucket 23.
    pub fn contains_temp(&self, t: f64) -> bool {
        let b = floor_bucket(t);
        self.lo.map_or(true, |l| b >= l) && self.hi.map_or(true, |h| b <= h)
    }

    pub fn label(&self) -> String {
        match (self.lo, self.hi) {
            (Some(l), Some(h)) if l == h => format!("{l}"),
            (Some(l), Some(h)) => format!("{l}-{h}"),
            (None, Some(h)) => format!("<={h}"),
            (Some(l), None) => format!(">={l}"),
            (None, None) => "any".to_string(),
        }
    }
}

/// A row for the `signals` table (and the `signals` CLI printout).
#[derive(Debug, Clone)]
pub struct Signal {
    pub ts: String,
    pub city: String,
    pub station_icao: String,
    pub strategy: &'static str,
    pub bracket: String,
    pub model_prob: Option<Decimal>,
    pub market_ask: Option<Decimal>,
    pub market_bid: Option<Decimal>,
    pub edge: Option<Decimal>,
    pub settlement_check: String,
    pub decision: String,
    pub reason: String,
}

impl Signal {
    pub fn new(city: &str, icao: &str, strategy: &'static str, bracket: &str) -> Signal {
        Signal {
            ts: now_rfc3339(),
            city: city.to_string(),
            station_icao: icao.to_string(),
            strategy,
            bracket: bracket.to_string(),
            model_prob: None,
            market_ask: None,
            market_bid: None,
            edge: None,
            settlement_check: "n/a".to_string(),
            decision: "SKIP".to_string(),
            reason: String::new(),
        }
    }

    pub fn skip(mut self, reason: &str) -> Signal {
        self.decision = "SKIP".to_string();
        self.reason = reason.to_string();
        self
    }

    pub fn trade(mut self, reason: &str) -> Signal {
        self.decision = "TRADE".to_string();
        self.reason = reason.to_string();
        self
    }
}
