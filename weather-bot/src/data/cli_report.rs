use crate::data::get_text;
use crate::error::Result;
use crate::util::lock;
use chrono::NaiveDate;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct CliReport {
    pub date: NaiveDate,
    pub max_f: Option<f64>,
    pub min_f: Option<f64>,
}

static HEADER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)CLIMATE SUMMARY FOR\s+([A-Z]+)\s+(\d{1,2})\s+(\d{4})").expect("valid regex"));
static MAXIMUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?mi)^\s*MAXIMUM\s+(-?\d+)").expect("valid regex"));
static MINIMUM: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?mi)^\s*MINIMUM\s+(-?\d+)").expect("valid regex"));

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Parse every "CLIMATE SUMMARY FOR <MONTH> <D> <YYYY>" block in an NWS CLI text product.
/// The first MAXIMUM/MINIMUM lines of a block are the TEMPERATURE (F) section.
pub fn parse_cli(text: &str) -> Vec<CliReport> {
    let heads: Vec<(usize, usize, Option<NaiveDate>)> = HEADER
        .captures_iter(text)
        .filter_map(|c| {
            let m = c.get(0)?;
            let d = NaiveDate::parse_from_str(
                &format!("{} {} {}", title_case(c.get(1)?.as_str()), c.get(2)?.as_str(), c.get(3)?.as_str()),
                "%B %d %Y",
            )
            .ok();
            Some((m.start(), m.end(), d))
        })
        .collect();
    let mut out = Vec::new();
    for (i, (_, end, date)) in heads.iter().enumerate() {
        let Some(date) = date else { continue };
        let stop = heads.get(i + 1).map(|h| h.0).unwrap_or(text.len());
        let chunk = &text[*end..stop];
        let grab = |re: &Regex| re.captures(chunk).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<f64>().ok());
        out.push(CliReport { date: *date, max_f: grab(&MAXIMUM), min_f: grab(&MINIMUM) });
    }
    out
}

pub struct CliClient {
    http: reqwest::Client,
    base: String,
    ttl: Duration,
    cache: Mutex<HashMap<String, (Instant, Option<CliReport>)>>,
}

impl CliClient {
    pub fn new(http: reqwest::Client, base: &str, ttl_secs: u64) -> Self {
        CliClient { http, base: base.to_string(), ttl: Duration::from_secs(ttl_secs), cache: Mutex::new(HashMap::new()) }
    }

    /// Latest CLI bulletin for a PIL (the resolution-identical source). Refetched at most once per TTL (hourly by default).
    /// Among several bulletins the most recent climate date wins; for equal dates the last one in the text.
    pub async fn latest(&self, pils: &str) -> Result<Option<CliReport>> {
        {
            let g = lock(&self.cache);
            if let Some((t, r)) = g.get(pils) {
                if t.elapsed() < self.ttl {
                    return Ok(r.clone());
                }
            }
        }
        let url = format!("{}?pil={}&fmt=text&limit=3", self.base, pils);
        let body = get_text(&self.http, &url, "nws-cli").await?;
        let latest = parse_cli(&body).into_iter().max_by_key(|r| r.date);
        lock(&self.cache).insert(pils.to_string(), (Instant::now(), latest.clone()));
        Ok(latest)
    }
}
