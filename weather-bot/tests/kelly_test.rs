use rust_decimal_macros::dec;
use weather_bot::risk::kelly::*;

#[test]
fn kelly_formula() {
    // price 0.5 => b = 1; p = 0.6 => f = (0.6 - 0.4) / 1 = 0.2
    assert_eq!(kelly_full(dec!(0.6), dec!(0.5)), dec!(0.2));
    // no edge or negative edge => zero
    assert_eq!(kelly_full(dec!(0.5), dec!(0.5)), dec!(0));
    assert_eq!(kelly_full(dec!(0.4), dec!(0.5)), dec!(0));
    // degenerate prices
    assert_eq!(kelly_full(dec!(0.9), dec!(1)), dec!(0));
    assert_eq!(kelly_full(dec!(0.9), dec!(0)), dec!(0));
}

#[test]
fn fractional_kelly_and_caps() {
    // 0.2 * 0.25 = 0.05 => exactly at the 5% cap => 50 on a 1000 bankroll
    assert_eq!(stake_usdc(dec!(0.6), dec!(0.5), dec!(0.25), dec!(0.05), dec!(1000)), dec!(50));
    // huge edge: cap binds
    assert_eq!(stake_usdc(dec!(0.9), dec!(0.5), dec!(0.25), dec!(0.05), dec!(1000)), dec!(50));
    // small edge: fraction binds. f = 0.1 * 0.25 = 0.025
    assert_eq!(stake_usdc(dec!(0.55), dec!(0.5), dec!(0.25), dec!(0.05), dec!(1000)), dec!(25));
    // harvester cap 8%
    assert_eq!(stake_usdc(dec!(0.98), dec!(0.90), dec!(0.15), dec!(0.08), dec!(1000)), dec!(80));
    assert_eq!(stake_usdc(dec!(0.5), dec!(0.5), dec!(0.25), dec!(0.05), dec!(1000)), dec!(0));
}

#[test]
fn share_sizing_truncates() {
    assert_eq!(shares_for(dec!(50), dec!(0.30)), dec!(166.66));
}
