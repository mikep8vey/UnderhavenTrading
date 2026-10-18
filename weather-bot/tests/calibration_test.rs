use rust_decimal_macros::dec;
use weather_bot::strategies::calibration::*;

#[test]
fn calibration_is_identity_until_enough_forecasts_settled() {
    let rows: Vec<(f64, bool)> = (0..10).map(|_| (0.8, false)).collect();
    let c = Calibrator::fit(&rows, 100, 20.0);
    assert!(!c.active());
    assert_eq!(c.apply(0.8), 0.8);
}

#[test]
fn an_overconfident_bucket_is_pulled_toward_what_actually_happened() {
    // 200 forecasts at 0.85, only half won: (100 + 20*0.85) / (200 + 20) = 0.5318
    let rows: Vec<(f64, bool)> = (0..200).map(|i| (0.85, i % 2 == 0)).collect();
    let c = Calibrator::fit(&rows, 100, 20.0);
    assert!(c.active());
    assert!((c.apply(0.85) - 0.5318).abs() < 0.001);
    // a bucket with no data stays at the raw probability
    assert!((c.apply(0.15) - 0.15).abs() < 1e-9);
    // raw Brier of this set: half at (0.15)^2, half at (0.85)^2
    assert!((c.brier().expect("brier") - 0.3725).abs() < 1e-9);
}

#[test]
fn shrinkage_blends_model_and_market() {
    assert_eq!(shrink(dec!(0.8), dec!(0.4), dec!(0.5)), dec!(0.6));
    assert_eq!(shrink(dec!(0.8), dec!(0.4), dec!(1)), dec!(0.8));
    assert_eq!(shrink(dec!(0.8), dec!(0.4), dec!(0)), dec!(0.4));
    // weights outside [0, 1] are clamped
    assert_eq!(shrink(dec!(0.8), dec!(0.4), dec!(2)), dec!(0.8));
    // weight 0.3: 0.3*0.8 + 0.7*0.4 = 0.52
    assert_eq!(shrink(dec!(0.8), dec!(0.4), dec!(0.3)), dec!(0.52));
}

#[test]
fn scores_show_when_the_market_beats_the_model() {
    // model says 0.9 YES, market says 0.1, YES never happens
    let rows = vec![(0.9, 0.1, false), (0.9, 0.1, false)];
    let cal = Calibrator::identity(100, 20.0);
    let s = score(&rows, &cal, 0.5);
    assert_eq!(s.n, 2);
    assert!(s.market < s.blend && s.blend < s.model);
    assert!((s.model - 0.81).abs() < 1e-9);
    assert!((s.market - 0.01).abs() < 1e-9);
    assert!((s.blend - 0.25).abs() < 1e-9);
    assert_eq!(score(&[], &cal, 0.5).n, 0);
}
