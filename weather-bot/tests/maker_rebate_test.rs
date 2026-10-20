use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;
use weather_bot::execution::maker_layer::*;
use weather_bot::execution::order_builder::{two_sided_sum_ok, OrderBuilder, OrderType};
use weather_bot::polymarket::gamma::FeeSchedule;

fn fee() -> FeeSchedule {
    FeeSchedule { rate: dec!(0.05), exponent: 1, rebate_rate: dec!(0.25) }
}

#[test]
fn fee_curve_and_rebate_share() {
    // 0.5 * 0.05 * (0.5*0.5) = 0.00625 per share
    assert_eq!(fee().fee_per_share(dec!(0.5)), dec!(0.00625));
    assert_eq!(fee().fee(dec!(0.5), dec!(100)), dec!(0.625));
    // maker rebate = 25% of the taker fee pool slice
    assert_eq!(fee().maker_rebate(dec!(0.5), dec!(100)), dec!(0.15625));
    assert_eq!(fee().fee_per_share(dec!(0.0)), Decimal::ZERO);
}

#[test]
fn orders_are_post_only_unless_taker_is_enabled() {
    let b = OrderBuilder::new(false);
    let m = b.maker("123", "0xcond", dec!(0.457), dec!(10.999), false, dec!(0.01));
    assert!(m.post_only);
    assert_eq!(m.order_type, OrderType::Gtc);
    assert_eq!(m.price, dec!(0.45));
    assert_eq!(m.size, dec!(10.99));
    assert_eq!(m.condition_id, "0xcond");
    assert!(b.taker("123", "0xcond", dec!(0.5), dec!(10), false, dec!(0.01)).is_err());

    let t = OrderBuilder::new(true)
        .taker("123", "0xcond", dec!(0.5), dec!(10), false, dec!(0.01))
        .expect("allowed");
    assert!(!t.post_only);
    assert_eq!(t.order_type, OrderType::Fak);
}

#[test]
fn maker_prices_never_cross() {
    let tick = dec!(0.01);
    assert_eq!(maker_price(QuoteMode::ImproveBid, Some(dec!(0.50)), dec!(0.53), dec!(0.99), tick), Some(dec!(0.51)));
    // one-tick spread: join the bid instead of crossing
    assert_eq!(maker_price(QuoteMode::ImproveBid, Some(dec!(0.50)), dec!(0.51), dec!(0.99), tick), Some(dec!(0.50)));
    assert_eq!(maker_price(QuoteMode::AskMinusTick, None, dec!(0.90), dec!(0.98), tick), Some(dec!(0.89)));
    // ceiling clamps the quote
    assert_eq!(maker_price(QuoteMode::ImproveBid, Some(dec!(0.50)), dec!(0.53), dec!(0.40), tick), Some(dec!(0.40)));
    assert_eq!(maker_price(QuoteMode::ImproveBid, None, dec!(0.53), dec!(0.99), tick), None);
}

#[test]
fn taker_fallback_is_fee_aware() {
    let f = fee();
    // fair 0.70, ask 0.55: edge 0.15 - fee(0.55)=0.0069 => 0.1431 > 0.10
    assert!(taker_allowed(&TakerRule::MinEdge(dec!(0.10)), dec!(0.70), dec!(0.55), Some(&f)));
    assert!(!taker_allowed(&TakerRule::MinEdge(dec!(0.10)), dec!(0.70), dec!(0.61), Some(&f)));
    // unknown fee schedule => never cross on the edge rule
    assert!(!taker_allowed(&TakerRule::MinEdge(dec!(0.10)), dec!(0.90), dec!(0.10), None));
    let h = TakerRule::MinAsk { min: dec!(0.85), max: dec!(0.99) };
    assert!(taker_allowed(&h, dec!(0.98), dec!(0.90), None));
    assert!(!taker_allowed(&h, dec!(0.98), dec!(0.80), None));
}

#[test]
fn reprice_only_on_price_change_or_hard_ttl() {
    let max = Duration::from_secs(600);
    // no resting quote -> post
    assert!(should_reprice(false, dec!(0.6), dec!(0.6), Duration::ZERO, max));
    // same price, young quote -> keep resting (this is the churn fix)
    assert!(!should_reprice(true, dec!(0.60), dec!(0.60), Duration::from_secs(90), max));
    // the desired price moved by one tick -> replace
    assert!(should_reprice(true, dec!(0.60), dec!(0.61), Duration::from_secs(3), max));
    assert!(should_reprice(true, dec!(0.60), dec!(0.59), Duration::from_secs(3), max));
    // hard TTL -> replace even with no price change
    assert!(should_reprice(true, dec!(0.60), dec!(0.60), Duration::from_secs(600), max));
    assert!(!should_reprice(true, dec!(0.60), dec!(0.60), Duration::from_secs(599), max));
}

#[test]
fn two_sided_quotes_must_not_sum_above_99() {
    assert!(two_sided_sum_ok(dec!(0.48), dec!(0.50)));
    assert!(!two_sided_sum_ok(dec!(0.50), dec!(0.50)));
}
