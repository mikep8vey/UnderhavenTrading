use crate::error::{BotError, Result};
use crate::risk::exposure::PositionRec;
use crate::strategies::signal::Signal;
use crate::util::lock;
use crate::util::math::parse_dec;
use crate::util::time::now_rfc3339;
use chrono::Utc;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection};
use rust_decimal::Decimal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SHARED_SCHEMA: &str = include_str!("schema.sql");
const ACCOUNT_SCHEMA: &str = include_str!("account_schema.sql");
/// Per-account tables. Older single-account databases hold these unprefixed; they become paper_*.
const ACCOUNT_TABLES: [&str; 5] = ["orders", "fills", "positions", "rebates", "equity_snapshots"];
const SHARED_TABLES: [&str; 5] = ["signals", "events", "runs", "forecast_log", "signal_exec"];
pub const ACCOUNTS: [&str; 2] = ["paper", "live"];

/// All money is stored as TEXT (exact Decimal strings), all timestamps as UTC RFC3339.
/// `Storage::open` returns the shared handle; `.account("paper")` / `.account("live")` return handles whose
/// order/fill/position/rebate/equity tables are separate (paper_* / live_*) while signals stay shared.
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
    pfx: String,
}

fn d(x: &Decimal) -> String {
    x.to_string()
}
fn od(x: &Option<Decimal>) -> Option<String> {
    x.as_ref().map(|v| v.to_string())
}
fn pd(s: String) -> Decimal {
    parse_dec(&s).unwrap_or(Decimal::ZERO)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .map_err(BotError::from)
}

