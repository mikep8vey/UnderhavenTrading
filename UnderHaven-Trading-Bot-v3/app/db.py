import sqlite3
from .config import DB_PATH

SCHEMA = '''
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS credentials (
  id INTEGER PRIMARY KEY CHECK(id=1),
  private_key TEXT, seed_phrase TEXT, api_key TEXT, api_secret TEXT, api_passphrase TEXT,
  funder TEXT, signer TEXT, signature_type INTEGER DEFAULT 0
);
CREATE TABLE IF NOT EXISTS paper_sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  started_at TEXT NOT NULL,
  ended_at TEXT,
  status TEXT NOT NULL DEFAULT 'RUNNING',
  scans INTEGER NOT NULL DEFAULT 0,
  markets_seen INTEGER NOT NULL DEFAULT 0,
  opportunities INTEGER NOT NULL DEFAULT 0,
  paper_trades INTEGER NOT NULL DEFAULT 0,
  wins INTEGER NOT NULL DEFAULT 0,
  losses INTEGER NOT NULL DEFAULT 0,
  simulated_spend REAL NOT NULL DEFAULT 0,
  simulated_pnl REAL NOT NULL DEFAULT 0,
  notes TEXT
);
CREATE TABLE IF NOT EXISTS opportunities (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER,
  ts TEXT, market TEXT, slug TEXT, yes_ask REAL, no_ask REAL, pair_cost REAL,
  gross_edge REAL, fee_estimate REAL, net_edge REAL, executable_shares REAL,
  executable_spend REAL, yes_liquidity REAL, no_liquidity REAL, qualified INTEGER DEFAULT 0,
  rejection_reason TEXT
);
CREATE TABLE IF NOT EXISTS trades (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER,
  ts TEXT, mode TEXT, market TEXT, yes_order TEXT, no_order TEXT, status TEXT,
  spend REAL, gross_edge REAL, fee_estimate REAL, net_edge REAL, pnl REAL,
  win INTEGER DEFAULT 0, note TEXT
);
CREATE INDEX IF NOT EXISTS idx_opportunities_session ON opportunities(session_id);
CREATE INDEX IF NOT EXISTS idx_opportunities_ts ON opportunities(ts);
CREATE INDEX IF NOT EXISTS idx_trades_session ON trades(session_id);
CREATE INDEX IF NOT EXISTS idx_trades_ts ON trades(ts);
'''


def connect():
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    c = sqlite3.connect(DB_PATH, timeout=30)
    c.row_factory = sqlite3.Row
    c.execute('PRAGMA journal_mode=WAL')
    c.execute('PRAGMA synchronous=NORMAL')
    c.executescript(SCHEMA)
    # Lightweight migrations from v2.
    for stmt in [
        "ALTER TABLE opportunities ADD COLUMN session_id INTEGER",
        "ALTER TABLE opportunities ADD COLUMN slug TEXT",
        "ALTER TABLE opportunities ADD COLUMN fee_estimate REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN executable_shares REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN executable_spend REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN qualified INTEGER DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN rejection_reason TEXT",
        "ALTER TABLE trades ADD COLUMN session_id INTEGER",
        "ALTER TABLE trades ADD COLUMN fee_estimate REAL DEFAULT 0",
        "ALTER TABLE trades ADD COLUMN win INTEGER DEFAULT 0",
    ]:
        try:
            c.execute(stmt)
        except sqlite3.OperationalError:
            pass
    c.commit()
    return c


def get_setting(key, default=None):
    with connect() as c:
        r = c.execute('SELECT value FROM settings WHERE key=?', (key,)).fetchone()
        return r['value'] if r else default


def set_setting(key, value):
    with connect() as c:
        c.execute('INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value', (key, str(value)))


def start_paper_session():
    import time
    ts = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())
    with connect() as c:
        c.execute("UPDATE paper_sessions SET status='ENDED', ended_at=? WHERE status='RUNNING'", (ts,))
        cur = c.execute('INSERT INTO paper_sessions(started_at,status) VALUES(?,?)', (ts, 'RUNNING'))
        return cur.lastrowid


def update_paper_session(session_id, **fields):
    allowed = {'scans','markets_seen','opportunities','paper_trades','wins','losses','simulated_spend','simulated_pnl','status','ended_at','notes'}
    data = {k:v for k,v in fields.items() if k in allowed}
    if not data: return
    sql = ', '.join(f'{k}=?' for k in data)
    with connect() as c:
        c.execute(f'UPDATE paper_sessions SET {sql} WHERE id=?', (*data.values(), session_id))


def current_paper_session():
    with connect() as c:
        r = c.execute("SELECT * FROM paper_sessions WHERE status='RUNNING' ORDER BY id DESC LIMIT 1").fetchone()
        return dict(r) if r else None


def latest_paper_session():
    with connect() as c:
        r = c.execute('SELECT * FROM paper_sessions ORDER BY id DESC LIMIT 1').fetchone()
        return dict(r) if r else None


def list_paper_sessions(limit=20):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM paper_sessions ORDER BY id DESC LIMIT ?', (limit,))]


def add_opportunity(row):
    with connect() as c:
        c.execute('''INSERT INTO opportunities(
            session_id,ts,market,slug,yes_ask,no_ask,pair_cost,gross_edge,fee_estimate,net_edge,
            executable_shares,executable_spend,yes_liquidity,no_liquidity,qualified,rejection_reason
        ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)''', row)


def add_trade(row):
    with connect() as c:
        c.execute('''INSERT INTO trades(
            session_id,ts,mode,market,yes_order,no_order,status,spend,gross_edge,fee_estimate,net_edge,pnl,win,note
        ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)''', row)


def recent_opportunities(limit=100):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM opportunities ORDER BY id DESC LIMIT ?', (limit,))]


def recent_trades(limit=100):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM trades ORDER BY id DESC LIMIT ?', (limit,))]


def session_opportunity_count(session_id):
    with connect() as c:
        return c.execute('SELECT COUNT(*) FROM opportunities WHERE session_id=?', (session_id,)).fetchone()[0]
