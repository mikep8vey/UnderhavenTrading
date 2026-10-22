//! Configuration loaded from the environment and persisted strategy/risk settings.
//!
//! Safety: there is deliberately no `live` or `trading_enabled` setting. The process
//! refuses to start if an unknown trading mode is requested.

use crate::model::{BacktestParameters, Interval, StrategyId};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub const APP_NAME: &str = "UNDERHAVEN BYBIT TRADING LAB";
pub const DEFAULT_REST: &str = "https://api.bybit.com";
pub const DEFAULT_WS: &str = "wss://stream.bybit.com/v5/public/linear";
pub const BUILD_VERSION: &str = concat!("underhaven/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub session_secret: [u8; 32],
    pub master_key_path: PathBuf,
    pub secure_cookies: bool,
    pub public_url: Option<String>,
    pub rest_base: String,
    pub ws_url: String,
    pub seed_symbols: Vec<String>,
    pub max_symbols: usize,
    pub intervals: Vec<Interval>,
    pub primary_interval: Interval,
    pub scanner_refresh_secs: u64,
    pub auto_start: bool,
    pub risk: RiskConfig,
    pub costs: CostConfig,
    pub strategies: StrategyConfigs,
    pub backtest: BacktestParameters,
    pub login_max_attempts: u32,
    pub login_window_secs: i64,
    pub allow_insecure_http: bool,
    /// Deployment TLS mode, surfaced in the UI so a self-signed fallback is
    /// never mistaken for a publicly trusted certificate.
    pub tls_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub risk_per_trade_pct: f64,
    pub daily_loss_limit_pct: f64,
    pub drawdown_limit_pct: f64,
    pub max_open_positions: u32,
    pub max_total_notional_pct: f64,
    pub max_correlated_notional_pct: f64,
    pub max_consecutive_losses: u32,
    pub max_spread_bps: f64,
    pub min_depth_usd: f64,
    pub max_data_staleness_ms: i64,
    pub max_position_age_hours: f64,
    pub cooldown_minutes: i64,
    pub leverage: f64,
    pub max_funding_pct: f64,
    pub require_stop_loss: bool,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            risk_per_trade_pct: 0.5,
            daily_loss_limit_pct: 2.0,
            drawdown_limit_pct: 10.0,
            max_open_positions: 2,
            max_total_notional_pct: 100.0,
            max_correlated_notional_pct: 60.0,
            max_consecutive_losses: 5,
            max_spread_bps: 8.0,
            min_depth_usd: 25_000.0,
            max_data_staleness_ms: 5_000,
            max_position_age_hours: 24.0,
            cooldown_minutes: 720,
            leverage: 1.0,
            max_funding_pct: 0.05,
            require_stop_loss: true,
        }
    }
}

