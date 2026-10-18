pub mod calibration;
pub mod forecast_edge;
pub mod no_harvester;
pub mod signal;

use crate::app::{Account, App};
use crate::execution::maker_layer::{FairFn, QuoteMode, TakerRule, TradeIntent};
use crate::polymarket::clob::Book;
use crate::polymarket::gamma::FeeSchedule;
use crate::risk::kelly::stake_usdc;
use crate::util::lock;
use crate::util::math::floor_dp;
use rust_decimal::Decimal;
use std::sync::Arc;
use std::time::Duration;

/// One qualified trade idea. The decision (side, probability, edge, price) is made ONCE and is identical for the paper
/// and the live account; only the size differs, because each account sizes from its own equity and risk book.
pub struct Opportunity {
    pub strategy: &'static str,
    pub market_id: String,
    pub outcome_idx: usize,
    pub city: String,
    pub event_key: String,
    pub bracket: String,
    pub token_id: String,
    pub outcome: &'static str,
    pub neg_risk: bool,
    pub tick: Decimal,
    pub fee: Option<FeeSchedule>,
    /// Probability the chosen side wins (after calibration and shrinkage for the forecast strategy).
    pub p: Decimal,
    pub ask: Decimal,
    pub book: Book,
    pub fair_fn: Option<FairFn>,
    pub edge_floor: Decimal,
    pub hard_cap: Decimal,
    pub mode: QuoteMode,
    pub taker: TakerRule,
    pub maker_timeout: Duration,
    pub max_spread: Decimal,
    pub kelly_mult: Decimal,
    pub cap_pct: Decimal,
    pub min_shares: Decimal,
    pub depth_ceiling: Decimal,
}

fn account_skip(app: &App, acct: &Account, op: &Opportunity, reason: &str) {
    if app.should_log(&format!("skip|{}|{}", acct.name, op.token_id), 900) {
        app.event(
            "INFO",
            "ACCOUNT_SKIP",
            &format!("[{}] {} {} {}: {reason}", acct.name, op.city, op.bracket, op.strategy),
        );
    }
}

/// Send an opportunity to every account (paper always; live when enabled). Each account applies its own halt state,
/// retry limit, Kelly sizing, depth check and exposure caps, then works the order through its own maker layer.
pub fn dispatch(app: &Arc<App>, op: &Opportunity, signal_id: i64) {
    let rc = &app.cfg.risk;
    for acct in app.accounts.iter() {
        if acct.halt.is_halted() {
            continue;
        }
        if !acct.can_attempt(&op.token_id, rc.max_attempts_per_market, rc.retry_cooldown_secs) {
            continue;
        }
        let equity = lock(&acct.exposure).equity();
        let stake = stake_usdc(op.p, op.ask, op.kelly_mult, op.cap_pct, equity);
        let shares = floor_dp(stake / op.ask, 2);
        if shares < op.min_shares {
            account_skip(app, acct, op, "kelly size below minimum order size");
            continue;
        }
        if op.book.ask_depth_up_to(op.depth_ceiling) < shares * rc.execution.depth_multiple {
            account_skip(app, acct, op, "insufficient depth");
            continue;
        }
        let reserved = lock(&acct.exposure).try_reserve(rc, &op.city, &op.event_key, &op.token_id, shares * op.ask);
        let reservation = match reserved {
            Ok(id) => id,
            Err(e) => {
                account_skip(app, acct, op, &format!("risk: {e}"));
                continue;
            }
        };
        let intent = TradeIntent {
            signal_id,
            strategy: op.strategy,
            market_id: op.market_id.clone(),
            outcome_idx: op.outcome_idx,
            city: op.city.clone(),
            event_key: op.event_key.clone(),
            bracket: op.bracket.clone(),
            token_id: op.token_id.clone(),
            outcome: op.outcome,
            neg_risk: op.neg_risk,
            tick: op.tick,
            fee: op.fee.clone(),
            size: shares,
            fair: op.p,
            fair_fn: op.fair_fn.clone(),
            edge_floor: op.edge_floor,
            hard_cap: op.hard_cap,
            mode: op.mode,
            taker: op.taker.clone(),
            maker_timeout: op.maker_timeout,
            max_spread: op.max_spread,
            reservation,
        };
        let a = app.clone();
        let acct2 = acct.clone();
        let token = op.token_id.clone();
        let label = format!("{} {} {} {}", op.strategy, op.city, op.bracket, op.outcome);
        tokio::spawn(async move {
            let res = acct2.maker.execute(intent).await;
            acct2.note_result(&token, &res);
            a.event("INFO", "EXEC_DONE", &format!("[{}] {label}: {res:?}", acct2.name));
        });
    }
}
