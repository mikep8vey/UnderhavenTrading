use crate::config::{Station, Unit};
use crate::data::get_text;
use crate::error::Result;
use crate::strategies::signal::{Bracket, MarketKind};
use crate::util::math::dec_from_f64;
use chrono::{DateTime, NaiveDateTime, Utc};
use rust_decimal::Decimal;

pub struct EnsembleClient {
    http: reqwest::Client,
    base: String,
    models: Vec<String>,
}

impl EnsembleClient {
    pub fn new(http: reqwest::Client, base: &str, models: Vec<String>) -> Self {
        EnsembleClient { http, base: base.to_string(), models }
    }

    /// One daily extreme per ensemble member (all models pooled), in the market unit.
    /// Uses AIRPORT lat/lon from the registry. Hourly data is requested in GMT and cut to the market's
    /// climate-day window in UTC, so the day boundary matches the settlement source.
    pub async fn member_extremes(
        &self,
        st: &Station,
        unit: Unit,
        window: (DateTime<Utc>, DateTime<Utc>),
        kind: MarketKind,
    ) -> Result<Vec<f64>> {
        let unit_s = match unit {
            Unit::C => "celsius",
            Unit::F => "fahrenheit",
        };
        let url = format!(
            "{}?latitude={}&longitude={}&hourly=temperature_2m&models={}&temperature_unit={}&timezone=GMT&start_date={}&end_date={}",
            self.base,
            st.lat,
            st.lon,
            self.models.join(","),
            unit_s,
            window.0.format("%Y-%m-%d"),
            window.1.format("%Y-%m-%d"),
        );
        let body = get_text(&self.http, &url, "ensemble").await?;
        let v: serde_json::Value = serde_json::from_str(&body)?;
        Ok(extract_extremes(&v, window, kind))
    }
}

/// Every `temperature_2m*` series in `hourly` is one member (control or perturbed).
pub fn extract_extremes(v: &serde_json::Value, window: (DateTime<Utc>, DateTime<Utc>), kind: MarketKind) -> Vec<f64> {
    let Some(hourly) = v.get("hourly").and_then(|h| h.as_object()) else { return Vec::new() };
    let Some(times) = hourly.get("time").and_then(|t| t.as_array()) else { return Vec::new() };
    let idx: Vec<usize> = times
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let s = t.as_str()?;
            let nd = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").ok()?;
            let dt = nd.and_utc();
            if dt >= window.0 && dt < window.1 {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    if idx.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (k, series) in hourly {
        if !k.starts_with("temperature_2m") {
            continue;
        }
        let Some(arr) = series.as_array() else { continue };
        let vals: Vec<f64> = idx.iter().filter_map(|&i| arr.get(i).and_then(|x| x.as_f64())).collect();
        if vals.is_empty() {
            continue;
        }
        let ext = match kind {
            MarketKind::High => vals.iter().cloned().fold(f64::MIN, f64::max),
            MarketKind::Low => vals.iter().cloned().fold(f64::MAX, f64::min),
        };
        out.push(ext);
    }
    out
}

/// P(bracket) = share of members whose floor()ed extreme lands in the bracket.
pub fn bucket_prob(members: &[f64], bracket: &Bracket) -> Decimal {
    if members.is_empty() {
        return Decimal::ZERO;
    }
    let hits = members.iter().filter(|t| bracket.contains_temp(**t)).count();
    Decimal::from(hits as u64) / Decimal::from(members.len() as u64)
}

pub fn mean(members: &[f64]) -> Decimal {
    if members.is_empty() {
        return Decimal::ZERO;
    }
    dec_from_f64(members.iter().sum::<f64>() / members.len() as f64)
}
