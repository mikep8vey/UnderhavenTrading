use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "weather-bot", version, about = "Polymarket weather trading bot")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Paper mode (default): real market data, simulated fills.
    DryRun {
        /// Permit taker fallbacks after the maker timeout (post_only is otherwise never overridden).
        #[arg(long)]
        allow_taker: bool,
    },
    /// Live trading. Requires --enable AND POLYMARKET_PRIVATE_KEY AND POLYMARKET_FUNDER_ADDRESS.
    Live {
        #[arg(long)]
        enable: bool,
        #[arg(long)]
        allow_taker: bool,
    },
    /// Print the station registry.
    Stations,
    /// Evaluate and print live signals (no orders are placed).
    Signals {
        #[arg(long)]
        city: Option<String>,
    },
    /// Print maker rebate accrual.
    Rebates,
    /// Dump SQLite tables to CSV.
    Export {
        #[arg(long)]
        csv: bool,
    },
}
