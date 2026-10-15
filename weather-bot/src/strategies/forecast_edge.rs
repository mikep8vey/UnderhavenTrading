//! Strategy 1 - FORECAST EDGE (morning/midday).
//! Edge = P_model - ask - taker_fee(ask). Trade only if Edge > min_edge AND spread < max_spread AND
//! depth >= 3x size. Orders are worked by the shared maker layer (post-only first).
use crate::app::App;
use crate::data::ensemble::bucket_prob;
use crate::error::Result;
use crate::execution::maker_layer::{FairFn, QuoteMode, TakerRule, TradeIntent};
use crate::polymarket::clob::Book;
use crate::polymarket::gamma::{Market, WeatherEvent};
use crate::risk::kelly::stake_usdc;
use crate::strategies::signal::{Bracket, Signal};
use crate::util::lock;
use crate::util::math::{clamp_prob, floor_dp};
use crate::util::time::{climate_date, local_hour};
use chrono::{Duration as CDur, Utc};
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
        for (bracket, m) in &ev.markets {
            let (sig, intent) = eval_market(app, ev, &st.icao, &members, bracket, m, execute).await;
            let persist = sig.decision == "TRADE"
                || sig.edge.map_or(false, |e| e > Decimal::ZERO)
                || sig.reason.starts_with("MANUAL_REVIEW");
            let id = if persist { app.storage.insert_signal(&sig).unwrap_or(0) } else { 0 };
            if let Some(mut it) = intent {
                it.signal_id = id;
                let a = app.clone();
                tokio::spawn(async move {
                    let label = format!("{} {} {}", it.city, it.bracket, it.outcome);
                    let res = a.maker.execute(it).await;
                    a.event("INFO", "EXEC_DONE", &format!("forecast_edge {label}: {res:?}"));
                });
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
    execute: bool,
) -> (Signal, Option<TradeIntent>) {
    let risk = &app.cfg.risk;
    let f = &risk.forecast;
    let sig = Signal::new(&ev.city, icao, "forecast_edge", &bracket.label());
    if !m.active || m.closed || !m.accepting_orders {
        return (sig.skip("market not tradable"), None);
    }
    let p_yes = clamp_prob(bucket_prob(members, bracket));
    let (yb, nb) = tokio::join!(app.clob.book(&m.yes_token), app.clob.book(&m.no_token));
    let (yb, nb) = match (yb, nb) {
        (Ok(a), Ok(b)) => (a, b),
        _ => return (sig.skip("order book unavailable"), None),
    };
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
    if c.edge <= f.min_edge {
        return (sig.skip(&format!("edge {:.3} <= {}", c.edge, f.min_edge)), None);
    }
    if c.ask - c.bid >= f.max_spread {
        return (sig.skip(&format!("spread {:.3} >= {}", c.ask - c.bid, f.max_spread)), None);
    }
    let equity = lock(&app.exposure).equity();
    let stake = stake_usdc(c.p, c.ask, f.kelly_mult, f.cap_pct, equity);
    let shares = floor_dp(stake / c.ask, 2);
    if shares < f.min_shares {
        return (sig.skip("kelly size below minimum order size"), None);
    }
    let ceiling = (c.p - f.min_edge).min(Decimal::new(99, 2));
    if c.book.ask_depth_up_to(ceiling) < shares * risk.execution.depth_multiple {
        return (sig.skip("insufficient depth"), None);
    }
    if !execute {
        return (sig.trade("would trade (dry evaluation)"), None);
    }
    let reservation = match lock(&app.exposure).try_reserve(risk, &ev.city, &ev.key(), &c.token, shares * c.ask) {
        Ok(id) => id,
        Err(e) => return (sig.skip(&format!("risk: {e}")), None),
    };
    let app2 = app.clone();
    let key = ev.key();
    let br = *bracket;
    let want_yes = c.outcome == "YES";
    let fair_fn: FairFn = Arc::new(move || {
        let mem = app2.cached_members(&key)?;
        let p = clamp_prob(bucket_prob(&mem, &br));
        Some(if want_yes { p } else { Decimal::ONE - p })
    });
    let intent = TradeIntent {
        signal_id: 0,
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
        size: shares,
        fair: c.p,
        fair_fn: Some(fair_fn),
        edge_floor: f.min_edge,
        hard_cap: Decimal::new(99, 2),
        mode: QuoteMode::ImproveBid,
        taker: TakerRule::MinEdge(f.taker_min_edge),
        maker_timeout: Duration::from_secs(f.maker_timeout_secs),
        max_spread: f.max_spread,
        reservation,
    };
    (sig.trade("edge, spread and depth gates passed"), Some(intent))
}
