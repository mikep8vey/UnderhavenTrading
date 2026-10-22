//! Command line entry point for UNDERHAVEN BYBIT TRADING LAB.
//!
//! There is intentionally no `--live` flag and no subcommand that can submit an
//! exchange order. The paper engine is the only execution path in this binary.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;
use underhaven::app;
use underhaven::config::{AppConfig, BUILD_VERSION};
use underhaven::model::{Interval, StrategyId};

#[derive(Parser, Debug)]
#[command(
    name = "underhaven",
    version = BUILD_VERSION,
    about = "UNDERHAVEN BYBIT TRADING LAB — paper-only Bybit perpetual research platform",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the paper engine and the authenticated dashboard.
    Serve,
    /// Create the initial administrator or reset an existing password.
    InitAdmin {
        #[arg(long, default_value = "admin")]
        username: String,
        /// Read the password from stdin instead of prompting (for automation).
        #[arg(long)]
        password_stdin: bool,
        /// Reset the password if the administrator already exists.
        #[arg(long)]
        force: bool,
    },
    /// Run a historical backtest and write JSON + trade CSV exports.
    Backtest {
        symbol: String,
        #[arg(long, default_value = "trend")]
        strategy: String,
        #[arg(long, default_value = "5m")]
        interval: String,
        /// Days of history to import when the local database is empty.
        #[arg(long, default_value_t = 90)]
        days: i64,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Maximum stored candles to feed into the simulator.
        #[arg(long, default_value_t = 20_000)]
        candles: usize,
    },
    /// Download public candles and funding history from Bybit into SQLite.
    ImportHistory {
        symbol: String,
        #[arg(long, default_value = "5m")]
        interval: String,
        #[arg(long, default_value_t = 90)]
        days: i64,
    },
    /// Validate configuration, database integrity and public connectivity.
    Doctor,
    /// Write a consistent SQLite backup using VACUUM INTO.
    Backup {
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Check the public HTTPS endpoint and certificate from this host.
    TlsCheck {
        /// Override the URL; defaults to UNDERHAVEN_PUBLIC_URL + /healthz.
        #[arg(long)]
        url: Option<String>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("underhaven: {error:#}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> Result<()> {
    let cli = Cli::parse();
    init_logging();

    let cfg = AppConfig::load().context("load configuration")?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            println!(
                "{} — PAPER ONLY — bind {} — db {}",
                BUILD_VERSION,
                cfg.bind,
                cfg.db_path.display()
            );
            app::serve(cfg).await
        }
        Command::InitAdmin {
            username,
            password_stdin,
            force,
        } => {
            let password = read_admin_password(password_stdin)?;
            app::init_admin(&cfg, &username, &password, force).await
        }
        Command::Backtest {
            symbol,
            strategy,
            interval,
            days,
            output,
            candles,
        } => {
            let strategy: StrategyId = strategy.parse().map_err(|error: String| {
                anyhow::anyhow!("{error}; expected trend, mean_reversion or breakout")
            })?;
            let interval: Interval = interval
                .parse()
                .map_err(|error: String| anyhow::anyhow!("{error}; expected 1m, 5m, 15m or 1h"))?;
            let symbol = normalize_symbol(&symbol)?;
            let output = output.unwrap_or_else(|| cfg.data_dir.join("exports"));
            // `cli_backtest` imports history when the local database is short.
            let path = app::cli_backtest(&cfg, &symbol, strategy, interval, days, &output, candles)
                .await?;
            println!("report: {}", path.display());
            Ok(())
        }
        Command::ImportHistory {
            symbol,
            interval,
            days,
        } => {
            let symbol = normalize_symbol(&symbol)?;
            let interval: Interval = interval
                .parse()
                .map_err(|error: String| anyhow::anyhow!("{error}; expected 1m, 5m, 15m or 1h"))?;
            app::cli_import(&cfg, &symbol, interval, days).await?;
            Ok(())
        }
        Command::Doctor => app::doctor(&cfg).await,
        Command::Backup { output } => {
            let output = output.unwrap_or_else(|| {
                cfg.data_dir.join("backups").join(format!(
                    "underhaven-{}.db",
                    Utc::now().format("%Y%m%d-%H%M%S")
                ))
            });
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)?;
            }
            app::backup(&cfg, &output).await
        }
        Command::TlsCheck { url } => {
            let url = url
                .or_else(|| cfg.public_url.clone())
                .context("no public URL; pass --url https://<ip> or set UNDERHAVEN_PUBLIC_URL")?;
            tls_check(&url).await
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,underhaven=info,tower_http=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn normalize_symbol(value: &str) -> Result<String> {
    let symbol = value.trim().to_ascii_uppercase();
    if symbol.len() < 6 || !symbol.ends_with("USDT") {
        bail!("symbol must look like BTCUSDT (USDT linear perpetual)");
    }
    if !symbol.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("symbol contains unsupported characters");
    }
    Ok(symbol)
}

fn read_admin_password(from_stdin: bool) -> Result<String> {
    if let Ok(value) = std::env::var("UNDERHAVEN_ADMIN_PASSWORD") {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    if from_stdin {
        let mut value = String::new();
        std::io::stdin()
            .read_to_string(&mut value)
            .context("read password from stdin")?;
        let password = value.trim_end_matches(['\r', '\n']).to_string();
        if password.is_empty() {
            bail!("stdin did not contain a password");
        }
        return Ok(password);
    }
    let password = rpassword::prompt_password("Admin password (min 12 chars, 3 classes): ")?;
    let confirm = rpassword::prompt_password("Confirm password: ")?;
    if password != confirm {
        bail!("passwords do not match");
    }
    Ok(password)
}

async fn tls_check(input: &str) -> Result<()> {
    let base = input.trim().trim_end_matches('/');
    if !base.starts_with("https://") {
        bail!("TLS check requires an https:// URL");
    }
    let url = if base.ends_with("/healthz") {
        base.to_string()
    } else {
        format!("{base}/healthz")
    };
    println!("checking {url} …");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent(format!("{BUILD_VERSION} tls-check"))
        .build()?;
    match client.get(&url).send().await {
        Ok(response) => {
            let status = response.status();
            let server = response
                .headers()
                .get(reqwest::header::SERVER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string());
            let body = response.text().await.unwrap_or_default();
            println!("HTTPS OK: status={status} server={server}");
            println!("body: {}", body.chars().take(300).collect::<String>());
            if !status.is_success() {
                bail!("endpoint returned HTTP {status}");
            }
            Ok(())
        }
        Err(error) => {
            println!("HTTPS/TLS check FAILED: {error}");
            println!(
                "If this deployment uses the self-signed fallback, the certificate is not \
                 publicly trusted by design. Use a browser trust exception only for the \
                 fallback, or fix Let's Encrypt short-lived issuance."
            );
            Err(error).context("public TLS check failed")
        }
    }
}
