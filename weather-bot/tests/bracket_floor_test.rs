use weather_bot::strategies::signal::Bracket;
use weather_bot::util::math::floor_bucket;

#[test]
fn a_23_9_reading_resolves_in_bucket_23() {
    assert_eq!(floor_bucket(23.9), 23);
    let b = Bracket::parse("23\u{b0}C").expect("parse");
    assert!(b.contains_temp(23.9));
    assert!(!b.contains_temp(24.0));
    assert!(!b.contains_temp(22.99));
}

#[test]
fn negative_temperatures_floor_downward() {
    assert_eq!(floor_bucket(-0.1), -1);
    let b = Bracket { lo: Some(-1), hi: Some(-1) };
    assert!(b.contains_temp(-0.1));
    assert!(!b.contains_temp(0.0));
}

#[test]
fn bracket_parsing() {
    assert_eq!(Bracket::parse("70-71\u{b0}F"), Some(Bracket { lo: Some(70), hi: Some(71) }));
    assert_eq!(Bracket::parse("69\u{b0}F or below"), Some(Bracket { lo: None, hi: Some(69) }));
    assert_eq!(Bracket::parse("78\u{b0}F or higher"), Some(Bracket { lo: Some(78), hi: None }));
    assert_eq!(Bracket::parse("23\u{b0}C"), Some(Bracket { lo: Some(23), hi: Some(23) }));
    assert_eq!(Bracket::parse("no numbers here"), None);
    let r = Bracket::parse("70-71\u{b0}F").expect("parse");
    assert!(r.contains_temp(71.99));
    assert!(!r.contains_temp(72.0));
}
