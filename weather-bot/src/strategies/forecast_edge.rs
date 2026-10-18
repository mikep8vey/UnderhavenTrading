//! Strategy 1 - FORECAST EDGE (morning/midday).
//!
//! raw ensemble bucket probability -> calibrated (learned from settled markets) -> shrunk toward the market's YES mid:
//!     p_yes = w * calibrated + (1 - w) * market_mid
//! edge = p_side - ask - taker_fee(ask); trade only if edge > min_edge AND spread < max_spread. Depth, sizing and risk
//! caps are applied per account in `dispatch`, so paper and live act on the same edge and probability.
use crate::app::App;
use crate::data::ensemble::bucket_prob;
use crate::error::Result;
use crate::execution::maker_layer::{FairFn, QuoteMode, TakerRule};
use crate::polymarket::clob::Book;
use crate::polymarket::gamma::{Market, WeatherEvent};
use crate::strategies::calibration::shrink;
use crate::strategies::signal::{Bracket, Signal};
use crate::strategies::{dispatch, Opportunity};
use crate::util::math::clamp_prob;
use crate::util::time::{climate_date, local_hour};
use chrono::{Duration as CDur, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;

const MIN_MEMBERS: usize = 20;

struct Cand {
    outcome: &'static str,
    token: String,
    outcome_idx: usize,
    p: Decimal,
    ask: Decimal,
    bid: Decimal,
    edge: Decimal,
    book: Book,
}

fn cand(outcome: &'static str, token: &str, idx: usize, p: Decimal, book: Book, m: &Market) -> Option<Cand> {
    let ask = book.best_ask()?.price;
    let bid = book.best_bid()?.price;
    let fee = m.fee.as_ref().map(|f| f.fee_per_share(ask)).unwrap_or(Decimal::ZERO);
    Some(Cand { outcome, token: token.to_string(), outcome_idx: idx, p, ask, bid, edge: p - ask - fee, book })
}

pub async fn scan(app: &Arc<App>, city_filter: Option<&str>, execute: bool) -> Result<Vec<Signal>> {
    let events = app.events().await?;
    let cfg = &app.cfg.risk.forecast;
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
        if ev.date < today || ev.date > today + CDur::days(1) {
            continue;
        }
        if ev.date == today && local_hour(now, city.tz) >= cfg.cutoff_hour_local {
            continue;
        }
        if let Err(m) = app.cfg.registry.verify_rules(&ev.city, &ev.rules) {
            let msg = format!("{} {}: {m}", ev.city, ev.date);
            if app.should_log(&format!("manual|{}", ev.key()), 600) {
                app.event("WARN", "MANUAL_REVIEW_REQUIRED", &msg);
            }
            let sig = Signal::new(&ev.city, &st.icao, "forecast_edge", "*").skip(&format!("MANUAL_REVIEW_REQUIRED: {m}"));
            let _ = app.storage.insert_signal(&sig);
            out.push(sig);
            continue;
        }
        let members = match app.members(ev).await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("ensemble {} {}: {e}", ev.city, ev.date);
                continue;
            }
        };
        if members.len() < MIN_MEMBERS {
            continue;
        }
        let phase = if ev.date == today { "today" } else { "tomorrow" };
        for (bracket, m) in &ev.markets {
            let (sig, opp) = eval_market(app, ev, &st.icao, &members, bracket, m, phase).await;
            let persist = sig.decision == "TRADE"
                || sig.edge.map_or(false, |e| e > cfg.min_edge)
                || sig.reason.starts_with("MANUAL_REVIEW");
            let id = if persist { app.storage.insert_signal(&sig).unwrap_or(0) } else { 0 };
            if let (true, Some(op)) = (execute, opp.as_ref()) {
                dispatch(app, op, id);
            }
            out.push(sig);
        }
    }
    Ok(out)
}

