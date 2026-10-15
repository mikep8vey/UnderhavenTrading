use crate::config::{CityCfg, Station};
use regex::Regex;
use std::fmt;

pub struct StationRegistry {
    stations: Vec<Station>,
    cities: Vec<CityCfg>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RulesMismatch {
    UnknownCity(String),
    StationNotMentioned { expected: String },
    DecoyMentioned(String),
}

impl fmt::Display for RulesMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RulesMismatch::UnknownCity(c) => write!(f, "city '{c}' is not in the registry"),
            RulesMismatch::StationNotMentioned { expected } => {
                write!(f, "rules text does not mention registry station '{expected}'")
            }
            RulesMismatch::DecoyMentioned(d) => write!(f, "rules text mentions a different station '{d}'"),
        }
    }
}

fn normalize(s: &str) -> String {
    s.replace(['\u{2019}', '\u{2018}', '`'], "'")
}

/// Case-insensitive whole-word containment (word = alphanumeric run).
pub fn contains_word(hay: &str, needle: &str) -> bool {
    let n = needle.trim();
    if n.is_empty() {
        return false;
    }
    let pat = format!(r"(?i)(^|[^a-z0-9]){}([^a-z0-9]|$)", regex::escape(&normalize(n)));
    match Regex::new(&pat) {
        Ok(re) => re.is_match(&normalize(hay)),
        Err(_) => false,
    }
}

impl StationRegistry {
    pub fn new(stations: Vec<Station>, cities: Vec<CityCfg>) -> Self {
        StationRegistry { stations, cities }
    }

    pub fn stations(&self) -> &[Station] {
        &self.stations
    }

    pub fn cities(&self) -> &[CityCfg] {
        &self.cities
    }

    pub fn station(&self, city: &str) -> Option<&Station> {
        self.stations.iter().find(|s| s.city.eq_ignore_ascii_case(city))
    }

    pub fn city_cfg(&self, city: &str) -> Option<&CityCfg> {
        self.cities.iter().find(|c| c.name.eq_ignore_ascii_case(city))
    }

    /// Resolve free text ("Highest temperature in NYC") to a registry city; longest alias wins.
    pub fn city_from_text(&self, text: &str) -> Option<&CityCfg> {
        let mut best: Option<(&CityCfg, usize)> = None;
        for c in &self.cities {
            for a in c.aliases.iter().chain(std::iter::once(&c.name)) {
                if contains_word(text, a) && best.map_or(true, |(_, l)| a.len() > l) {
                    best = Some((c, a.len()));
                }
            }
        }
        best.map(|(c, _)| c)
    }

    /// Cross-check a market's rules text against the registry (SETTLEMENT_SAFETY_CHECK #1).
    /// The registry station (name, accepted alias, or ICAO) must be mentioned and no decoy may be.
    pub fn verify_rules(&self, city: &str, rules: &str) -> Result<&Station, RulesMismatch> {
        let st = self.station(city).ok_or_else(|| RulesMismatch::UnknownCity(city.to_string()))?;
        let cfg = self.city_cfg(city).ok_or_else(|| RulesMismatch::UnknownCity(city.to_string()))?;
        let mentioned = contains_word(rules, &st.polymarket_station)
            || contains_word(rules, &st.icao)
            || cfg.station_names.iter().any(|n| contains_word(rules, n));
        if !mentioned {
            return Err(RulesMismatch::StationNotMentioned { expected: st.polymarket_station.clone() });
        }
        for d in &cfg.decoys {
            if contains_word(rules, d) {
                return Err(RulesMismatch::DecoyMentioned(d.clone()));
            }
        }
        Ok(st)
    }
}