impl Storage {
    pub fn open(path: &Path) -> Result<Storage> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        for t in ACCOUNT_TABLES {
            if table_exists(&conn, t)? && !table_exists(&conn, &format!("paper_{t}"))? {
                conn.execute_batch(&format!("ALTER TABLE {t} RENAME TO paper_{t};"))?;
            }
        }
        conn.execute_batch(SHARED_SCHEMA)?;
        for a in ACCOUNTS {
            conn.execute_batch(&ACCOUNT_SCHEMA.replace("{P}", &format!("{a}_")))?;
        }
        Ok(Storage { conn: Arc::new(Mutex::new(conn)), pfx: String::new() })
    }

    /// Handle for one account's tables (shares the same connection).
    pub fn account(&self, name: &str) -> Storage {
        Storage { conn: self.conn.clone(), pfx: format!("{name}_") }
    }

    /// Table-name prefix of this handle ("paper_", "live_" or "").
    pub fn prefix(&self) -> &str {
        &self.pfx
    }

    fn t(&self, name: &str) -> String {
        format!("{}{}", self.pfx, name)
    }

    /// Record what one account did with a signal. Safe to call repeatedly; the latest status wins.
    pub fn set_signal_exec(&self, signal_id: i64, account: &str, status: &str, detail: &str) -> Result<()> {
        if signal_id <= 0 {
            return Ok(());
        }
        lock(&self.conn).execute(
            "INSERT INTO signal_exec (signal_id,account,status,detail,ts) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(signal_id,account) DO UPDATE SET status=excluded.status,detail=excluded.detail,ts=excluded.ts",
            params![signal_id, account, status, detail, now_rfc3339()],
        )?;
        Ok(())
    }

    pub fn insert_signal(&self, s: &Signal) -> Result<i64> {
        let c = lock(&self.conn);
        c.execute(
            "INSERT INTO signals (ts,city,station_icao,strategy,bracket,model_prob,market_ask,market_bid,edge,settlement_check,decision,reason)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                s.ts, s.city, s.station_icao, s.strategy, s.bracket, od(&s.model_prob), od(&s.market_ask),
                od(&s.market_bid), od(&s.edge), s.settlement_check, s.decision, s.reason
            ],
        )?;
        Ok(c.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_order(
        &self,
        signal_id: i64,
        exchange_id: &str,
        side: &str,
        price: Decimal,
        size: Decimal,
        post_only: bool,
        status: &str,
    ) -> Result<i64> {
        let c = lock(&self.conn);
        c.execute(
            &format!(
                "INSERT INTO {} (ts,signal_id,exchange_id,side,price,size,post_only,status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                self.t("orders")
            ),
            params![now_rfc3339(), signal_id, exchange_id, side, d(&price), d(&size), post_only as i64, status],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_order(&self, id: i64, status: &str, filled: Decimal, fee: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            &format!("UPDATE {} SET status=?1, filled=?2, fee=?3 WHERE id=?4", self.t("orders")),
            params![status, d(&filled), d(&fee), id],
        )?;
        Ok(())
    }

    pub fn insert_fill(&self, order_id: i64, price: Decimal, size: Decimal, is_maker: bool) -> Result<()> {
        lock(&self.conn).execute(
            &format!("INSERT INTO {} (order_id,ts,price,size,is_maker) VALUES (?1,?2,?3,?4,?5)", self.t("fills")),
            params![order_id, now_rfc3339(), d(&price), d(&size), is_maker as i64],
        )?;
        Ok(())
    }

    pub fn upsert_position(&self, p: &PositionRec) -> Result<()> {
        lock(&self.conn).execute(
            &format!(
                "INSERT INTO {} (ts,city,bracket,side,size,avg_price,status,market_id,token_id,outcome_idx,event_key)
                 VALUES (?1,?2,?3,?4,?5,?6,'open',?7,?8,?9,?10)
                 ON CONFLICT(token_id) DO UPDATE SET size=excluded.size, avg_price=excluded.avg_price, ts=excluded.ts",
                self.t("positions")
            ),
            params![
                now_rfc3339(), p.city, p.bracket, p.outcome, d(&p.size), d(&p.avg_price), p.market_id, p.token_id,
                p.outcome_idx as i64, p.event_key
            ],
        )?;
        Ok(())
    }

    pub fn close_position(&self, token_id: &str, pnl: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            &format!("UPDATE {} SET status='closed', pnl=?1, ts=?2 WHERE token_id=?3", self.t("positions")),
            params![d(&pnl), now_rfc3339(), token_id],
        )?;
        Ok(())
    }

    pub fn open_positions(&self) -> Result<Vec<PositionRec>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(&format!(
            "SELECT market_id,token_id,outcome_idx,city,event_key,bracket,side,size,avg_price FROM {} WHERE status='open'",
            self.t("positions")
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(PositionRec {
                market_id: r.get(0)?,
                token_id: r.get(1)?,
                outcome_idx: r.get::<_, i64>(2)? as usize,
                city: r.get(3)?,
                event_key: r.get(4)?,
                bracket: r.get(5)?,
                outcome: r.get(6)?,
                size: pd(r.get(7)?),
                avg_price: pd(r.get(8)?),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn realized_pnl(&self) -> Result<Decimal> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(&format!("SELECT pnl FROM {} WHERE status='closed'", self.t("positions")))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut total = Decimal::ZERO;
        for r in rows {
            total += pd(r?);
        }
        Ok(total)
    }

    pub fn add_rebate(&self, maker_volume: Decimal, accrued: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            &format!("INSERT INTO {} (ts,maker_volume,accrued_usdc) VALUES (?1,?2,?3)", self.t("rebates")),
            params![now_rfc3339(), d(&maker_volume), d(&accrued)],
        )?;
        Ok(())
    }

    /// (cumulative maker volume, estimated accrued rebate) in USDC.
    pub fn rebate_totals(&self) -> Result<(Decimal, Decimal)> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(&format!("SELECT maker_volume, accrued_usdc FROM {}", self.t("rebates")))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let (mut vol, mut acc) = (Decimal::ZERO, Decimal::ZERO);
        for r in rows {
            let (v, a) = r?;
            vol += pd(v);
            acc += pd(a);
        }
        Ok((vol, acc))
    }

    pub fn insert_equity(&self, equity: Decimal, drawdown: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            &format!("INSERT INTO {} (ts,equity,drawdown) VALUES (?1,?2,?3)", self.t("equity_snapshots")),
            params![now_rfc3339(), d(&equity), d(&drawdown)],
        )?;
        Ok(())
    }

    /// Highest equity ever recorded (restores the drawdown peak after a restart).
    pub fn peak_equity(&self) -> Result<Decimal> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(&format!("SELECT equity FROM {}", self.t("equity_snapshots")))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut peak = Decimal::ZERO;
        for r in rows {
            let v = pd(r?);
            if v > peak {
                peak = v;
            }
        }
        Ok(peak)
    }

    /// First equity snapshot on or after a UTC date ("YYYY-MM-DD"): the start-of-day equity for the daily loss cap.
    pub fn first_equity_since(&self, date: &str) -> Result<Option<Decimal>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(&format!(
            "SELECT equity FROM {} WHERE ts >= ?1 ORDER BY id LIMIT 1",
            self.t("equity_snapshots")
        ))?;
        let mut rows = stmt.query_map(params![date], |r| r.get::<_, String>(0))?;
        match rows.next() {
            Some(r) => Ok(Some(pd(r?))),
            None => Ok(None),
        }
    }

    // ---- forecast log (calibration dataset, shared) ----

    #[allow(clippy::too_many_arguments)]
    pub fn log_forecast(
        &self,
        market_id: &str,
        phase: &str,
        city: &str,
        bracket: &str,
        event_date: &str,
        yes_idx: usize,
        p_raw: f64,
        p_mkt: f64,
    ) -> Result<()> {
        lock(&self.conn).execute(
            "INSERT OR IGNORE INTO forecast_log (ts,market_id,phase,city,bracket,event_date,yes_idx,p_raw,p_mkt)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![now_rfc3339(), market_id, phase, city, bracket, event_date, yes_idx as i64, p_raw, p_mkt],
        )?;
        Ok(())
    }

    /// Logged forecasts whose market date has passed and whose outcome is still unknown: (id, market_id, yes_idx).
    pub fn unresolved_forecasts(&self, before_date: &str, limit: i64) -> Result<Vec<(i64, String, usize)>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(
            "SELECT id, market_id, yes_idx FROM forecast_log WHERE outcome IS NULL AND event_date < ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![before_date, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)? as usize))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn set_forecast_outcome(&self, id: i64, yes_won: bool) -> Result<()> {
        lock(&self.conn).execute(
            "UPDATE forecast_log SET outcome=?1 WHERE id=?2",
            params![yes_won as i64, id],
        )?;
        Ok(())
    }

    /// (raw model probability of YES, market YES mid at the time, did YES win) for every settled forecast.
    pub fn scoring_rows(&self) -> Result<Vec<(f64, f64, bool)>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare("SELECT p_raw, p_mkt, outcome FROM forecast_log WHERE outcome IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)? != 0)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// (city, event_date, raw model probability, market YES mid, did YES win) for every settled forecast.
    /// Ordered by event_date so the walk-forward evaluator can fit only on earlier days.
    pub fn scoring_rows_full(&self) -> Result<Vec<(String, String, f64, f64, bool)>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(
            "SELECT city,event_date,p_raw,p_mkt,outcome FROM forecast_log WHERE outcome IS NOT NULL ORDER BY event_date",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, i64>(4)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Archive an account's tables (renamed with a _bak_<timestamp> suffix) and start it fresh. forecast_log is kept.
    pub fn archive_account(&self, name: &str) -> Result<String> {
        if !ACCOUNTS.contains(&name) {
            return Err(BotError::Config(format!("unknown account '{name}'")));
        }
        let stamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
        let c = lock(&self.conn);
        for t in ACCOUNT_TABLES {
            c.execute_batch(&format!("ALTER TABLE {name}_{t} RENAME TO {name}_{t}_bak_{stamp};"))?;
        }
        c.execute_batch(&ACCOUNT_SCHEMA.replace("{P}", &format!("{name}_")))?;
        Ok(stamp)
    }

    /// (raw model probability of YES, did YES win) for every settled forecast.
    pub fn calibration_rows(&self) -> Result<Vec<(f64, bool)>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare("SELECT p_raw, outcome FROM forecast_log WHERE outcome IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)? != 0)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Run a fixed SELECT and return each row as a JSON object (used by the dashboard).
    pub fn query_json(&self, sql: &str) -> Result<Vec<serde_json::Value>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(sql)?;
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut m = serde_json::Map::new();
            for (i, n) in names.iter().enumerate() {
                let v = match row.get_ref(i)? {
                    ValueRef::Null => serde_json::Value::Null,
                    ValueRef::Integer(x) => serde_json::Value::from(x),
                    ValueRef::Real(f) => serde_json::Value::from(f),
                    ValueRef::Text(b) => serde_json::Value::String(String::from_utf8_lossy(b).to_string()),
                    ValueRef::Blob(_) => serde_json::Value::Null,
                };
                m.insert(n.clone(), v);
            }
            out.push(serde_json::Value::Object(m));
        }
        Ok(out)
    }

    pub fn log_event(&self, level: &str, kind: &str, message: &str) -> Result<()> {
        lock(&self.conn).execute(
            "INSERT INTO events (ts,level,kind,message) VALUES (?1,?2,?3,?4)",
            params![now_rfc3339(), level, kind, message],
        )?;
        Ok(())
    }

    pub fn start_run(&self, mode: &str, starting_balance: Decimal) -> Result<i64> {
        let c = lock(&self.conn);
        c.execute(
            "INSERT INTO runs (ts,mode,starting_balance) VALUES (?1,?2,?3)",
            params![now_rfc3339(), mode, d(&starting_balance)],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn end_run(&self, id: i64, ending_balance: Decimal, net_pnl: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            "UPDATE runs SET ending_balance=?1, net_pnl=?2 WHERE id=?3",
            params![d(&ending_balance), d(&net_pnl), id],
        )?;
        Ok(())
    }

    /// Dump every table to `<dir>/<table>_<YYYY-MM-DD>.csv` (UTC date).
    pub fn export_csv(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dir)?;
        let day = Utc::now().format("%Y-%m-%d").to_string();
        let mut tables: Vec<String> = SHARED_TABLES.iter().map(|s| s.to_string()).collect();
        for a in ACCOUNTS {
            for t in ACCOUNT_TABLES {
                tables.push(format!("{a}_{t}"));
            }
        }
        let c = lock(&self.conn);
        let mut written = Vec::new();
        for t in tables {
            let path = dir.join(format!("{t}_{day}.csv"));
            let mut stmt = c.prepare(&format!("SELECT * FROM {t}"))?;
            let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
            let mut w = csv::Writer::from_path(&path)?;
            w.write_record(&names)?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let mut rec: Vec<String> = Vec::with_capacity(names.len());
                for i in 0..names.len() {
                    rec.push(match row.get_ref(i)? {
                        ValueRef::Null => String::new(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(f) => f.to_string(),
                        ValueRef::Text(b) => String::from_utf8_lossy(b).to_string(),
                        ValueRef::Blob(_) => String::new(),
                    });
                }
                w.write_record(&rec)?;
            }
            w.flush()?;
            written.push(path);
        }
        Ok(written)
    }
}
