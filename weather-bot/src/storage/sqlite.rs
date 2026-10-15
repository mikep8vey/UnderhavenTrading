use crate::error::Result;
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
use std::sync::Mutex;
use std::time::Duration;

const TABLES: [&str; 8] = ["signals", "orders", "fills", "positions", "rebates", "events", "runs", "equity_snapshots"];

/// All money is stored as TEXT (exact Decimal strings), all timestamps as UTC RFC3339.
pub struct Storage {
    conn: Mutex<Connection>,
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
        conn.execute_batch(include_str!("schema.sql"))?;
        Ok(Storage { conn: Mutex::new(conn) })
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
            "INSERT INTO orders (ts,signal_id,exchange_id,side,price,size,post_only,status) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![now_rfc3339(), signal_id, exchange_id, side, d(&price), d(&size), post_only as i64, status],
        )?;
        Ok(c.last_insert_rowid())
    }

    pub fn update_order(&self, id: i64, status: &str, filled: Decimal, fee: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            "UPDATE orders SET status=?1, filled=?2, fee=?3 WHERE id=?4",
            params![status, d(&filled), d(&fee), id],
        )?;
        Ok(())
    }

    pub fn insert_fill(&self, order_id: i64, price: Decimal, size: Decimal, is_maker: bool) -> Result<()> {
        lock(&self.conn).execute(
            "INSERT INTO fills (order_id,ts,price,size,is_maker) VALUES (?1,?2,?3,?4,?5)",
            params![order_id, now_rfc3339(), d(&price), d(&size), is_maker as i64],
        )?;
        Ok(())
    }

    pub fn upsert_position(&self, p: &PositionRec) -> Result<()> {
        lock(&self.conn).execute(
            "INSERT INTO positions (ts,city,bracket,side,size,avg_price,status,market_id,token_id,outcome_idx,event_key)
             VALUES (?1,?2,?3,?4,?5,?6,'open',?7,?8,?9,?10)
             ON CONFLICT(token_id) DO UPDATE SET size=excluded.size, avg_price=excluded.avg_price, ts=excluded.ts",
            params![
                now_rfc3339(), p.city, p.bracket, p.outcome, d(&p.size), d(&p.avg_price), p.market_id, p.token_id,
                p.outcome_idx as i64, p.event_key
            ],
        )?;
        Ok(())
    }

    pub fn close_position(&self, token_id: &str, pnl: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            "UPDATE positions SET status='closed', pnl=?1, ts=?2 WHERE token_id=?3",
            params![d(&pnl), now_rfc3339(), token_id],
        )?;
        Ok(())
    }

    pub fn open_positions(&self) -> Result<Vec<PositionRec>> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare(
            "SELECT market_id,token_id,outcome_idx,city,event_key,bracket,side,size,avg_price FROM positions WHERE status='open'",
        )?;
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
        let mut stmt = c.prepare("SELECT pnl FROM positions WHERE status='closed'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut total = Decimal::ZERO;
        for r in rows {
            total += pd(r?);
        }
        Ok(total)
    }

    pub fn add_rebate(&self, maker_volume: Decimal, accrued: Decimal) -> Result<()> {
        lock(&self.conn).execute(
            "INSERT INTO rebates (ts,maker_volume,accrued_usdc) VALUES (?1,?2,?3)",
            params![now_rfc3339(), d(&maker_volume), d(&accrued)],
        )?;
        Ok(())
    }

    /// (cumulative maker volume, estimated accrued rebate) in USDC.
    pub fn rebate_totals(&self) -> Result<(Decimal, Decimal)> {
        let c = lock(&self.conn);
        let mut stmt = c.prepare("SELECT maker_volume, accrued_usdc FROM rebates")?;
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
            "INSERT INTO equity_snapshots (ts,equity,drawdown) VALUES (?1,?2,?3)",
            params![now_rfc3339(), d(&equity), d(&drawdown)],
        )?;
        Ok(())
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
        let c = lock(&self.conn);
        let mut written = Vec::new();
        for t in TABLES {
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
