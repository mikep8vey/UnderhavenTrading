use crate::error::{BotError, Result};
use chrono::{
    DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeZone, Timelike, Utc,
};
use chrono_tz::{OffsetComponents, Tz};
use serde::Deserialize;
use std::future::Future;

/// Which calendar day a station's daily extreme belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DayBasis {
    /// Civil local calendar day.
    Local,
    /// NWS climatological day: local STANDARD time all year (1am-1am during DST).
    Lst,
}

/// UTC RFC3339 timestamp for storage.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn base_offset(tz: Tz, at_utc: NaiveDateTime) -> Duration {
    tz.offset_from_utc_datetime(&at_utc).base_utc_offset()
}

/// The station-local climate date an instant belongs to (never the UTC date).
pub fn climate_date(utc: DateTime<Utc>, tz: Tz, basis: DayBasis) -> NaiveDate {
    match basis {
        DayBasis::Local => utc.with_timezone(&tz).date_naive(),
        DayBasis::Lst => (utc.naive_utc() + base_offset(tz, utc.naive_utc())).date(),
    }
}

fn day_start_utc(date: NaiveDate, tz: Tz, basis: DayBasis) -> DateTime<Utc> {
    let mid = date.and_time(NaiveTime::MIN);
    let via_standard = || Utc.from_utc_datetime(&(mid - base_offset(tz, mid)));
    match basis {
        DayBasis::Local => match tz.from_local_datetime(&mid).earliest() {
            Some(d) => d.with_timezone(&Utc),
            None => via_standard(),
        },
        DayBasis::Lst => via_standard(),
    }
}

/// [start, end) of a climate day, in UTC.
pub fn climate_window_utc(date: NaiveDate, tz: Tz, basis: DayBasis) -> (DateTime<Utc>, DateTime<Utc>) {
    let next = date.succ_opt().unwrap_or(date);
    (day_start_utc(date, tz, basis), day_start_utc(next, tz, basis))
}

pub fn local_hour(utc: DateTime<Utc>, tz: Tz) -> u32 {
    utc.with_timezone(&tz).hour()
}

/// Every network call goes through this: 5s timeout per attempt, 3 attempts, exponential backoff.
pub async fn retry<T, F, Fut>(label: &str, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut delay = std::time::Duration::from_millis(500);
    let mut last = BotError::Network(format!("{label}: no attempt made"));
    for attempt in 1..=3u32 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), f()).await {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => last = e,
            Err(_) => last = BotError::Network(format!("{label}: timed out after 5s")),
        }
        tracing::warn!(attempt, "{label} failed: {last}");
        if attempt < 3 {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
    }
    Err(last)
}
