//! SQLite persistence.
//!
//! The store owns the schema and all typed reads. Ledger mutations that must be
//! atomic (wallet + position + fill + audit) are performed by the paper engine in
//! a single `sqlx` transaction using the helpers here.
//!
//! The database is the source of truth across restarts. Every fill carries a
//! unique idempotency key and every funding settlement a composite primary key,
//! so replaying a recovery pass cannot double-charge the paper wallet.

use crate::config::{CostConfig, RiskConfig, StrategyConfigs};
use crate::model::{
    q8, AuditEvent, Candle, ClosedTrade, Direction, EquityPoint, FundingRate, Instrument, Interval,
    PaperPosition, ScanRow, Signal, StrategyId, StrategyStats,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Wallet {
    pub strategy: StrategyId,
    pub initial_equity: f64,
    pub cash: f64,
    pub realized: f64,
    pub fees: f64,
    pub funding: f64,
    pub equity: f64,
    pub peak_equity: f64,
    pub day_start_equity: f64,
    pub day_date: String,
}

#[derive(Debug, Clone, Default)]
pub struct EngineState {
    pub paused: bool,
    pub pause_reason: Option<String>,
    pub emergency_stop: bool,
    pub emergency_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: String,
    pub user_id: String,
    pub username: String,
    pub csrf_token: String,
    pub expires_at_ms: i64,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct StoredCredential {
    pub id: String,
    pub name: String,
    pub api_key_enc: String,
    pub api_secret_enc: String,
    pub read_only: bool,
    pub uid: Option<String>,
    pub note: Option<String>,
    pub last_verified_ms: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct BacktestRow {
    pub run_id: String,
    pub strategy: StrategyId,
    pub symbol: String,
    pub created_at_ms: i64,
    pub fingerprint: String,
    pub report_json: String,
}

impl Store {
    pub async fn connect(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .with_context(|| format!("connect SQLite {}", path.display()))?;
        sqlx::query("PRAGMA wal_autocheckpoint = 1000")
            .execute(&pool)
            .await?;
        // Embedded at compile time: the installed binary does not depend on a
        // `migrations/` directory being present on the VPS.
        static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
        MIGRATOR
            .run(&pool)
            .await
            .context("run database migrations")?;
        Ok(Self {
            pool,
            path: path.to_path_buf(),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn db_size_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    pub async fn wal_enabled(&self) -> bool {
        sqlx::query_scalar::<_, String>("PRAGMA journal_mode")
            .fetch_one(&self.pool)
            .await
            .map(|m| m.eq_ignore_ascii_case("wal"))
            .unwrap_or(false)
    }

    pub async fn audit(&self, event_type: &str, actor: &str, detail: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_events (ts_ms, event_type, actor, detail) VALUES (?, ?, ?, ?)",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(event_type)
        .bind(actor)
        .bind(detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recent_audit(&self, limit: i64) -> Result<Vec<AuditEvent>> {
        let rows = sqlx::query(
            "SELECT ts_ms, event_type, actor, detail FROM audit_events ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEvent {
                ts_ms: r.get("ts_ms"),
                event_type: r.get("event_type"),
                actor: r.get("actor"),
                detail: r.get("detail"),
            })
            .collect())
    }

    pub async fn ensure_wallets(&self, initial_equity: f64) -> Result<()> {
        let today = Utc::now().format("%Y-%m-%d").to_string();
        let now = Utc::now().timestamp_millis();
        for strategy in StrategyId::ALL {
            sqlx::query(
                "INSERT OR IGNORE INTO wallets
                 (strategy, initial_equity, cash, realized, fees, funding, equity, peak_equity, day_start_equity, day_date, updated_at_ms)
                 VALUES (?, ?, ?, 0, 0, 0, ?, ?, ?, ?, ?)",
            )
            .bind(strategy.as_str())
            .bind(initial_equity)
            .bind(initial_equity)
            .bind(initial_equity)
            .bind(initial_equity)
            .bind(initial_equity)
            .bind(&today)
            .bind(now)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    fn wallet_from_row(row: &sqlx::sqlite::SqliteRow) -> Wallet {
        let strategy: String = row.get("strategy");
        Wallet {
            strategy: strategy.parse().unwrap_or(StrategyId::Trend),
            initial_equity: row.get("initial_equity"),
            cash: row.get("cash"),
            realized: row.get("realized"),
            fees: row.get("fees"),
            funding: row.get("funding"),
            equity: row.get("equity"),
            peak_equity: row.get("peak_equity"),
            day_start_equity: row.get("day_start_equity"),
            day_date: row.get("day_date"),
        }
    }

    pub async fn wallet(&self, strategy: StrategyId) -> Result<Wallet> {
        let row = sqlx::query("SELECT * FROM wallets WHERE strategy = ?")
            .bind(strategy.as_str())
            .fetch_one(&self.pool)
            .await?;
        Ok(Self::wallet_from_row(&row))
    }

    pub async fn wallets(&self) -> Result<Vec<Wallet>> {
        let rows = sqlx::query("SELECT * FROM wallets ORDER BY strategy")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(Self::wallet_from_row).collect())
    }

    pub async fn save_wallet(&self, wallet: &Wallet) -> Result<()> {
        sqlx::query(
            "UPDATE wallets SET initial_equity=?, cash=?, realized=?, fees=?, funding=?, equity=?,
             peak_equity=?, day_start_equity=?, day_date=?, updated_at_ms=? WHERE strategy=?",
        )
        .bind(wallet.initial_equity)
        .bind(q8(wallet.cash))
        .bind(q8(wallet.realized))
        .bind(q8(wallet.fees))
        .bind(q8(wallet.funding))
        .bind(q8(wallet.equity))
        .bind(q8(wallet.peak_equity))
        .bind(q8(wallet.day_start_equity))
        .bind(&wallet.day_date)
        .bind(Utc::now().timestamp_millis())
        .bind(wallet.strategy.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn engine_state(&self) -> Result<EngineState> {
        let row = sqlx::query(
            "SELECT paused, pause_reason, emergency_stop, emergency_reason FROM engine_state WHERE id=1",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(EngineState {
            paused: row.get::<i64, _>("paused") != 0,
            pause_reason: row.get("pause_reason"),
            emergency_stop: row.get::<i64, _>("emergency_stop") != 0,
            emergency_reason: row.get("emergency_reason"),
        })
    }

    pub async fn set_engine_state(&self, state: &EngineState) -> Result<()> {
        sqlx::query(
            "UPDATE engine_state SET paused=?, pause_reason=?, emergency_stop=?, emergency_reason=?, updated_at_ms=? WHERE id=1",
        )
        .bind(state.paused as i64)
        .bind(state.pause_reason.clone())
        .bind(state.emergency_stop as i64)
        .bind(state.emergency_reason.clone())
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn open_positions(&self) -> Result<Vec<PaperPosition>> {
        let rows =
            sqlx::query("SELECT * FROM positions WHERE status='open' ORDER BY entry_time_ms")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.iter().map(Self::position_from_row).collect())
    }

    pub async fn open_position_for(&self, strategy: StrategyId) -> Result<Option<PaperPosition>> {
        let row = sqlx::query("SELECT * FROM positions WHERE status='open' AND strategy=? LIMIT 1")
            .bind(strategy.as_str())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.as_ref().map(Self::position_from_row))
    }

    fn position_from_row(row: &sqlx::sqlite::SqliteRow) -> PaperPosition {
        let strategy: String = row.get("strategy");
        let direction: String = row.get("direction");
        PaperPosition {
            id: row.get("id"),
            strategy: strategy.parse().unwrap_or(StrategyId::Trend),
            symbol: row.get("symbol"),
            direction: direction.parse().unwrap_or(Direction::Long),
            qty: row.get("qty"),
            entry_price: row.get("entry_price"),
            entry_time_ms: row.get("entry_time_ms"),
            mark_price: row.get("mark_price"),
            stop: row.get("stop"),
            initial_stop: row.get("initial_stop"),
            target: row.get("target"),
            trailing_distance: row.get("trailing_distance"),
            highest_price: row.get("highest_price"),
            lowest_price: row.get("lowest_price"),
            max_hold_until_ms: row.get("max_hold_until_ms"),
            fees_entry: row.get("fees_entry"),
            funding: row.get("funding"),
            unrealized_pnl: 0.0,
            notional: row.get("notional"),
            signal_id: row.get("signal_id"),
            status: row.get("status"),
        }
    }

    pub async fn closed_trades(
        &self,
        strategy: Option<StrategyId>,
        limit: i64,
    ) -> Result<Vec<ClosedTrade>> {
        let rows = match strategy {
            Some(s) => {
                sqlx::query(
                    "SELECT * FROM trades WHERE strategy=? ORDER BY exit_time_ms DESC LIMIT ?",
                )
                .bind(s.as_str())
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query("SELECT * FROM trades ORDER BY exit_time_ms DESC LIMIT ?")
                    .bind(limit)
                    .fetch_all(&self.pool)
                    .await?
            }
        };
        Ok(rows.iter().map(Self::trade_from_row).collect())
    }

    fn trade_from_row(row: &sqlx::sqlite::SqliteRow) -> ClosedTrade {
        let strategy: String = row.get("strategy");
        let direction: String = row.get("direction");
        ClosedTrade {
            id: row.get("id"),
            strategy: strategy.parse().unwrap_or(StrategyId::Trend),
            symbol: row.get("symbol"),
            direction: direction.parse().unwrap_or(Direction::Long),
            qty: row.get("qty"),
            entry_price: row.get("entry_price"),
            exit_price: row.get("exit_price"),
            entry_time_ms: row.get("entry_time_ms"),
            exit_time_ms: row.get("exit_time_ms"),
            gross_pnl: row.get("gross_pnl"),
            fees: row.get("fees"),
            funding: row.get("funding"),
            slippage_est: row.get("slippage_est"),
            net_pnl: row.get("net_pnl"),
            close_reason: row.get("close_reason"),
            bars_held: row.get("bars_held"),
            simulated_fill: row.get("simulated_fill"),
        }
    }

    pub async fn insert_trade(&self, trade: &ClosedTrade) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO trades
             (id, strategy, symbol, direction, qty, entry_price, exit_price, entry_time_ms, exit_time_ms,
              gross_pnl, fees, funding, slippage_est, net_pnl, close_reason, bars_held, simulated_fill, created_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&trade.id)
        .bind(trade.strategy.as_str())
        .bind(&trade.symbol)
        .bind(trade.direction.as_str())
        .bind(trade.qty)
        .bind(trade.entry_price)
        .bind(trade.exit_price)
        .bind(trade.entry_time_ms)
        .bind(trade.exit_time_ms)
        .bind(q8(trade.gross_pnl))
        .bind(q8(trade.fees))
        .bind(q8(trade.funding))
        .bind(q8(trade.slippage_est))
        .bind(q8(trade.net_pnl))
        .bind(&trade.close_reason)
        .bind(trade.bars_held)
        .bind(&trade.simulated_fill)
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // The paper fill tuple is intentionally explicit: every ledger field is
    // visible at each call site, including the idempotency key.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_fill(
        &self,
        id: &str,
        strategy: StrategyId,
        position_id: Option<&str>,
        ts_ms: i64,
        side: &str,
        qty: f64,
        price: f64,
        fee: f64,
        kind: &str,
        idempotency_key: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT OR IGNORE INTO fills
             (id, strategy, position_id, ts_ms, side, qty, price, fee, kind, idempotency_key)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(id)
        .bind(strategy.as_str())
        .bind(position_id)
        .bind(ts_ms)
        .bind(side)
        .bind(q8(qty))
        .bind(price)
        .bind(q8(fee))
        .bind(kind)
        .bind(idempotency_key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn equity_curve(&self, strategy: StrategyId, limit: i64) -> Result<Vec<EquityPoint>> {
        let rows = sqlx::query(
            "SELECT ts_ms, equity, cash, unrealized, drawdown_pct FROM equity_points
             WHERE strategy=? ORDER BY ts_ms DESC LIMIT ?",
        )
        .bind(strategy.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<EquityPoint> = rows
            .into_iter()
            .map(|r| EquityPoint {
                ts_ms: r.get("ts_ms"),
                equity: r.get("equity"),
                cash: r.get("cash"),
                unrealized: r.get("unrealized"),
                drawdown_pct: r.get("drawdown_pct"),
            })
            .collect();
        out.reverse();
        Ok(out)
    }

    pub async fn insert_equity_point(&self, strategy: StrategyId, p: &EquityPoint) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO equity_points (strategy, ts_ms, equity, cash, unrealized, drawdown_pct)
             VALUES (?,?,?,?,?,?)",
        )
        .bind(strategy.as_str())
        .bind(p.ts_ms)
        .bind(q8(p.equity))
        .bind(q8(p.cash))
        .bind(q8(p.unrealized))
        .bind(p.drawdown_pct)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_signal(&self, signal: &Signal) -> Result<()> {
        sqlx::query(
            "INSERT OR IGNORE INTO signals
             (id, strategy, symbol, timeframe, direction, candle_time_ms, signal_time_ms, entry_reference,
              proposed_entry, stop, target, atr, snapshot_json, expected_fees, expected_funding,
              expected_spread_cost, expected_slippage, expected_costs_total, win_probability,
              probability_source, estimated_net_edge, decision, rejection_reason, strategy_version, created_at_ms)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        )
        .bind(&signal.id)
        .bind(signal.strategy.as_str())
        .bind(&signal.symbol)
        .bind(&signal.timeframe)
        .bind(signal.direction.as_str())
        .bind(signal.candle_time_ms)
        .bind(signal.signal_time_ms)
        .bind(signal.entry_reference)
        .bind(signal.proposed_entry)
        .bind(signal.stop)
        .bind(signal.target)
        .bind(signal.atr)
        .bind(&signal.snapshot_json)
        .bind(signal.expected_fees)
        .bind(signal.expected_funding)
        .bind(signal.expected_spread_cost)
        .bind(signal.expected_slippage)
        .bind(signal.expected_costs_total)
        .bind(signal.win_probability)
        .bind(&signal.probability_source)
        .bind(signal.estimated_net_edge)
        .bind(&signal.decision)
        .bind(signal.rejection_reason.clone())
        .bind(&signal.strategy_version)
        .bind(signal.created_at_ms)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recent_signals(&self, limit: i64) -> Result<Vec<Signal>> {
        let rows = sqlx::query("SELECT * FROM signals ORDER BY created_at_ms DESC LIMIT ?")
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let strategy: String = r.get("strategy");
                Signal {
                    id: r.get("id"),
                    strategy: strategy.parse().unwrap_or(StrategyId::Trend),
                    symbol: r.get("symbol"),
                    timeframe: r.get("timeframe"),
                    direction: r
                        .get::<String, _>("direction")
                        .parse()
                        .unwrap_or(Direction::Long),
                    candle_time_ms: r.get("candle_time_ms"),
                    signal_time_ms: r.get("signal_time_ms"),
                    entry_reference: r.get("entry_reference"),
                    proposed_entry: r.get("proposed_entry"),
                    stop: r.get("stop"),
                    target: r.get("target"),
                    atr: r.get("atr"),
                    snapshot_json: r.get("snapshot_json"),
                    expected_fees: r.get("expected_fees"),
                    expected_funding: r.get("expected_funding"),
                    expected_spread_cost: r.get("expected_spread_cost"),
                    expected_slippage: r.get("expected_slippage"),
                    expected_costs_total: r.get("expected_costs_total"),
                    win_probability: r.get("win_probability"),
                    probability_source: r.get("probability_source"),
                    estimated_net_edge: r.get("estimated_net_edge"),
                    decision: r.get("decision"),
                    rejection_reason: r.get("rejection_reason"),
                    strategy_version: r.get("strategy_version"),
                    created_at_ms: r.get("created_at_ms"),
                }
            })
            .collect())
    }

    pub async fn put_candles(&self, candles: &[Candle]) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        for c in candles {
            sqlx::query(
                "INSERT OR REPLACE INTO candles
                 (symbol, interval, open_time_ms, open, high, low, close, volume, turnover, confirm, source, ingested_at_ms)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            )
            .bind(&c.symbol)
            .bind(c.interval.slug())
            .bind(c.open_time_ms)
            .bind(c.open)
            .bind(c.high)
            .bind(c.low)
            .bind(c.close)
            .bind(c.volume)
            .bind(c.turnover)
            .bind(c.confirm as i64)
            .bind(&c.source)
            .bind(c.ingested_at_ms)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_candles(
        &self,
        symbol: &str,
        interval: Interval,
        limit: i64,
    ) -> Result<Vec<Candle>> {
        let rows = sqlx::query(
            "SELECT * FROM candles WHERE symbol=? AND interval=? ORDER BY open_time_ms DESC LIMIT ?",
        )
        .bind(symbol)
        .bind(interval.slug())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<Candle> = rows
            .into_iter()
            .map(|r| Candle {
                symbol: r.get("symbol"),
                interval,
                open_time_ms: r.get("open_time_ms"),
                open: r.get("open"),
                high: r.get("high"),
                low: r.get("low"),
                close: r.get("close"),
                volume: r.get("volume"),
                turnover: r.get("turnover"),
                confirm: r.get::<i64, _>("confirm") != 0,
                source: r.get("source"),
                ingested_at_ms: r.get("ingested_at_ms"),
            })
            .collect();
        out.reverse();
        Ok(out)
    }

    pub async fn put_funding(&self, rates: &[FundingRate]) -> Result<()> {
        for r in rates {
            sqlx::query(
                "INSERT OR REPLACE INTO funding_rates (symbol, funding_time_ms, rate, source, fetched_at_ms)
                 VALUES (?,?,?,?,?)",
            )
            .bind(&r.symbol)
            .bind(r.funding_time_ms)
            .bind(r.rate)
            .bind(&r.source)
            .bind(r.fetched_at_ms)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn get_funding(
        &self,
        symbol: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingRate>> {
        let rows = sqlx::query(
            "SELECT * FROM funding_rates WHERE symbol=? AND funding_time_ms BETWEEN ? AND ? ORDER BY funding_time_ms",
        )
        .bind(symbol)
        .bind(start_ms)
        .bind(end_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| FundingRate {
                symbol: r.get("symbol"),
                funding_time_ms: r.get("funding_time_ms"),
                rate: r.get("rate"),
                source: r.get("source"),
                fetched_at_ms: r.get("fetched_at_ms"),
            })
            .collect())
    }

    pub async fn save_instruments(&self, instruments: &[Instrument]) -> Result<()> {
        for i in instruments {
            let payload = serde_json::to_string(i).unwrap_or_default();
            sqlx::query(
                "INSERT OR REPLACE INTO instruments (symbol, payload_json, fetched_at_ms) VALUES (?,?,?)",
            )
            .bind(&i.symbol)
            .bind(payload)
            .bind(i.fetched_at_ms)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub async fn load_instruments(&self) -> Result<Vec<Instrument>> {
        let rows = sqlx::query("SELECT payload_json FROM instruments")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let payload: String = r.get("payload_json");
                serde_json::from_str(&payload).ok()
            })
            .collect())
    }

    pub async fn save_scan(&self, rows: &[ScanRow]) -> Result<()> {
        let json = serde_json::to_string(rows).unwrap_or_else(|_| "[]".into());
        sqlx::query("INSERT INTO scan_snapshots (ts_ms, rows_json) VALUES (?, ?)")
            .bind(Utc::now().timestamp_millis())
            .bind(json)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn save_backtest(
        &self,
        run_id: &str,
        strategy: StrategyId,
        symbol: &str,
        fingerprint: &str,
        report_json: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT OR REPLACE INTO backtests (run_id, strategy, symbol, created_at_ms, fingerprint, report_json)
             VALUES (?,?,?,?,?,?)",
        )
        .bind(run_id)
        .bind(strategy.as_str())
        .bind(symbol)
        .bind(Utc::now().timestamp_millis())
        .bind(fingerprint)
        .bind(report_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_backtests(&self, limit: i64) -> Result<Vec<BacktestRow>> {
        let rows = sqlx::query(
            "SELECT run_id, strategy, symbol, created_at_ms, fingerprint, report_json FROM backtests ORDER BY created_at_ms DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let strategy: String = r.get("strategy");
                BacktestRow {
                    run_id: r.get("run_id"),
                    strategy: strategy.parse().unwrap_or(StrategyId::Trend),
                    symbol: r.get("symbol"),
                    created_at_ms: r.get("created_at_ms"),
                    fingerprint: r.get("fingerprint"),
                    report_json: r.get("report_json"),
                }
            })
            .collect())
    }

    pub async fn backtest(&self, run_id: &str) -> Result<Option<BacktestRow>> {
        let row = sqlx::query("SELECT * FROM backtests WHERE run_id=?")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| {
            let strategy: String = r.get("strategy");
            BacktestRow {
                run_id: r.get("run_id"),
                strategy: strategy.parse().unwrap_or(StrategyId::Trend),
                symbol: r.get("symbol"),
                created_at_ms: r.get("created_at_ms"),
                fingerprint: r.get("fingerprint"),
                report_json: r.get("report_json"),
            }
        }))
    }

    // ---- auth -----------------------------------------------------------

    pub async fn create_user(&self, username: &str, password_hash: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at_ms, disabled) VALUES (?,?,?,?,0)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(username)
        .bind(password_hash)
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user_by_name(&self, username: &str) -> Result<Option<AuthUser>> {
        let row =
            sqlx::query("SELECT id, username, password_hash, disabled FROM users WHERE username=?")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| AuthUser {
            id: r.get("id"),
            username: r.get("username"),
            password_hash: r.get("password_hash"),
            disabled: r.get::<i64, _>("disabled") != 0,
        }))
    }

    pub async fn user_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn update_password(&self, user_id: &str, password_hash: &str) -> Result<()> {
        sqlx::query("UPDATE users SET password_hash=? WHERE id=?")
            .bind(password_hash)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn create_session(
        &self,
        user_id: &str,
        token_hash: &str,
        csrf: &str,
        expires_at_ms: i64,
        ip: &str,
        user_agent: &str,
    ) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at_ms, expires_at_ms, last_seen_ms, revoked, ip, user_agent)
             VALUES (?,?,?,?,?,?,?,0,?,?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(token_hash)
        .bind(csrf)
        .bind(now)
        .bind(expires_at_ms)
        .bind(now)
        .bind(ip)
        .bind(user_agent)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn session_by_token(&self, token_hash: &str) -> Result<Option<SessionRow>> {
        let row = sqlx::query(
            "SELECT s.id, s.user_id, s.csrf_token, s.expires_at_ms, s.revoked, u.username
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash=? AND s.revoked=0 AND u.disabled=0",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionRow {
            id: r.get("id"),
            user_id: r.get("user_id"),
            username: r.get("username"),
            csrf_token: r.get("csrf_token"),
            expires_at_ms: r.get("expires_at_ms"),
            revoked: r.get::<i64, _>("revoked") != 0,
        }))
    }

    pub async fn touch_session(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET last_seen_ms=? WHERE id=?")
            .bind(Utc::now().timestamp_millis())
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn revoke_session(&self, id: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET revoked=1 WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn revoke_all_sessions(&self, user_id: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET revoked=1 WHERE user_id=?")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM sessions WHERE expires_at_ms < ? OR (revoked=1 AND last_seen_ms < ?)",
        )
        .bind(Utc::now().timestamp_millis())
        .bind(Utc::now().timestamp_millis() - 7 * 86_400_000)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn record_login(&self, ip: &str, username: &str, success: bool) -> Result<()> {
        sqlx::query("INSERT INTO login_attempts (ts_ms, ip, username, success) VALUES (?,?,?,?)")
            .bind(Utc::now().timestamp_millis())
            .bind(ip)
            .bind(username)
            .bind(success as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recent_failed_logins(&self, ip: &str, since_ms: i64) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM login_attempts WHERE ip=? AND ts_ms>=? AND success=0",
        )
        .bind(ip)
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await?)
    }

    // ---- read-only credentials -----------------------------------------

    pub async fn save_credential(
        &self,
        name: &str,
        api_key_enc: &str,
        api_secret_enc: &str,
        uid: Option<&str>,
        note: Option<&str>,
    ) -> Result<()> {
        sqlx::query("DELETE FROM credentials")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "INSERT INTO credentials (id, name, api_key_enc, api_secret_enc, read_only, uid, note, created_at_ms, last_verified_ms)
             VALUES (?,?,?,?,1,?,?,?,?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(name)
        .bind(api_key_enc)
        .bind(api_secret_enc)
        .bind(uid)
        .bind(note)
        .bind(Utc::now().timestamp_millis())
        .bind(Utc::now().timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn credential(&self) -> Result<Option<StoredCredential>> {
        let row = sqlx::query("SELECT * FROM credentials ORDER BY created_at_ms DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| StoredCredential {
            id: r.get("id"),
            name: r.get("name"),
            api_key_enc: r.get("api_key_enc"),
            api_secret_enc: r.get("api_secret_enc"),
            read_only: r.get::<i64, _>("read_only") != 0,
            uid: r.get("uid"),
            note: r.get("note"),
            last_verified_ms: r.get("last_verified_ms"),
        }))
    }

    pub async fn delete_credential(&self) -> Result<()> {
        sqlx::query("DELETE FROM credentials")
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn mark_credential_verified(&self) -> Result<()> {
        sqlx::query("UPDATE credentials SET last_verified_ms=?")
            .bind(Utc::now().timestamp_millis())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ---- settings / maintenance ---------------------------------------

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?,?)")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(
            sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key=?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn save_runtime_config(
        &self,
        risk: &RiskConfig,
        costs: &CostConfig,
        strategies: &StrategyConfigs,
    ) -> Result<()> {
        self.set_setting("risk_json", &serde_json::to_string(risk)?)
            .await?;
        self.set_setting("costs_json", &serde_json::to_string(costs)?)
            .await?;
        self.set_setting("strategies_json", &serde_json::to_string(strategies)?)
            .await?;
        Ok(())
    }

    pub async fn backup_to(&self, path: &Path) -> Result<()> {
        if path.exists() {
            std::fs::remove_file(path)
                .with_context(|| format!("remove existing backup {}", path.display()))?;
        }
        let target = path.to_string_lossy().replace('\'', "''");
        sqlx::query(&format!("VACUUM INTO '{target}'"))
            .execute(&self.pool)
            .await
            .with_context(|| format!("vacuum into {}", path.display()))?;
        Ok(())
    }

    /// Compare the ledger-derived cash against the wallet row. Returns the
    /// absolute discrepancy (USDT) and a human-readable explanation.
    pub async fn reconcile(&self, strategy: StrategyId) -> Result<(f64, String)> {
        let wallet = self.wallet(strategy).await?;
        let trade_net: f64 = sqlx::query_scalar::<_, f64>(
            "SELECT COALESCE(SUM(net_pnl),0.0) FROM trades WHERE strategy=?",
        )
        .bind(strategy.as_str())
        .fetch_one(&self.pool)
        .await?;
        let open: (f64, f64) = sqlx::query_as::<_, (f64, f64)>(
            "SELECT COALESCE(SUM(fees_entry),0.0), COALESCE(SUM(funding),0.0) FROM positions WHERE strategy=? AND status='open'",
        )
        .bind(strategy.as_str())
        .fetch_one(&self.pool)
        .await?;
        let expected = q8(wallet.initial_equity + trade_net - open.0 + open.1);
        let delta = q8(wallet.cash - expected);
        let detail = format!(
            "wallet cash {:.8}, ledger-derived {:.8} (trades {:.8}, open fees {:.8}, open funding {:.8})",
            wallet.cash, expected, trade_net, open.0, open.1
        );
        Ok((delta.abs(), detail))
    }
}

pub fn utc_day(ts_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ts_ms)
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string()
}

pub fn stats_from_closed(
    strategy: StrategyId,
    trades: &[ClosedTrade],
    equity: &[EquityPoint],
    initial: f64,
) -> StrategyStats {
    let metric: Vec<crate::metrics::MetricTrade> = trades
        .iter()
        .map(|t| crate::metrics::MetricTrade {
            gross_pnl: t.gross_pnl,
            net_pnl: t.net_pnl,
            fees: t.fees,
            funding_cash_effect: t.funding,
            slippage_est: t.slippage_est,
            notional: t.qty * t.entry_price,
            equity_at_entry: initial,
            exit_time_ms: t.exit_time_ms,
            bars_held: t.bars_held,
        })
        .collect();
    crate::metrics::compute_stats(strategy, &metric, equity, initial)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Direction, PaperPosition};
    use tempfile::TempDir;

    async fn store() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::connect(&path).await.unwrap();
        store.ensure_wallets(100.0).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn migrations_create_wallets_and_engine_state() {
        let (store, _dir) = store().await;
        let wallets = store.wallets().await.unwrap();
        assert_eq!(wallets.len(), 3);
        assert!(wallets.iter().all(|w| (w.cash - 100.0).abs() < 1e-9));
        let state = store.engine_state().await.unwrap();
        assert!(!state.paused && !state.emergency_stop);
    }

    #[tokio::test]
    async fn audit_is_append_only() {
        let (store, _dir) = store().await;
        store.audit("test", "unit", "hello").await.unwrap();
        let result = sqlx::query("UPDATE audit_events SET detail='tampered'")
            .execute(store.pool())
            .await;
        assert!(result.is_err());
        let result = sqlx::query("DELETE FROM audit_events")
            .execute(store.pool())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fill_idempotency_key_is_enforced() {
        let (store, _dir) = store().await;
        assert!(store
            .insert_fill(
                "f1",
                StrategyId::Trend,
                None,
                1,
                "buy",
                1.0,
                100.0,
                0.1,
                "entry",
                "key-1"
            )
            .await
            .unwrap());
        assert!(!store
            .insert_fill(
                "f2",
                StrategyId::Trend,
                None,
                1,
                "buy",
                1.0,
                100.0,
                0.1,
                "entry",
                "key-1"
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn one_open_position_per_strategy() {
        let (store, _dir) = store().await;
        let pos = PaperPosition {
            id: "p1".into(),
            strategy: StrategyId::Trend,
            symbol: "BTCUSDT".into(),
            direction: Direction::Long,
            qty: 0.001,
            entry_price: 50_000.0,
            entry_time_ms: 1,
            mark_price: 50_000.0,
            stop: 49_000.0,
            initial_stop: 49_000.0,
            target: 52_000.0,
            trailing_distance: 0.0,
            highest_price: 50_000.0,
            lowest_price: 50_000.0,
            max_hold_until_ms: 1_000,
            fees_entry: 0.01,
            funding: 0.0,
            unrealized_pnl: 0.0,
            notional: 50.0,
            signal_id: None,
            status: "open".into(),
        };
        let sql = "INSERT INTO positions (id,strategy,symbol,direction,qty,entry_price,entry_time_ms,mark_price,stop,initial_stop,target,trailing_distance,highest_price,lowest_price,max_hold_until_ms,fees_entry,funding,notional,signal_id,status,updated_at_ms) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)";
        for id in ["p1", "p2"] {
            let result = sqlx::query(sql)
                .bind(id)
                .bind("trend")
                .bind(&pos.symbol)
                .bind("long")
                .bind(pos.qty)
                .bind(pos.entry_price)
                .bind(pos.entry_time_ms)
                .bind(pos.mark_price)
                .bind(pos.stop)
                .bind(pos.initial_stop)
                .bind(pos.target)
                .bind(0.0)
                .bind(pos.entry_price)
                .bind(pos.entry_price)
                .bind(pos.max_hold_until_ms)
                .bind(pos.fees_entry)
                .bind(0.0)
                .bind(pos.notional)
                .bind(Option::<String>::None)
                .bind("open")
                .bind(1)
                .execute(store.pool())
                .await;
            if id == "p1" {
                assert!(result.is_ok());
            } else {
                assert!(
                    result.is_err(),
                    "second open position must violate the unique index"
                );
            }
        }
    }

    #[tokio::test]
    async fn backup_produces_readable_file() {
        let (store, dir) = store().await;
        let target = dir.path().join("backup.db");
        store.backup_to(&target).await.unwrap();
        assert!(target.exists());
        assert!(std::fs::metadata(&target).unwrap().len() > 0);
    }
}