async fn eval_market(
    app: &Arc<App>,
    ev: &WeatherEvent,
    icao: &str,
    members: &Arc<Vec<f64>>,
    bracket: &Bracket,
    m: &Market,
    phase: &str,
) -> (Signal, Option<Opportunity>) {
    let risk = &app.cfg.risk;
    let f = &risk.forecast;
    let sig = Signal::new(&ev.city, icao, "forecast_edge", &bracket.label());
    if !m.active || m.closed || !m.accepting_orders {
        return (sig.skip("market not tradable"), None);
    }
    let p_raw = clamp_prob(bucket_prob(members, bracket));
    let (yb, nb) = tokio::join!(app.clob.book(&m.yes_token), app.clob.book(&m.no_token));
    let (yb, nb) = match (yb, nb) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return (sig.skip("order book unavailable"), None),
    };
    let p_mkt = match yb.mid().or_else(|| yb.best_ask().map(|a| a.price)) {
        Some(x) => x,
        None => return (sig.skip("no YES quote"), None),
    };
    // Log the raw forecast once per market and phase: this is what calibration learns from after settlement.
    let _ = app.storage.log_forecast(
        &m.id,
        phase,
        &ev.city,
        &bracket.label(),
        &ev.date.to_string(),
        m.yes_idx,
        p_raw.to_f64().unwrap_or(0.5),
        p_mkt.to_f64().unwrap_or(0.5),
    );
    let p_cal = app.calibrate(p_raw);
    let p_yes = clamp_prob(shrink(p_cal, p_mkt, f.shrink_weight));

    let yes = cand("YES", &m.yes_token, m.yes_idx, p_yes, yb, m);
    let no = cand("NO", &m.no_token, 1 - m.yes_idx, Decimal::ONE - p_yes, nb, m);
    let c = match (yes, no) {
        (Some(a), Some(b)) => {
            if a.edge >= b.edge {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return (sig.skip("no two-sided book"), None),
    };
    let mut sig = sig;
    sig.bracket = format!("{} {}", bracket.label(), c.outcome);
    sig.model_prob = Some(c.p);
    sig.market_ask = Some(c.ask);
    sig.market_bid = Some(c.bid);
    sig.edge = Some(c.edge);
    if m.fee.is_none() {
        sig.settlement_check = "fee schedule missing: maker-only".to_string();
    }
    let detail = format!("raw {:.3} cal {:.3} mkt {:.3} -> yes {:.3}", p_raw, p_cal, p_mkt, p_yes);
    if c.edge <= f.min_edge {
        return (sig.skip(&format!("edge {:.3} <= {} ({detail})", c.edge, f.min_edge)), None);
    }
    if c.ask < f.min_entry_price {
        return (sig.skip(&format!("ask {:.3} below minimum entry price {} (lottery ticket)", c.ask, f.min_entry_price)), None);
    }
    if c.outcome == "NO" && p_mkt > f.no_max_yes_mid {
        return (
            sig.skip(&format!("favourite guard: market gives YES {:.2} > {} ({detail})", p_mkt, f.no_max_yes_mid)),
            None,
        );
    }
    if c.ask - c.bid >= f.max_spread {
        return (sig.skip(&format!("spread {:.3} >= {}", c.ask - c.bid, f.max_spread)), None);
    }

    let app2 = app.clone();
    let key = ev.key();
    let br = *bracket;
    let want_yes = c.outcome == "YES";
    let w = f.shrink_weight;
    let pm = p_mkt;
    let fair_fn: FairFn = Arc::new(move || {
        let mem = app2.cached_members(&key)?;
        let p = clamp_prob(shrink(app2.calibrate(clamp_prob(bucket_prob(&mem, &br))), pm, w));
        Some(if want_yes { p } else { Decimal::ONE - p })
    });
    let op = Opportunity {
        strategy: "forecast_edge",
        market_id: m.id.clone(),
        outcome_idx: c.outcome_idx,
        city: ev.city.clone(),
        event_key: ev.key(),
        bracket: sig.bracket.clone(),
        token_id: c.token.clone(),
        outcome: c.outcome,
        neg_risk: m.neg_risk,
        tick: m.tick,
        fee: m.fee.clone(),
        p: c.p,
        ask: c.ask,
        book: c.book,
        fair_fn: Some(fair_fn),
        edge_floor: f.min_edge,
        hard_cap: Decimal::new(99, 2),
        mode: QuoteMode::ImproveBid,
        taker: TakerRule::MinEdge(f.taker_min_edge),
        maker_timeout: Duration::from_secs(f.maker_timeout_secs),
        max_spread: f.max_spread,
        kelly_mult: f.kelly_mult,
        cap_pct: f.cap_pct,
        min_shares: f.min_shares,
        depth_ceiling: (c.p - f.min_edge).min(Decimal::new(99, 2)),
    };
    (sig.trade(&format!("edge, spread gates passed ({detail})")), Some(op))
}
