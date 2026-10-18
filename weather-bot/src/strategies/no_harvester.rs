//! Strategy 2 - LATE-DAY NO HARVESTER (afternoon/evening).
//!
//! A bracket is only ever PROVISIONALLY_DEAD after the running observed extreme crosses it; no NO order
//! is placed until SETTLEMENT_SAFETY_CHECK passes.
//!
//! Crossing rule (floor space): for a HIGH market a bracket is dead once floor(running_high) > bracket.hi,
//! i.e. running_high >= hi + 1. This is deliberately stricter than "running_high >= bracket_max": at
//! running_high == hi the bracket is still alive (the day's max can still land in it).
//! For a LOW market it is dead once floor(running_low) < bracket.lo.
use crate::app::App;
use crate::data::metar::running_extreme;
use crate::error::Result;
use crate::execution::maker_layer::{QuoteMode, TakerRule};
use crate::strategies::signal::{Bracket, MarketKind, Signal};
use crate::strategies::{dispatch, Opportunity};
use crate::util::math::floor_bucket;
use crate::util::time::{climate_date, local_hour};
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketStatus {
    Alive,
    ProvisionallyDead,
}

pub fn classify(kind: MarketKind, b: &Bracket, run_high: f64, run_low: f64) -> BracketStatus {
    let dead = match kind {
        MarketKind::High => b.hi.map_or(false, |h| floor_bucket(run_high) > h),
        MarketKind::Low => b.lo.map_or(false, |l| floor_bucket(run_low) < l),
    };
    if dead {
        BracketStatus::ProvisionallyDead
    } else {
        BracketStatus::Alive
    }
}

/// The continuous-temperature boundary whose crossing kills the bracket (None if it can't die).
pub fn dead_boundary(kind: MarketKind, b: &Bracket) -> Option<f64> {
    match kind {
        MarketKind::High => b.hi.map(|h| (h + 1) as f64),
        MarketKind::Low => b.lo.map(|l| l as f64),
    }
}

#[derive(Debug, Clone)]
pub struct SafetyInputs {
    /// Check 1: rules-text station vs registry (Err carries the reason).
    pub station_rules: std::result::Result<(), String>,
    /// Check 2: the running extreme's local climate day, the market's day, and "today" at the station.
    pub extreme_date: NaiveDate,
    pub market_date: NaiveDate,
    pub today_climate: NaiveDate,
    /// Check 3: latest resolution-source bulletin extreme (same unit), or why it is unavailable.
    pub resolution_extreme: std::result::Result<f64, String>,
    pub metar_extreme: f64,
    pub tolerance: f64,
    /// Check 4.
    pub boundary_distance: f64,
    pub margin: f64,
    /// Check 5.
    pub market_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SafetyFailure {
    StationMismatch(String),
    LocalDayMismatch,
    ResolutionSourceUnavailable(String),
    ResolutionDisagrees { source: f64, metar: f64 },
    BoundaryTooClose(f64),
    MarketNotOpen,
}

impl fmt::Display for SafetyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SafetyFailure::StationMismatch(s) => write!(f, "station mismatch ({s})"),
            SafetyFailure::LocalDayMismatch => write!(f, "running extreme is not from the station-local market day"),
            SafetyFailure::ResolutionSourceUnavailable(s) => write!(f, "resolution source unavailable ({s})"),
            SafetyFailure::ResolutionDisagrees { source, metar } => {
                write!(f, "resolution source {source} disagrees with METAR {metar}")
            }
            SafetyFailure::BoundaryTooClose(d) => write!(f, "boundary within {d:.2} of running extreme"),
            SafetyFailure::MarketNotOpen => write!(f, "market suspended or resolved"),
        }
    }
}

/// SETTLEMENT_SAFETY_CHECK: ALL checks must pass; an empty vec means pass.
pub fn settlement_safety_check(i: &SafetyInputs) -> Vec<SafetyFailure> {
    let mut fails = Vec::new();
    if let Err(e) = &i.station_rules {
        fails.push(SafetyFailure::StationMismatch(e.clone()));
    }
    if !(i.extreme_date == i.market_date && i.market_date == i.today_climate) {
        fails.push(SafetyFailure::LocalDayMismatch);
    }
    match &i.resolution_extreme {
        Err(e) => fails.push(SafetyFailure::ResolutionSourceUnavailable(e.clone())),
        Ok(src) => {
            if (src - i.metar_extreme).abs() > i.tolerance {
                fails.push(SafetyFailure::ResolutionDisagrees { source: *src, metar: i.metar_extreme });
            }
        }
    }
    if i.boundary_distance < i.margin {
        fails.push(SafetyFailure::BoundaryTooClose(i.boundary_distance));
    }
    if !i.market_open {
        fails.push(SafetyFailure::MarketNotOpen);
    }
    fails
}

