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
    /// Weight on the (calibrated) model when blending with the market's YES mid: p = w*model + (1-w)*market.
    #[serde(with = "fl", default = "default_shrink")]
    pub shrink_weight: Decimal,
    /// Calibration stays off (identity) until this many settled forecasts exist.
    #[serde(default = "default_cal_min")]
    pub calibration_min_samples: u32,
    /// Pseudo-observations pulling each calibration bucket toward the raw probability.
    #[serde(default = "default_cal_prior")]
    pub calibration_prior: f64,
    /// Favourite guard: never buy NO on a bracket the market gives more than this YES probability (the market is better
    /// calibrated than a raw ensemble at its modal bracket). Set to 1.0 to disable.
    #[serde(with = "fl", default = "default_no_max_yes")]
    pub no_max_yes_mid: Decimal,
    /// Lottery-ticket guard: skip entries priced below this.
    #[serde(with = "fl", default = "default_min_entry")]
    pub min_entry_price: Decimal,
}

fn default_shrink() -> Decimal {
    Decimal::new(3, 1)
}
fn default_maker_depth() -> Decimal {
    Decimal::ZERO
}
fn default_taker_depth() -> Decimal {
    Decimal::new(30, 1)
}
fn default_no_max_yes() -> Decimal {
    Decimal::new(30, 2)
}
fn default_min_entry() -> Decimal {
    Decimal::new(5, 2)
}
fn default_cal_min() -> u32 {
    100
}
fn default_cal_prior() -> f64 {
    20.0
}
fn default_daily_loss() -> Decimal {
    Decimal::new(5, 2)
}
fn default_attempts() -> u32 {
    3
}
fn default_cooldown() -> u64 {
    900
}
fn default_data_api() -> String {
    "https://data-api.polymarket.com".to_string()
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
    /// Ask-depth requirement for the passive maker quote. 0.0 disables the gate (the default: a maker bid
    /// does not consume ask liquidity). Set e.g. 0.25 to cap the order at a fraction of visible touch depth.
    #[serde(with = "fl", default = "default_maker_depth")]
    pub maker_depth_multiple: Decimal,
    /// Ask-depth requirement before the optional taker fallback crosses the spread (3.0 = 3x size).
    #[serde(with = "fl", default = "default_taker_depth")]
    pub taker_depth_multiple: Decimal,
    #[serde(with = "fl")]
    pub reprice_threshold: Decimal,
    /// How often the fair value is refreshed from the cached ensemble while a quote rests.
    pub reprice_secs: u64,
    /// Hard TTL: a resting quote is cancelled and reposted after this long even if nothing moved.
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
    /// Public trade tape (paper-maker fills). Defaults keep older config files working.
    #[serde(default = "default_data_api")]
    pub data_api_url: String,
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
    /// Per account: stop opening trades for the rest of the UTC day once equity is down this much since the day's start.
    #[serde(with = "fl", default = "default_daily_loss")]
    pub daily_loss_cap_pct: Decimal,
    /// Per account and market per UTC day: give up after this many unfilled entry attempts.
    #[serde(default = "default_attempts")]
    pub max_attempts_per_market: u32,
    /// Minimum wait between two attempts on the same market.
    #[serde(default = "default_cooldown")]
    pub retry_cooldown_secs: u64,
    /// Bankroll the LIVE account sizes from (what you actually fund). Must be > 0 to enable live trading.
    #[serde(with = "fl", default)]
    pub live_bankroll_usdc: Decimal,
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
