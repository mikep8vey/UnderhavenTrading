use crate::data::get_text;
use crate::data::station_registry::StationRegistry;
use crate::error::Result;
use crate::strategies::signal::{Bracket, MarketKind};
use crate::util::math::{json_dec, parse_dec};
use chrono::{Datelike, Duration, NaiveDate};
use once_cell::sync::Lazy;
use regex::Regex;
use rust_decimal::Decimal;

/// Fee schedule exactly as published on the market (never hard-coded).
/// Per-share taker fee = price * rate * (price * (1 - price)) ^ exponent
/// (Polymarket's published fee-curve form; verify against the current docs before trusting rebate estimates).
#[derive(Debug, Clone)]
pub struct FeeSchedule {
    pub rate: Decimal,
    pub exponent: u32,
    pub rebate_rate: Decimal,
}

impl FeeSchedule {
    pub fn fee_per_share(&self, price: Decimal) -> Decimal {
        let base = price * (Decimal::ONE - price);
        let mut curve = Decimal::ONE;
        for _ in 0..self.exponent.max(1) {
            curve *= base;
        }
        price * self.rate * curve
    }
    pub fn fee(&self, price: Decimal, size: Decimal) -> Decimal {
        self.fee_per_share(price) * size
    }
    /// Maker's share of the fee pool for a fill of this size at this price (estimate).
    pub fn maker_rebate(&self, price: Decimal, size: Decimal) -> Decimal {
        self.rebate_rate * self.fee(price, size)
    }
}

#[derive(Debug, Clone)]
pub struct Market {
    pub id: String,
    pub condition_id: String,
    pub question: String,
    pub group_title: String,
    pub yes_token: String,
    pub no_token: String,
    pub yes_idx: usize,
    pub neg_risk: bool,
    pub tick: Decimal,
    pub fee: Option<FeeSchedule>,
    pub closed: bool,
    pub active: bool,
    pub accepting_orders: bool,
}

#[derive(Debug, Clone)]
pub struct WeatherEvent {
    pub event_id: String,
    pub title: String,
    pub kind: MarketKind,
    pub city: String,
    pub date: NaiveDate,
    /// Event + market rules text (used to confirm the settlement station).
    pub rules: String,
    pub markets: Vec<(Bracket, Market)>,
}

impl WeatherEvent {
    pub fn key(&self) -> String {
        format!("{}|{}|{:?}", self.city, self.date, self.kind)
    }
}

static TITLE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(highest|lowest)\s+temperature\s+in\s+(.+?)\s+on\s+([A-Za-z]+)\s+(\d{1,2})").expect("valid regex")
});

fn str_list(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::String(s) => serde_json::from_str::<Vec<String>>(s).ok(),
        serde_json::Value::Array(a) => Some(a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect()),
        _ => None,
    }
}

fn parse_fee(m: &serde_json::Value) -> Option<FeeSchedule> {
    let fs = m.get("feeSchedule")?;
    let rate = fs.get("rate").and_then(json_dec).or_else(|| fs.get("feeRate").and_then(json_dec))?;
    let exponent = fs.get("exponent").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
    let rebate_rate = fs.get("rebateRate").and_then(json_dec).unwrap_or(Decimal::ZERO);
    Some(FeeSchedule { rate, exponent, rebate_rate })
}

fn parse_market(m: &serde_json::Value) -> Option<Market> {
    let ids = str_list(m.get("clobTokenIds")?)?;
    let outcomes = str_list(m.get("outcomes")?)?;
    if ids.len() != 2 || outcomes.len() != 2 {
        return None;
    }
    let yes_idx = outcomes.iter().position(|o| o.eq_ignore_ascii_case("yes"))?;
    let s = |k: &str| m.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Some(Market {
        id: m.get("id").map(|x| x.as_str().map(|s| s.to_string()).unwrap_or_else(|| x.to_string())).unwrap_or_default(),
        condition_id: s("conditionId"),
        question: s("question"),
        group_title: s("groupItemTitle"),
        yes_token: ids[yes_idx].clone(),
        no_token: ids[1 - yes_idx].clone(),
        yes_idx,
        neg_risk: m.get("negRisk").and_then(|x| x.as_bool()).unwrap_or(false),
        tick: m.get("orderPriceMinTickSize").and_then(json_dec).unwrap_or(Decimal::new(1, 2)),
        fee: parse_fee(m),
        closed: m.get("closed").and_then(|x| x.as_bool()).unwrap_or(false),
        active: m.get("active").and_then(|x| x.as_bool()).unwrap_or(true),
        accepting_orders: m.get("acceptingOrders").and_then(|x| x.as_bool()).unwrap_or(true),
    })
}