pub async fn scan(app: &Arc<App>, city_filter: Option<&str>, execute: bool) -> Result<Vec<Signal>> {
    let events = app.events().await?;
    let h = &app.cfg.risk.harvester;
    let now = Utc::now();
    let mut out = Vec::new();
    for ev in events.iter() {
        if let Some(c) = city_filter {
            if !ev.city.eq_ignore_ascii_case(c) {
                continue;
            }
        }
        let (Some(city), Some(st)) = (app.cfg.registry.city_cfg(&ev.city), app.cfg.registry.station(&ev.city)) else {
            continue;
        };
        let today = climate_date(now, city.tz, city.day_basis);
        if ev.date != today || local_hour(now, city.tz) < h.start_hour_local {
            continue;
        }
        let obs = match app.metar.observations(&st.icao).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("metar {}: {e}", st.icao);
                continue;
            }
        };
        let Some(re) = running_extreme(&obs, ev.date, city.tz, city.day_basis, city.unit) else { continue };
        let metar_extreme = if ev.kind == MarketKind::High { re.high } else { re.low };

        for (bracket, m) in &ev.markets {
            if classify(ev.kind, bracket, re.high, re.low) == BracketStatus::Alive {
                continue;
            }
            let mut sig = Signal::new(&ev.city, &st.icao, "no_harvester", &format!("{} NO", bracket.label()));
            sig.settlement_check = "PROVISIONALLY_DEAD".to_string();
            sig.model_prob = Some(h.model_prob);
            let book = match app.clob.book(&m.no_token).await {
                Ok(b) => b,
                Err(_) => {
                    out.push(sig.skip("order book unavailable"));
                    continue;
                }
            };
            let (Some(ask), Some(bid)) = (book.best_ask(), book.best_bid()) else {
                out.push(sig.skip("no two-sided book"));
                continue;
            };
            let (ask, bid) = (ask.price, bid.price);
            sig.market_ask = Some(ask);
            sig.market_bid = Some(bid);
            sig.edge = Some(h.model_prob - ask);
            if ask < h.floor_ask {
                out.push(sig.skip("ask below floor (adverse selection)"));
                continue;
            }
            if ask < h.min_ask || ask > h.max_ask {
                out.push(sig.skip("NO ask outside [min_ask, max_ask]"));
                continue;
            }
            if ask - bid > h.max_spread {
                out.push(sig.skip("spread too wide"));
                continue;
            }
            if ask >= h.model_prob {
                out.push(sig.skip("no edge: ask is at or above the assumed win probability"));
                continue;
            }

            // ---- SETTLEMENT_SAFETY_CHECK ----
            let station_rules = app
                .cfg
                .registry
                .verify_rules(&ev.city, &ev.rules)
                .map(|_| ())
                .map_err(|e| e.to_string());
            let resolution_extreme: std::result::Result<f64, String> = if st.pils.is_empty() {
                Err(format!("no fetchable bulletin for '{}'", st.source))
            } else {
                match app.cli.latest(&st.pils).await {
                    Ok(Some(r)) if r.date == ev.date => {
                        let v = if ev.kind == MarketKind::High { r.max_f } else { r.min_f };
                        v.ok_or_else(|| "bulletin has no extreme yet".to_string())
                    }
                    Ok(Some(r)) => Err(format!("latest bulletin is for {}", r.date)),
                    Ok(None) => Err("no bulletin found".to_string()),
                    Err(e) => Err(e.to_string()),
                }
            };
            let boundary_distance = dead_boundary(ev.kind, bracket)
                .map(|b| (metar_extreme - b).abs())
                .unwrap_or(f64::MAX);
            let inputs = SafetyInputs {
                station_rules,
                extreme_date: re.date,
                market_date: ev.date,
                today_climate: today,
                resolution_extreme,
                metar_extreme,
                tolerance: h.settle_tolerance,
                boundary_distance,
                margin: h.boundary_margin,
                market_open: m.active && !m.closed && m.accepting_orders,
            };
            let fails = settlement_safety_check(&inputs);
            if !fails.is_empty() {
                let why = fails.iter().map(|f| f.to_string()).collect::<Vec<_>>().join("; ");
                sig.settlement_check = format!("SETTLEMENT_UNSAFE: {why}");
                if app.should_log(&format!("unsafe|{}", m.no_token), 600) {
                    app.event("INFO", "SETTLEMENT_UNSAFE", &format!("{} {} {}: {why}", ev.city, ev.date, bracket.label()));
                    let _ = app.storage.insert_signal(&sig);
                }
                out.push(sig.skip("SETTLEMENT_UNSAFE"));
                continue;
            }
            sig.settlement_check = "PASS".to_string();
            let sig = sig.trade("provisionally dead, settlement check passed");
            if execute {
                let signal_id = app.storage.insert_signal(&sig).unwrap_or(0);
                let op = Opportunity {
                    strategy: "no_harvester",
                    market_id: m.id.clone(),
                    outcome_idx: 1 - m.yes_idx,
                    city: ev.city.clone(),
                    event_key: ev.key(),
                    bracket: sig.bracket.clone(),
                    token_id: m.no_token.clone(),
                    outcome: "NO",
                    neg_risk: m.neg_risk,
                    tick: m.tick,
                    fee: m.fee.clone(),
                    p: h.model_prob,
                    ask,
                    book,
                    fair_fn: None,
                    edge_floor: Decimal::ZERO,
                    hard_cap: h.max_ask,
                    mode: QuoteMode::AskMinusTick,
                    taker: TakerRule::MinAsk { min: h.taker_min_ask, max: h.max_ask },
                    maker_timeout: Duration::from_secs(h.maker_timeout_secs),
                    max_spread: h.max_spread,
                    kelly_mult: h.kelly_mult,
                    cap_pct: h.cap_pct,
                    min_shares: h.min_shares,
                    depth_ceiling: h.model_prob.min(h.max_ask),
                };
                dispatch(app, &op, signal_id);
            }
            out.push(sig);
        }
    }
    Ok(out)
}
