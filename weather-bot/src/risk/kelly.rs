use crate::util::math::floor_dp;
use rust_decimal::Decimal;

/// Full Kelly for buying a binary share at `price` with win probability `p`:
/// f = (b*p - q) / b, b = (1 - price) / price, q = 1 - p. Never negative.
pub fn kelly_full(p: Decimal, price: Decimal) -> Decimal {
    if price <= Decimal::ZERO || price >= Decimal::ONE || p <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let b = (Decimal::ONE - price) / price;
    let q = Decimal::ONE - p;
    let f = (b * p - q) / b;
    if f > Decimal::ZERO {
        f
    } else {
        Decimal::ZERO
    }
}

/// Fractional Kelly stake in USDC, capped at `cap_pct` of bankroll.
pub fn stake_usdc(p: Decimal, price: Decimal, mult: Decimal, cap_pct: Decimal, bankroll: Decimal) -> Decimal {
    let f = (kelly_full(p, price) * mult).min(cap_pct);
    if f <= Decimal::ZERO || bankroll <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    f * bankroll
}

/// Shares purchasable for a stake (truncated to 2 dp).
pub fn shares_for(stake: Decimal, price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    floor_dp(stake / price, 2)
}
