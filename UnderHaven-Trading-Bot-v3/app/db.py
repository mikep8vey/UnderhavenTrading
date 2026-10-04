import sqlite3, time
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
CREATE TABLE IF NOT EXISTS scan_stats (
  minute TEXT PRIMARY KEY,
  session_id INTEGER,
  scans INTEGER NOT NULL DEFAULT 0,
  markets_seen INTEGER NOT NULL DEFAULT 0,
  best_gross REAL, best_net REAL, min_pair REAL,
  qualified_scans INTEGER NOT NULL DEFAULT 0,
  errors INTEGER NOT NULL DEFAULT 0,
  last_error TEXT DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_opportunities_session ON opportunities(session_id);
CREATE INDEX IF NOT EXISTS idx_opportunities_ts ON opportunities(ts);
CREATE INDEX IF NOT EXISTS idx_trades_session ON trades(session_id);
CREATE INDEX IF NOT EXISTS idx_trades_ts ON trades(ts);
'''


def connect():
    # SQLite may need to create/update the DB, WAL and SHM files. The installer
    # deliberately creates this directory and database as the `underhaven`
    # service user, so the long-running bot can persist paper/live history.
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


# ---- Scan history (powers the /report page) ---------------------------------

def _minute(ts=None):
    return time.strftime('%Y-%m-%dT%H:%MZ', time.gmtime(ts))


def record_scan(session_id, markets, error=''):
    """Fold one scan into the current UTC-minute row. One row per minute keeps the
    table tiny (~1,440 rows/day) while still showing, for any window, how many scans
    ran, the best gross/net edge seen, and whether anything ever qualified."""
    gross = [x['gross_edge'] for x in markets if x.get('gross_edge') is not None]
    net = [x['net_edge'] for x in markets if x.get('net_edge') is not None]
    pair = [x['pair_cost'] for x in markets if x.get('pair_cost') is not None]
    q = 1 if any(x.get('qualified') for x in markets) else 0
    row = (_minute(), session_id, len(markets),
           max(gross) if gross else None, max(net) if net else None, min(pair) if pair else None,
           q, 1 if error else 0, error or '')
    with connect() as c:
        c.execute('''INSERT INTO scan_stats(minute,session_id,scans,markets_seen,best_gross,best_net,min_pair,qualified_scans,errors,last_error)
            VALUES(?,?,1,?,?,?,?,?,?,?)
            ON CONFLICT(minute) DO UPDATE SET
              scans=scans+1,
              session_id=excluded.session_id,
              markets_seen=MAX(markets_seen,excluded.markets_seen),
              best_gross=CASE WHEN best_gross IS NULL THEN excluded.best_gross WHEN excluded.best_gross IS NULL THEN best_gross ELSE MAX(best_gross,excluded.best_gross) END,
              best_net=CASE WHEN best_net IS NULL THEN excluded.best_net WHEN excluded.best_net IS NULL THEN best_net ELSE MAX(best_net,excluded.best_net) END,
              min_pair=CASE WHEN min_pair IS NULL THEN excluded.min_pair WHEN excluded.min_pair IS NULL THEN min_pair ELSE MIN(min_pair,excluded.min_pair) END,
              qualified_scans=qualified_scans+excluded.qualified_scans,
              errors=errors+excluded.errors,
              last_error=CASE WHEN excluded.last_error<>'' THEN excluded.last_error ELSE last_error END''', row)


def prune(days=30):
    """Drop old history so the database cannot grow without limit on a long-running VPS."""
    cutoff = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() - days * 86400))
    with connect() as c:
        c.execute('DELETE FROM scan_stats WHERE minute < ?', (cutoff[:16] + 'Z',))
        c.execute('DELETE FROM opportunities WHERE ts < ?', (cutoff,))


def report_data(since_ts):
    """Everything the report page needs for the window [since_ts, now]. since_ts is
    an ISO UTC string like 2026-10-04T00:09:51Z."""
    since_min = since_ts[:16] + 'Z'
    with connect() as c:
        tot = dict(c.execute('''SELECT COUNT(*) AS minutes, COALESCE(SUM(scans),0) AS scans,
              COALESCE(MAX(markets_seen),0) AS markets, MAX(best_gross) AS best_gross, MAX(best_net) AS best_net,
              MIN(min_pair) AS min_pair, COALESCE(SUM(qualified_scans),0) AS qualified_scans,
              COALESCE(SUM(errors),0) AS errors, MIN(minute) AS first_minute, MAX(minute) AS last_minute
              FROM scan_stats WHERE minute>=?''', (since_min,)).fetchone())
        hours = [dict(r) for r in c.execute('''SELECT substr(minute,1,13)||':00:00Z' AS hour, COUNT(*) AS minutes,
              SUM(scans) AS scans, MAX(markets_seen) AS markets, MAX(best_gross) AS best_gross, MAX(best_net) AS best_net,
              MIN(min_pair) AS min_pair, SUM(qualified_scans) AS qualified_scans, SUM(errors) AS errors
              FROM scan_stats WHERE minute>=? GROUP BY substr(minute,1,13) ORDER BY hour DESC''', (since_min,))]
        pt = {r['h']: (r['n'], r['pnl']) for r in c.execute('''SELECT substr(ts,1,13)||':00:00Z' AS h, COUNT(*) AS n,
              COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='PAPER' AND ts>=? GROUP BY substr(ts,1,13)''', (since_ts,))}
        for h in hours:
            h['paper_trades'], h['paper_pnl'] = pt.get(h['hour'], (0, 0.0))
        best = [dict(r) for r in c.execute('''SELECT minute,best_gross,best_net,min_pair,qualified_scans FROM scan_stats
              WHERE minute>=? AND best_net IS NOT NULL ORDER BY best_net DESC LIMIT 10''', (since_min,))]
        errs = [dict(r) for r in c.execute('''SELECT minute,errors,last_error FROM scan_stats
              WHERE minute>=? AND last_error<>'' ORDER BY minute DESC LIMIT 10''', (since_min,))]
        paper = dict(c.execute("SELECT COUNT(*) AS n, COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='PAPER' AND ts>=?", (since_ts,)).fetchone())
        live = dict(c.execute("SELECT COUNT(*) AS n, COALESCE(SUM(CASE WHEN status='BOTH_LEGS_FILLED' THEN 1 ELSE 0 END),0) AS filled, COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='LIVE' AND ts>=?", (since_ts,)).fetchone())
        sessions = c.execute('SELECT COUNT(*) FROM paper_sessions WHERE started_at>=?', (since_ts,)).fetchone()[0]
        trades = [dict(r) for r in c.execute('SELECT ts,mode,market,status,spend,net_edge,pnl FROM trades WHERE ts>=? ORDER BY id DESC LIMIT 25', (since_ts,))]
    return dict(total=tot, hours=hours, best=best, errors=errs, paper=paper, live=live, sessions=sessions, trades=trades)


def scan_rows(since_ts):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM scan_stats WHERE minute>=? ORDER BY minute', (since_ts[:16] + 'Z',))]
