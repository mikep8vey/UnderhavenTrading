use crate::config::Unit;
use rust_decimal::prelude::*;

/// Bracket assignment ALWAYS uses floor(): 23.9 resolves in bucket 23.
pub fn floor_bucket(t: f64) -> i32 {
    t.floor() as i32
}

pub fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

pub fn c_to_unit(c: f64, unit: Unit) -> f64 {
    match unit {
        Unit::C => c,
        Unit::F => c_to_f(c),
    }
}

pub fn dec_from_f64(x: f64) -> Decimal {
    Decimal::from_f64(x).unwrap_or(Decimal::ZERO)
}

pub fn parse_dec(s: &str) -> Option<Decimal> {
    Decimal::from_str(s.trim()).ok()
}

/// Decimal from a JSON number or numeric string.
pub fn json_dec(v: &serde_json::Value) -> Option<Decimal> {
    match v {
        serde_json::Value::String(s) => parse_dec(s),
        serde_json::Value::Number(n) => {
            Decimal::from_str(&n.to_string()).ok().or_else(|| n.as_f64().and_then(Decimal::from_f64))
        }
        _ => None,
    }
}

/// Round DOWN to a multiple of `step` (used for tick sizes).
pub fn floor_to(x: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return x;
    }
    (x / step).floor() * step
}

/// Truncate toward zero to `dp` decimal places (used for share sizes).
pub fn floor_dp(x: Decimal, dp: u32) -> Decimal {
    x.round_dp_with_strategy(dp, RoundingStrategy::ToZero)
}

pub fn clamp_prob(p: Decimal) -> Decimal {
    let lo = Decimal::new(1, 2);
    let hi = Decimal::new(99, 2);
    if p < lo {
        lo
    } else if p > hi {
        hi
    } else {
        p
    }
}
