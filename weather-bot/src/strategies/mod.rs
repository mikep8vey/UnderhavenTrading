pub mod calibration;
pub mod forecast_edge;
pub mod no_harvester;
pub mod signal;

use crate::app::{Account, App};
use crate::execution::maker_layer::{ExecOutcome, FairFn, QuoteMode, TakerRule, TradeIntent};
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
    /// Polymarket condition id (used by the paper broker's public-trade-tape fill model).
    pub condition_id: String,
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

fn note(app: &App, signal_id: i64, acct: &Account, status: &str, detail: &str) {
    if signal_id > 0 {
        let _ = app.storage.set_signal_exec(signal_id, acct.name, status, detail);
    }
}

fn account_skip(app: &App, acct: &Account, op: &Opportunity, signal_id: i64, status: &str, reason: &str) {
    note(app, signal_id, acct, status, reason);
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
            note(app, signal_id, acct, "SKIPPED_HALT", "account halted");
            continue;
        }
        if !acct.can_attempt(&op.token_id, rc.max_attempts_per_market, rc.retry_cooldown_secs) {
            note(app, signal_id, acct, "SKIPPED_RETRY", "attempt limit or retry cooldown");
            continue;
        }
        let equity = lock(&acct.exposure).equity();
        let stake = stake_usdc(op.p, op.ask, op.kelly_mult, op.cap_pct, equity);
        let shares = floor_dp(stake / op.ask, 2);
        if shares < op.min_shares {
            account_skip(app, acct, op, signal_id, "SKIPPED_MIN_SIZE", "kelly size below minimum order size");
            continue;
        }
        // The first order is always post-only. Ask depth is only relevant when the optional taker
        // fallback crosses the spread (checked in maker_layer). 0.0 disables this maker gate entirely.
        if rc.execution.maker_depth_multiple > Decimal::ZERO
            && op.book.ask_depth_up_to(op.depth_ceiling) < shares * rc.execution.maker_depth_multiple
        {
            account_skip(app, acct, op, signal_id, "SKIPPED_DEPTH", "insufficient maker-side depth");
            continue;
        }
        let reserved = lock(&acct.exposure).try_reserve(rc, &op.city, &op.event_key, &op.token_id, shares * op.ask);
        let reservation = match reserved {
            Ok(id) => id,
            Err(e) => {
                account_skip(app, acct, op, signal_id, "SKIPPED_RISK", &format!("risk: {e}"));
                continue;
            }
        };
        let intent = TradeIntent {
            signal_id,
            strategy: op.strategy,
            market_id: op.market_id.clone(),
            condition_id: op.condition_id.clone(),
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
        note(app, signal_id, acct, "ORDERED", "maker quote working");
        let a = app.clone();
        let acct2 = acct.clone();
        let token = op.token_id.clone();
        let label = format!("{} {} {} {}", op.strategy, op.city, op.bracket, op.outcome);
        tokio::spawn(async move {
            let res = acct2.maker.execute(intent).await;
            acct2.note_result(&token, &res);
            let (status, detail) = match &res {
                ExecOutcome::Filled { size, avg_price } => ("FILLED", format!("filled {size} @ {avg_price}")),
                ExecOutcome::Partial { size, avg_price } => ("PARTIAL", format!("partial {size} @ {avg_price}")),
                ExecOutcome::Unfilled(why) => ("UNFILLED", why.clone()),
                ExecOutcome::Halted { size } => ("HALTED", format!("halted after {size}")),
                ExecOutcome::Failed(why) => ("FAILED", why.clone()),
            };
            let _ = a.storage.set_signal_exec(signal_id, acct2.name, status, &detail);
            a.event("INFO", "EXEC_DONE", &format!("[{}] {label}: {res:?}", acct2.name));
        });
    }
}
