use chrono::NaiveDate;
use weather_bot::strategies::no_harvester::*;
use weather_bot::strategies::signal::{Bracket, MarketKind};

fn d(day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, day).expect("date")
}

fn good() -> SafetyInputs {
    SafetyInputs {
        station_rules: Ok(()),
        extreme_date: d(15),
        market_date: d(15),
        today_climate: d(15),
        resolution_extreme: Ok(72.0),
        metar_extreme: 72.3,
        tolerance: 0.5,
        boundary_distance: 1.3,
        margin: 0.5,
        market_open: true,
    }
}

#[test]
fn high_bracket_dies_only_after_the_floor_crosses_its_max() {
    let b = Bracket { lo: Some(70), hi: Some(71) };
    assert_eq!(classify(MarketKind::High, &b, 71.9, 50.0), BracketStatus::Alive);
    assert_eq!(classify(MarketKind::High, &b, 72.0, 50.0), BracketStatus::ProvisionallyDead);
    // exactly at hi is still alive (the day's max can still land in the bracket)
    assert_eq!(classify(MarketKind::High, &Bracket { lo: Some(70), hi: Some(72) }, 72.0, 50.0), BracketStatus::Alive);
    // open-ended top bracket can never die on the high side
    assert_eq!(classify(MarketKind::High, &Bracket { lo: Some(78), hi: None }, 99.0, 50.0), BracketStatus::Alive);
}

#[test]
fn low_bracket_dies_when_running_low_drops_below_its_min() {
    let b = Bracket { lo: Some(40), hi: Some(41) };
    assert_eq!(classify(MarketKind::Low, &b, 60.0, 40.0), BracketStatus::Alive);
    assert_eq!(classify(MarketKind::Low, &b, 60.0, 39.5), BracketStatus::ProvisionallyDead);
    assert_eq!(dead_boundary(MarketKind::Low, &b), Some(40.0));
    assert_eq!(dead_boundary(MarketKind::High, &b), Some(42.0));
}

#[test]
fn all_checks_pass_on_clean_inputs() {
    assert!(settlement_safety_check(&good()).is_empty());
}

#[test]
fn each_check_can_fail_independently() {
    let mut i = good();
    i.station_rules = Err("JFK".into());
    assert!(matches!(settlement_safety_check(&i)[..], [SafetyFailure::StationMismatch(_)]));

    let mut i = good();
    i.extreme_date = d(14);
    assert_eq!(settlement_safety_check(&i), vec![SafetyFailure::LocalDayMismatch]);

    let mut i = good();
    i.resolution_extreme = Err("no bulletin".into());
    assert!(matches!(settlement_safety_check(&i)[..], [SafetyFailure::ResolutionSourceUnavailable(_)]));

    let mut i = good();
    i.resolution_extreme = Ok(74.0);
    assert!(matches!(settlement_safety_check(&i)[..], [SafetyFailure::ResolutionDisagrees { .. }]));

    let mut i = good();
    i.boundary_distance = 0.2;
    assert!(matches!(settlement_safety_check(&i)[..], [SafetyFailure::BoundaryTooClose(_)]));

    let mut i = good();
    i.market_open = false;
    assert_eq!(settlement_safety_check(&i), vec![SafetyFailure::MarketNotOpen]);
}
