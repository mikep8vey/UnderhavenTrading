use crate::data::station_registry::StationRegistry;
use crate::error::{BotError, Result};
use crate::util::time::DayBasis;
use chrono_tz::Tz;
use rust_decimal::serde::float as fl;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Unit {
    C,
    F,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Station {
    pub city: String,
    pub polymarket_station: String,
    pub icao: String,
    pub pils: String,
    pub lat: f64,
    pub lon: f64,
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CityCfg {
    pub name: String,
    pub aliases: Vec<String>,
    pub tz: Tz,
    pub unit: Unit,
    pub day_basis: DayBasis,
    pub station_names: Vec<String>,
    pub decoys: Vec<String>,
}

#[derive(Deserialize)]
struct StationsFile {
    station: Vec<Station>,
}
#[derive(Deserialize)]
struct CitiesFile {
    city: Vec<CityCfg>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForecastCfg {
    #[serde(with = "fl")]
    pub kelly_mult: Decimal,
    #[serde(with = "fl")]
    pub cap_pct: Decimal,
    #[serde(with = "fl")]
    pub min_edge: Decimal,
    #[serde(with = "fl")]
    pub taker_min_edge: Decimal,
    #[serde(with = "fl")]
    pub max_spread: Decimal,
    #[serde(with = "fl")]
    pub min_shares: Decimal,
    pub maker_timeout_secs: u64,
    pub cutoff_hour_local: u32,
    pub scan_interval_secs: u64,
    pub ensemble_ttl_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HarvesterCfg {
    #[serde(with = "fl")]
    pub kelly_mult: Decimal,
    #[serde(with = "fl")]
    pub cap_pct: Decimal,
    #[serde(with = "fl")]
    pub min_ask: Decimal,
    #[serde(with = "fl")]
    pub max_ask: Decimal,
    #[serde(with = "fl")]
    pub floor_ask: Decimal,
    #[serde(with = "fl")]
    pub max_spread: Decimal,
    #[serde(with = "fl")]
    pub taker_min_ask: Decimal,
    #[serde(with = "fl")]
    pub model_prob: Decimal,
    #[serde(with = "fl")]
    pub min_shares: Decimal,
    pub maker_timeout_secs: u64,
    pub start_hour_local: u32,
    pub poll_secs: u64,
    pub settle_tolerance: f64,
    pub boundary_margin: f64,
    pub cli_ttl_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExecCfg {
    #[serde(with = "fl")]
    pub depth_multiple: Decimal,
    #[serde(with = "fl")]
    pub reprice_threshold: Decimal,
    pub reprice_secs: u64,
    pub max_quote_age_secs: u64,
    pub poll_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuardCfg {
    pub crypto_vol_threshold: f64,
    pub crypto_url: String,
    pub crypto_symbols: Vec<String>,
    pub metar_swing_c: f64,
    pub metar_swing_window_mins: i64,
    pub pause_secs: u64,
    pub check_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnsembleCfg {
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EndpointCfg {
    pub gamma_url: String,
    pub clob_url: String,
    pub metar_url: String,
    pub cli_url: String,
    pub ensemble_url: String,
    pub chain_id: u64,
    pub exchange: String,
    pub neg_risk_exchange: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DashboardCfg {
    pub enabled: bool,
    pub bind: String,
}

impl Default for DashboardCfg {
    fn default() -> Self {
        DashboardCfg { enabled: true, bind: "127.0.0.1:8787".to_string() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskCfg {
    #[serde(with = "fl")]
    pub bankroll_usdc: Decimal,
    #[serde(with = "fl")]
    pub drawdown_kill_pct: Decimal,
    #[serde(with = "fl")]
    pub max_event_pct: Decimal,
    #[serde(with = "fl")]
    pub max_total_exposure_pct: Decimal,
    pub max_positions_per_city: usize,
    pub forecast: ForecastCfg,
    pub harvester: HarvesterCfg,
    pub execution: ExecCfg,
    pub guards: GuardCfg,
    pub ensemble: EnsembleCfg,
    pub endpoints: EndpointCfg,
    #[serde(default)]
    pub dashboard: DashboardCfg,
}

pub struct Config {
    pub registry: StationRegistry,
    pub risk: RiskCfg,
    pub db_path: PathBuf,
    pub export_dir: PathBuf,
    pub admin_file: PathBuf,
}

/// Dashboard admin credentials live next to the config directory (e.g. /etc/weather-bot/admin.json).
pub fn admin_file_path(dir: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("WEATHER_BOT_ADMIN_FILE") {
        return PathBuf::from(p);
    }
    dir.parent().map(|p| p.join("admin.json")).unwrap_or_else(|| PathBuf::from("admin.json"))
}

pub fn config_dir() -> Result<PathBuf> {
    if let Ok(d) = std::env::var("WEATHER_BOT_CONFIG_DIR") {
        let p = PathBuf::from(d);
        if p.is_dir() {
            return Ok(p);
        }
        return Err(BotError::Config(format!(
            "WEATHER_BOT_CONFIG_DIR={} is not a directory",
            p.display()
        )));
    }
    for c in ["./config", "/etc/weather-bot/config"] {
        let p = PathBuf::from(c);
        if p.is_dir() {
            return Ok(p);
        }
    }
    Err(BotError::Config(
        "no config directory found (set WEATHER_BOT_CONFIG_DIR or run from the repo root)".into(),
    ))
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| BotError::Config(format!("cannot read {}: {e}", path.display())))?;
    toml::from_str(&text).map_err(|e| BotError::Config(format!("invalid {}: {e}", path.display())))
}

pub fn load_registry(dir: &Path) -> Result<StationRegistry> {
    let s: StationsFile = read_toml(&dir.join("stations.toml"))?;
    let c: CitiesFile = read_toml(&dir.join("cities.toml"))?;
    for st in &s.station {
        if st.icao.trim().is_empty() {
            return Err(BotError::Config(format!("station for {} has an empty icao", st.city)));
        }
        if !c.city.iter().any(|x| x.name == st.city) {
            return Err(BotError::Config(format!(
                "station city '{}' has no entry in cities.toml",
                st.city
            )));
        }
    }
    Ok(StationRegistry::new(s.station, c.city))
}

impl Config {
    pub fn load() -> Result<Config> {
        let dir = config_dir()?;
        let registry = load_registry(&dir)?;
        let risk: RiskCfg = read_toml(&dir.join("risk.toml"))?;
        let db_path = PathBuf::from(
            std::env::var("WEATHER_BOT_DB").unwrap_or_else(|_| "./weather-bot.db".to_string()),
        );
        let export_dir = PathBuf::from(
            std::env::var("WEATHER_BOT_EXPORT_DIR").unwrap_or_else(|_| "./exports".to_string()),
        );
        let admin_file = admin_file_path(&dir);
        Ok(Config { registry, risk, db_path, export_dir, admin_file })
    }
}
