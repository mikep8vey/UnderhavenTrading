use crate::config::Unit;
use crate::data::get_text;
use crate::error::{BotError, Result};
use crate::util::lock;
use crate::util::math::c_to_unit;
use crate::util::time::{climate_date, DayBasis};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Obs {
    pub time: DateTime<Utc>,
    pub temp_c: f64,
    pub max_t24_c: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct RunningExtreme {
    pub date: NaiveDate,
    /// Highest / lowest observed so far on the climate day, in the market's unit.
    pub high: f64,
    pub low: f64,
    pub last_obs: DateTime<Utc>,
    pub n: usize,
}

/// Dedup window: polls inside this window reuse the snapshot.
const FRESH: Duration = Duration::from_secs(20);
/// If a fetch fails, a snapshot up to 5 minutes old is still served.
const STALE: Duration = Duration::from_secs(300);

pub struct MetarClient {
    http: reqwest::Client,
    base: String,
    cache: Mutex<HashMap<String, (Instant, Vec<Obs>)>>,
}

impl MetarClient {
    pub fn new(http: reqwest::Client, base: &str) -> Self {
        MetarClient { http, base: base.to_string(), cache: Mutex::new(HashMap::new()) }
    }

    fn cached(&self, icao: &str, max_age: Duration) -> Option<Vec<Obs>> {
        let g = lock(&self.cache);
        g.get(icao).filter(|(t, _)| t.elapsed() <= max_age).map(|(_, v)| v.clone())
    }

    /// Last ~30h of METAR/SPECI observations (enough to rebuild the local-day running extreme after a restart).
    pub async fn observations(&self, icao: &str) -> Result<Vec<Obs>> {
        if let Some(v) = self.cached(icao, FRESH) {
            return Ok(v);
        }
        let url = format!("{}?ids={}&format=json&hours=30", self.base, icao);
        match get_text(&self.http, &url, "metar").await {
            Ok(body) => {
                let obs = parse_metar_json(&body)?;
                lock(&self.cache).insert(icao.to_string(), (Instant::now(), obs.clone()));
                Ok(obs)
            }
            Err(e) => match self.cached(icao, STALE) {
                Some(v) => {
                    tracing::warn!("metar {icao}: fetch failed ({e}); serving cached snapshot");
                    Ok(v)
                }
                None => Err(e),
            },
        }
    }
}

pub fn parse_metar_json(body: &str) -> Result<Vec<Obs>> {
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(body)?;
    let arr = v.as_array().ok_or_else(|| BotError::Parse("metar: expected a JSON array".into()))?;
    let mut out = Vec::new();
    for o in arr {
        let t = o.get("obsTime").and_then(|x| x.as_i64());
        let temp = o.get("temp").and_then(|x| x.as_f64());
        if let (Some(t), Some(temp)) = (t, temp) {
            if let Some(time) = DateTime::from_timestamp(t, 0) {
                out.push(Obs { time, temp_c: temp, max_t24_c: o.get("maxT24").and_then(|x| x.as_f64()) });
            }
        }
    }
    out.sort_by_key(|o| o.time);
    Ok(out)
}

/// Running high/low over the station-local climate day `date` (never the UTC day), in market units.
pub fn running_extreme(obs: &[Obs], date: NaiveDate, tz: Tz, basis: DayBasis, unit: Unit) -> Option<RunningExtreme> {
    let mut hi = f64::MIN;
    let mut lo = f64::MAX;
    let mut n = 0usize;
    let mut last: Option<DateTime<Utc>> = None;
    for o in obs {
        if climate_date(o.time, tz, basis) != date {
            continue;
        }
        let t = c_to_unit(o.temp_c, unit);
        hi = hi.max(t);
        lo = lo.min(t);
        n += 1;
        last = Some(o.time);
    }
    last.map(|last_obs| RunningExtreme { date, high: hi, low: lo, last_obs, n })
}

/// Max-min temperature swing (°C) among observations in the trailing window.
pub fn swing_c(obs: &[Obs], now: DateTime<Utc>, window_mins: i64) -> f64 {
    let cutoff = now - chrono::Duration::minutes(window_mins);
    let temps: Vec<f64> = obs.iter().filter(|o| o.time >= cutoff && o.time <= now).map(|o| o.temp_c).collect();
    if temps.len() < 2 {
        return 0.0;
    }
    let hi = temps.iter().cloned().fold(f64::MIN, f64::max);
    let lo = temps.iter().cloned().fold(f64::MAX, f64::min);
    hi - lo
}
