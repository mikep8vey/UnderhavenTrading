use std::path::Path;
use weather_bot::config::load_registry;
use weather_bot::data::station_registry::RulesMismatch;

fn reg() -> weather_bot::data::station_registry::StationRegistry {
    load_registry(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/config"))).expect("config loads")
}

#[test]
fn registry_matches_the_hard_coded_table() {
    let r = reg();
    assert_eq!(r.stations().len(), 8);
    let ny = r.station("New York").expect("nyc");
    assert_eq!((ny.icao.as_str(), ny.pils.as_str(), ny.polymarket_station.as_str()), ("KLGA", "CLILGA", "LaGuardia"));
    assert_eq!(r.station("Chicago").expect("chi").icao, "KORD");
    assert_eq!(r.station("Dallas").expect("dal").icao, "KDAL");
    let paris = r.station("Paris").expect("paris");
    assert_eq!(paris.icao, "LFPB");
    assert!(paris.pils.is_empty());
    assert!((r.station("London").expect("lon").lat - 51.5053).abs() < 1e-9);
}

#[test]
fn rules_text_must_name_the_registry_station() {
    let r = reg();
    assert!(r.verify_rules("New York", "Resolves per LaGuardia Airport Station (KLGA) via NWS.").is_ok());
    assert!(r.verify_rules("Chicago", "Measured at Chicago O\u{2019}Hare Intl Airport").is_ok());
    assert!(matches!(
        r.verify_rules("New York", "Resolves per JFK airport"),
        Err(RulesMismatch::StationNotMentioned { .. })
    ));
}

#[test]
fn a_mentioned_decoy_station_blocks_trading() {
    let r = reg();
    assert!(matches!(
        r.verify_rules("New York", "LaGuardia, though some sources cite JFK"),
        Err(RulesMismatch::DecoyMentioned(_))
    ));
    assert!(matches!(r.verify_rules("Atlantis", "anything"), Err(RulesMismatch::UnknownCity(_))));
}

#[test]
fn city_aliases_resolve() {
    let r = reg();
    assert_eq!(r.city_from_text("Highest temperature in NYC").map(|c| c.name.as_str()), Some("New York"));
    assert_eq!(r.city_from_text("Highest temperature in Los Angeles").map(|c| c.name.as_str()), Some("Los Angeles"));
}