impl RiskConfig {
    pub fn validate(&self) -> Result<()> {
        if !(0.01..=2.0).contains(&self.risk_per_trade_pct) {
            bail!("risk_per_trade_pct must be between 0.01 and 2.0");
        }
        if !(0.1..=20.0).contains(&self.daily_loss_limit_pct) {
            bail!("daily_loss_limit_pct must be between 0.1 and 20");
        }
        if !(1.0..=50.0).contains(&self.drawdown_limit_pct) {
            bail!("drawdown_limit_pct must be between 1 and 50");
        }
        if self.max_open_positions == 0 || self.max_open_positions > 10 {
            bail!("max_open_positions must be between 1 and 10");
        }
        if !(1.0..=500.0).contains(&self.max_total_notional_pct) {
            bail!("max_total_notional_pct must be between 1 and 500");
        }
        if self.max_correlated_notional_pct <= 0.0
            || self.max_correlated_notional_pct > self.max_total_notional_pct
        {
            bail!("max_correlated_notional_pct must be positive and <= max_total_notional_pct");
        }
        if !self.require_stop_loss {
            bail!("paper-only policy requires a stop loss on every position");
        }
        if !(1.0..=10.0).contains(&self.leverage) {
            bail!("leverage must be between 1 and 10 (default 1x)");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostConfig {
    /// Conservative taker assumption. Replaced with the account's actual rate
    /// only when a read-only fee-rate lookup succeeds and is attributed to the key.
    pub taker_fee_bps: f64,
    pub maker_fee_bps: f64,
    pub slippage_bps: f64,
    pub max_cost_fraction_of_target: f64,
    pub min_reward_risk: f64,
    pub fee_source: String,
    pub funding_source: String,
    pub limit_fill_buffer_bps: f64,
    pub limit_min_queue_secs: i64,
    pub allow_limit_orders: bool,
}

impl Default for CostConfig {
    fn default() -> Self {
        Self {
            // Explicit conservative assumption, NOT a claim about Bybit's current schedule.
            taker_fee_bps: 7.5,
            maker_fee_bps: 2.0,
            slippage_bps: 4.0,
            max_cost_fraction_of_target: 0.35,
            min_reward_risk: 1.2,
            fee_source: "assumed_conservative".into(),
            funding_source: "bybit_funding_history_or_latest_ticker_estimate".into(),
            limit_fill_buffer_bps: 2.0,
            limit_min_queue_secs: 5,
            allow_limit_orders: false,
        }
    }
}

impl CostConfig {
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=100.0).contains(&self.taker_fee_bps) {
            bail!("taker_fee_bps outside sane range");
        }
        if !(0.0..=100.0).contains(&self.slippage_bps) {
            bail!("slippage_bps outside sane range");
        }
        if self.max_cost_fraction_of_target <= 0.0 || self.max_cost_fraction_of_target >= 1.0 {
            bail!("max_cost_fraction_of_target must be in (0,1)");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategyConfigs {
    pub trend: TrendConfig,
    pub mean_reversion: MeanReversionConfig,
    pub breakout: BreakoutConfig,
}

impl StrategyConfigs {
    pub fn validate(&self) -> Result<()> {
        self.trend.validate()?;
        self.mean_reversion.validate()?;
        self.breakout.validate()?;
        Ok(())
    }

    pub fn timeframe(&self, id: StrategyId) -> Interval {
        match id {
            StrategyId::Trend => self.trend.timeframe,
            StrategyId::MeanReversion => self.mean_reversion.timeframe,
            StrategyId::Breakout => self.breakout.timeframe,
        }
    }

    pub fn enabled(&self, id: StrategyId) -> bool {
        match id {
            StrategyId::Trend => self.trend.enabled,
            StrategyId::MeanReversion => self.mean_reversion.enabled,
            StrategyId::Breakout => self.breakout.enabled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendConfig {
    pub enabled: bool,
    pub timeframe: Interval,
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub trend_reference: usize,
    pub momentum_period: usize,
    pub momentum_min_pct: f64,
    pub atr_period: usize,
    pub min_atr_pct: f64,
    pub max_atr_pct: f64,
    pub stop_atr_mult: f64,
    pub target_atr_mult: f64,
    pub trailing_atr_mult: f64,
    pub max_hold_bars: i64,
}

impl Default for TrendConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeframe: Interval::M5,
            ema_fast: 20,
            ema_slow: 50,
            trend_reference: 100,
            momentum_period: 10,
            momentum_min_pct: 0.03,
            atr_period: 14,
            min_atr_pct: 0.03,
            max_atr_pct: 2.5,
            stop_atr_mult: 1.5,
            target_atr_mult: 3.0,
            trailing_atr_mult: 1.2,
            max_hold_bars: 96,
        }
    }
}

impl TrendConfig {
    pub fn validate(&self) -> Result<()> {
        if self.ema_fast >= self.ema_slow {
            bail!("trend EMA fast must be < slow");
        }
        if self.ema_slow < 5 || self.trend_reference < self.ema_slow {
            bail!("trend reference must be >= slow EMA and slow EMA >= 5");
        }
        if self.atr_period < 2 || self.momentum_period < 2 {
            bail!("trend indicator periods too small");
        }
        if self.stop_atr_mult <= 0.0 || self.target_atr_mult <= 0.0 {
            bail!("trend stop/target multipliers must be positive");
        }
        if self.max_hold_bars < 1 {
            bail!("trend max hold bars must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeanReversionConfig {
    pub enabled: bool,
    pub timeframe: Interval,
    pub sma_period: usize,
    pub bb_k: f64,
    pub z_entry: f64,
    pub atr_period: usize,
    pub stop_atr_mult: f64,
    pub max_hold_bars: i64,
    pub min_band_width_pct: f64,
    pub max_band_width_pct: f64,
    pub max_trend_strength: f64,
    pub require_reversal: bool,
}

impl Default for MeanReversionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeframe: Interval::M5,
            sma_period: 20,
            bb_k: 2.0,
            z_entry: 2.0,
            atr_period: 14,
            stop_atr_mult: 1.5,
            max_hold_bars: 48,
            min_band_width_pct: 0.15,
            max_band_width_pct: 4.0,
            max_trend_strength: 1.0,
            require_reversal: true,
        }
    }
}

impl MeanReversionConfig {
    pub fn validate(&self) -> Result<()> {
        if self.sma_period < 5 {
            bail!("mean reversion SMA period must be >= 5");
        }
        if self.z_entry <= 0.5 || self.z_entry > 6.0 {
            bail!("mean reversion z entry must be in (0.5, 6]");
        }
        if self.bb_k <= 0.5 {
            bail!("Bollinger k must be > 0.5");
        }
        if self.atr_period < 2 || self.stop_atr_mult <= 0.0 {
            bail!("mean reversion ATR settings invalid");
        }
        if self.max_hold_bars < 1 {
            bail!("mean reversion max hold bars must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakoutConfig {
    pub enabled: bool,
    pub timeframe: Interval,
    pub lookback: usize,
    pub buffer_atr: f64,
    pub atr_period: usize,
    pub volume_mult: f64,
    pub volume_period: usize,
    pub stop_atr_mult: f64,
    pub target_atr_mult: f64,
    pub trailing_atr_mult: f64,
    pub max_hold_bars: i64,
    pub min_range_pct: f64,
    pub max_range_pct: f64,
}

impl Default for BreakoutConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            timeframe: Interval::M5,
            lookback: 20,
            buffer_atr: 0.05,
            atr_period: 14,
            volume_mult: 1.2,
            volume_period: 20,
            stop_atr_mult: 1.2,
            target_atr_mult: 2.4,
            trailing_atr_mult: 1.1,
            max_hold_bars: 96,
            min_range_pct: 0.15,
            max_range_pct: 3.5,
        }
    }
}

impl BreakoutConfig {
    pub fn validate(&self) -> Result<()> {
        if self.lookback < 5 {
            bail!("breakout lookback must be >= 5");
        }
        if self.atr_period < 2 || self.volume_period < 2 {
            bail!("breakout indicator periods too small");
        }
        if self.stop_atr_mult <= 0.0 || self.target_atr_mult <= 0.0 {
            bail!("breakout stop/target multipliers must be positive");
        }
        if self.max_hold_bars < 1 {
            bail!("breakout max hold bars must be positive");
        }
        Ok(())
    }
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    match env::var(key) {
        Ok(v) => v.parse::<T>().unwrap_or(default),
        Err(_) => default,
    }
}

fn read_or_create_secret(path: &Path, len: usize) -> Result<Vec<u8>> {
    if path.exists() {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        if bytes.len() < len {
            bail!("secret file {} is too short", path.display());
        }
        return Ok(bytes);
    }
    let mut buf = vec![0u8; len];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut buf);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &buf)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(buf)
}

pub fn parse_hex_or_base64(s: &str) -> Option<Vec<u8>> {
    if let Ok(v) = hex::decode(s.trim()) {
        return Some(v);
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .ok()
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        if let Ok(mode) = env::var("UNDERHAVEN_TRADING_MODE") {
            if !mode.eq_ignore_ascii_case("paper") {
                bail!(
                    "UNDERHAVEN_TRADING_MODE={mode} refused: this application is PAPER ONLY and has no live-order code path"
                );
            }
        }

        let data_dir =
            PathBuf::from(env::var("UNDERHAVEN_DATA_DIR").unwrap_or_else(|_| "./data".to_string()));
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("create data directory {}", data_dir.display()))?;

        let db_path = env::var("UNDERHAVEN_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("underhaven.db"));

        let session_key_file = data_dir.join("session.key");
        let session_raw = match env::var("UNDERHAVEN_SESSION_SECRET") {
            Ok(v) if !v.trim().is_empty() => parse_hex_or_base64(&v)
                .context("UNDERHAVEN_SESSION_SECRET must be hex or base64")?,
            _ => read_or_create_secret(&session_key_file, 32)?,
        };
        if session_raw.len() < 32 {
            bail!("session secret must be at least 32 bytes");
        }
        let mut session_secret = [0u8; 32];
        session_secret.copy_from_slice(&session_raw[..32]);

        let master_key_path = env::var("UNDERHAVEN_MASTER_KEY_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("master.key"));

        let bind: SocketAddr = env::var("UNDERHAVEN_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .context("UNDERHAVEN_BIND must be a socket address")?;

        let secure_cookies = env_parse("UNDERHAVEN_SECURE_COOKIES", true);
        let allow_insecure_http = env_parse("UNDERHAVEN_ALLOW_INSECURE_HTTP", false);

        if !secure_cookies && !allow_insecure_http {
            // Local development over http://127.0.0.1 cannot use Secure cookies,
            // so allow it only for loopback binds. A public bind requires opt-in.
            let loopback = bind.ip().is_loopback();
            if !loopback {
                bail!("refusing non-loopback bind without Secure cookies; set UNDERHAVEN_SECURE_COOKIES=true (recommended) or explicitly set UNDERHAVEN_ALLOW_INSECURE_HTTP=true for a trusted private network");
            }
        }

        let seed_symbols = env::var("UNDERHAVEN_SYMBOLS")
            .unwrap_or_else(|_| "BTCUSDT,ETHUSDT,SOLUSDT,XRPUSDT,DOGEUSDT".to_string())
            .split(',')
            .map(|s| s.trim().to_ascii_uppercase())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        let max_symbols = env_parse("UNDERHAVEN_MAX_SYMBOLS", 12usize).clamp(1, 60);
        let primary_interval = env::var("UNDERHAVEN_PRIMARY_INTERVAL")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Interval::M5);

        let rest_base = env::var("UNDERHAVEN_REST_BASE").unwrap_or_else(|_| DEFAULT_REST.into());
        let ws_url = env::var("UNDERHAVEN_WS_URL").unwrap_or_else(|_| DEFAULT_WS.into());
        if !rest_base.starts_with("https://") && !rest_base.starts_with("http://127.0.0.1") {
            bail!("Bybit REST base must use HTTPS");
        }
        if !ws_url.starts_with("wss://") {
            bail!("Bybit WebSocket URL must use WSS");
        }

        let risk = RiskConfig {
            max_spread_bps: env_parse("UNDERHAVEN_MAX_SPREAD_BPS", 8.0),
            min_depth_usd: env_parse("UNDERHAVEN_MIN_DEPTH_USD", 25_000.0),
            max_data_staleness_ms: env_parse("UNDERHAVEN_MAX_STALENESS_MS", 5_000),
            ..RiskConfig::default()
        };
        let costs = CostConfig {
            taker_fee_bps: env_parse("UNDERHAVEN_TAKER_FEE_BPS", 7.5),
            slippage_bps: env_parse("UNDERHAVEN_SLIPPAGE_BPS", 4.0),
            ..CostConfig::default()
        };
        risk.validate()?;
        costs.validate()?;

        let strategies = StrategyConfigs::default();
        strategies.validate()?;

        let tls_mode = env::var("UNDERHAVEN_TLS_MODE")
            .unwrap_or_else(|_| "unknown".to_string())
            .to_ascii_lowercase();
        if !matches!(
            tls_mode.as_str(),
            "letsencrypt" | "self_signed" | "proxy" | "none" | "unknown"
        ) {
            bail!("UNDERHAVEN_TLS_MODE must be letsencrypt, self_signed, proxy, none or unknown");
        }

        Ok(Self {
            bind,
            data_dir,
            db_path,
            session_secret,
            master_key_path,
            secure_cookies,
            public_url: env::var("UNDERHAVEN_PUBLIC_URL").ok(),
            rest_base,
            ws_url,
            seed_symbols,
            max_symbols,
            intervals: Interval::ALL.to_vec(),
            primary_interval,
            scanner_refresh_secs: env_parse("UNDERHAVEN_SCANNER_REFRESH_SECS", 120u64),
            auto_start: env_parse("UNDERHAVEN_AUTO_START", true),
            risk,
            costs,
            strategies,
            backtest: BacktestParameters::default(),
            login_max_attempts: env_parse("UNDERHAVEN_LOGIN_MAX_ATTEMPTS", 5u32),
            login_window_secs: env_parse("UNDERHAVEN_LOGIN_WINDOW_SECS", 900i64),
            allow_insecure_http,
            tls_mode,
        })
    }

    pub fn database_url(&self) -> String {
        format!(
            "sqlite://{}?mode=rwc",
            self.db_path.to_string_lossy().replace('\\', "/")
        )
    }
}
