use clap::Parser;
use weather_bot::app::{self, App, LiveCreds};
use weather_bot::cli::{Cli, Cmd};
use weather_bot::config::Config;
use weather_bot::dashboard;
use weather_bot::data::station_registry::contains_word;
use weather_bot::error::{BotError, Result};
use weather_bot::polymarket::auth::{parse_addr, Wallet};
use weather_bot::storage::sqlite::Storage;
use weather_bot::strategies::{forecast_edge, no_harvester, signal::Signal};

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    weather_bot::util::logging::init();
    let cli = Cli::parse(); // clap exits with code 2 on bad arguments
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(e.exit_code());
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command.unwrap_or(Cmd::DryRun { allow_taker: false }) {
        Cmd::Stations => {
            let cfg = Config::load()?;
            println!("{:<14} {:<14} {:<5} {:<8} {:>9} {:>10}  {}", "CITY", "STATION", "ICAO", "PILS", "LAT", "LON", "SOURCE");
            for s in cfg.registry.stations() {
                println!(
                    "{:<14} {:<14} {:<5} {:<8} {:>9.4} {:>10.4}  {}",
                    s.city, s.polymarket_station, s.icao, s.pils, s.lat, s.lon, s.source
                );
            }
            Ok(())
        }
        Cmd::Rebates => {
            let cfg = Config::load()?;
            let st = Storage::open(&cfg.db_path)?;
            let (vol, accrued) = st.rebate_totals()?;
            println!("maker_volume_usdc      : {vol}");
            println!("accrued_rebate_usdc    : {accrued}  (estimate; Polymarket pays the actual rebate)");
            Ok(())
        }
        Cmd::SetAdmin { user, password_stdin } => {
            let cfg = Config::load()?;
            let user = match user {
                Some(u) => u,
                None => {
                    print!("Admin username: ");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                    let mut l = String::new();
                    std::io::stdin().read_line(&mut l)?;
                    l.trim().to_string()
                }
            };
            if user.is_empty() {
                return Err(BotError::Config("username cannot be empty".into()));
            }
            let pw = if password_stdin {
                let mut l = String::new();
                std::io::stdin().read_line(&mut l)?;
                l.trim_end_matches(['\r', '\n']).to_string()
            } else {
                let a = rpassword::prompt_password("Admin password (min 12 chars): ")?;
                let b = rpassword::prompt_password("Repeat password: ")?;
                if a != b {
                    return Err(BotError::Config("passwords do not match".into()));
                }
                a
            };
            if pw.chars().count() < 12 {
                return Err(BotError::Config("password must be at least 12 characters".into()));
            }
            dashboard::write_admin(&cfg.admin_file, &user, &pw)?;
            println!("admin '{user}' saved to {}", cfg.admin_file.display());
            Ok(())
        }
        Cmd::Export { csv } => {
            if !csv {
                return Err(BotError::Config("specify an export format: --csv".into()));
            }
            let cfg = Config::load()?;
            let st = Storage::open(&cfg.db_path)?;
            for p in st.export_csv(&cfg.export_dir)? {
                println!("wrote {}", p.display());
            }
            Ok(())
        }
        Cmd::Signals { city } => {
            let cfg = Config::load()?;
            let city_name = match city {
                Some(c) => {
                    let found = cfg
                        .registry
                        .cities()
                        .iter()
                        .find(|x| x.name.eq_ignore_ascii_case(&c) || x.aliases.iter().any(|a| contains_word(&c, a)))
                        .map(|x| x.name.clone());
                    match found {
                        Some(n) => Some(n),
                        None => return Err(BotError::Config(format!("unknown city '{c}'"))),
                    }
                }
                None => None,
            };
            let app = App::new(cfg, None, false).await?;
            let mut all: Vec<Signal> = forecast_edge::scan(&app, city_name.as_deref(), false).await?;
            all.extend(no_harvester::scan(&app, city_name.as_deref(), false).await?);
            println!(
                "{:<13} {:<15} {:<9} {:>6} {:>6} {:>6} {:>7}  {:<6} {}",
                "CITY", "STRATEGY", "BRACKET", "P", "ASK", "BID", "EDGE", "DECIS", "REASON / SAFETY"
            );
            for s in &all {
                let f = |o: &Option<rust_decimal::Decimal>| o.map(|d| format!("{d:.3}")).unwrap_or_else(|| "-".into());
                println!(
                    "{:<13} {:<15} {:<9} {:>6} {:>6} {:>6} {:>7}  {:<6} {} {}",
                    s.city, s.strategy, s.bracket, f(&s.model_prob), f(&s.market_ask), f(&s.market_bid),
                    f(&s.edge), s.decision, s.reason, s.settlement_check
                );
            }
            println!("{} signals", all.len());
            Ok(())
        }
        Cmd::DryRun { allow_taker } => {
            let cfg = Config::load()?;
            let app = App::new(cfg, None, allow_taker).await?;
            app::run_bot(app, "dry-run").await
        }
        Cmd::Live { enable, allow_taker } => {
            if !enable {
                return Err(BotError::Config("live mode requires the explicit --enable flag".into()));
            }
            let pk = std::env::var("POLYMARKET_PRIVATE_KEY").unwrap_or_default();
            let funder = std::env::var("POLYMARKET_FUNDER_ADDRESS").unwrap_or_default();
            if pk.trim().is_empty() || funder.trim().is_empty() {
                return Err(BotError::Config(
                    "live mode requires POLYMARKET_PRIVATE_KEY and POLYMARKET_FUNDER_ADDRESS in the environment (.env)".into(),
                ));
            }
            let sig_type: u8 = std::env::var("POLYMARKET_SIGNATURE_TYPE")
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let creds = LiveCreds { wallet: Wallet::from_hex(&pk)?, funder: parse_addr(&funder)?, signature_type: sig_type };
            let cfg = Config::load()?;
            let app = App::new(cfg, Some(creds), allow_taker).await?;
            app::run_bot(app, "live").await
        }
    }
}