fn parse_date(month: &str, day: &str, today: NaiveDate) -> Option<NaiveDate> {
    let d: u32 = day.parse().ok()?;
    let year = today.year();
    let try_year = |y: i32| NaiveDate::parse_from_str(&format!("{month} {d} {y}"), "%B %d %Y").ok();
    let cand = try_year(year)?;
    if cand < today - Duration::days(200) {
        try_year(year + 1)
    } else {
        Some(cand)
    }
}

/// Turn a Gamma `/events` page into tradable-looking weather events for registry cities.
pub fn parse_events(json: &serde_json::Value, registry: &StationRegistry, today: NaiveDate) -> Vec<WeatherEvent> {
    let Some(arr) = json.as_array() else { return Vec::new() };
    let mut out = Vec::new();
    for ev in arr {
        let title = ev.get("title").and_then(|x| x.as_str()).unwrap_or("");
        let Some(c) = TITLE.captures(title) else { continue };
        let kind = if c[1].eq_ignore_ascii_case("highest") { MarketKind::High } else { MarketKind::Low };
        let Some(city) = registry.city_from_text(&c[2]) else { continue };
        let Some(date) = parse_date(&c[3], &c[4], today) else { continue };
        let mut rules = ev.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let mut markets = Vec::new();
        if let Some(ms) = ev.get("markets").and_then(|x| x.as_array()) {
            for (i, m) in ms.iter().enumerate() {
                if i == 0 {
                    rules.push(' ');
                    rules.push_str(m.get("description").and_then(|x| x.as_str()).unwrap_or(""));
                }
                let Some(mk) = parse_market(m) else { continue };
                let bracket = Bracket::parse(&mk.group_title).or_else(|| Bracket::parse(&mk.question));
                if let Some(b) = bracket {
                    markets.push((b, mk));
                }
            }
        }
        if markets.is_empty() {
            continue;
        }
        out.push(WeatherEvent {
            event_id: ev.get("id").map(|x| x.as_str().map(|s| s.to_string()).unwrap_or_else(|| x.to_string())).unwrap_or_default(),
            title: title.to_string(),
            kind,
            city: city.name.clone(),
            date,
            rules,
            markets,
        });
    }
    out
}

pub struct GammaClient {
    http: reqwest::Client,
    base: String,
}

impl GammaClient {
    pub fn new(http: reqwest::Client, base: &str) -> Self {
        GammaClient { http, base: base.trim_end_matches('/').to_string() }
    }

    pub async fn weather_events(&self, registry: &StationRegistry, today: NaiveDate) -> Result<Vec<WeatherEvent>> {
        let mut all = Vec::new();
        for page in 0..4 {
            let url = format!(
                "{}/events?tag_slug=weather&closed=false&active=true&limit=100&offset={}",
                self.base,
                page * 100
            );
            let body = get_text(&self.http, &url, "gamma-events").await?;
            let v: serde_json::Value = serde_json::from_str(&body)?;
            let n = v.as_array().map(|a| a.len()).unwrap_or(0);
            all.extend(parse_events(&v, registry, today));
            if n < 100 {
                break;
            }
        }
        Ok(all)
    }

    /// Payout per share (1 or 0) once the market has resolved; None while unresolved.
    pub async fn resolution(&self, market_id: &str, outcome_idx: usize) -> Result<Option<Decimal>> {
        let body = get_text(&self.http, &format!("{}/markets/{}", self.base, market_id), "gamma-market").await?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        if !v.get("closed").and_then(|x| x.as_bool()).unwrap_or(false) {
            return Ok(None);
        }
        let prices = v.get("outcomePrices").and_then(str_list).unwrap_or_default();
        let p = prices.get(outcome_idx).and_then(|s| parse_dec(s));
        Ok(match p {
            Some(p) if p >= Decimal::new(99, 2) => Some(Decimal::ONE),
            Some(p) if p <= Decimal::new(1, 2) => Some(Decimal::ZERO),
            _ => None,
        })
    }
}
