import sqlite3, threading, time
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
  rejection_reason TEXT,
  asset TEXT DEFAULT 'BTC'
);
CREATE TABLE IF NOT EXISTS trades (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER,
  ts TEXT, mode TEXT, market TEXT, yes_order TEXT, no_order TEXT, status TEXT,
  spend REAL, gross_edge REAL, fee_estimate REAL, net_edge REAL, pnl REAL,
  win INTEGER DEFAULT 0, note TEXT,
  asset TEXT DEFAULT 'BTC'
);
CREATE TABLE IF NOT EXISTS scan_stats (
  minute TEXT NOT NULL,
  asset TEXT NOT NULL DEFAULT '*',
  session_id INTEGER,
  scans INTEGER NOT NULL DEFAULT 0,
  markets_seen INTEGER NOT NULL DEFAULT 0,
  best_gross REAL, best_net REAL, min_pair REAL,
  raw_scans INTEGER NOT NULL DEFAULT 0,
  qualified_scans INTEGER NOT NULL DEFAULT 0,
  qual_secs REAL,
  errors INTEGER NOT NULL DEFAULT 0,
  last_error TEXT DEFAULT '',
  PRIMARY KEY (minute, asset)
);
CREATE INDEX IF NOT EXISTS idx_opportunities_session ON opportunities(session_id);
CREATE INDEX IF NOT EXISTS idx_opportunities_ts ON opportunities(ts);
CREATE INDEX IF NOT EXISTS idx_trades_session ON trades(session_id);
CREATE INDEX IF NOT EXISTS idx_trades_ts ON trades(ts);
'''


class _Conn(sqlite3.Connection):
    """`with connect() as c:` now also CLOSES the connection (plain sqlite3 only commits), so file
    descriptors are released immediately instead of waiting for garbage collection."""
    def __exit__(self, *exc):
        try:
            return super().__exit__(*exc)
        finally:
            self.close()


_init_lock = threading.Lock()
_ready = False


def _migrate(c):
    for stmt in [
        "ALTER TABLE opportunities ADD COLUMN session_id INTEGER",
        "ALTER TABLE opportunities ADD COLUMN slug TEXT",
        "ALTER TABLE opportunities ADD COLUMN fee_estimate REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN executable_shares REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN executable_spend REAL DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN qualified INTEGER DEFAULT 0",
        "ALTER TABLE opportunities ADD COLUMN rejection_reason TEXT",
        "ALTER TABLE opportunities ADD COLUMN asset TEXT DEFAULT 'BTC'",
        "ALTER TABLE trades ADD COLUMN session_id INTEGER",
        "ALTER TABLE trades ADD COLUMN fee_estimate REAL DEFAULT 0",
        "ALTER TABLE trades ADD COLUMN win INTEGER DEFAULT 0",
        "ALTER TABLE trades ADD COLUMN asset TEXT DEFAULT 'BTC'",
    ]:
        try:
            c.execute(stmt)
        except sqlite3.OperationalError:
            pass
    # v3.4 -> v3.5: scan_stats gained an `asset` column (one row per minute per coin, plus a '*' total row).
    cols = [r[1] for r in c.execute('PRAGMA table_info(scan_stats)')]
    if 'asset' not in cols:
        c.execute('ALTER TABLE scan_stats RENAME TO scan_stats_old')
        c.executescript(SCHEMA)  # recreates scan_stats in the new shape
        for asset in ('*', 'BTC'):  # everything recorded so far was BTC-only
            c.execute("""INSERT OR IGNORE INTO scan_stats(minute,asset,session_id,scans,markets_seen,best_gross,best_net,min_pair,
                         raw_scans,qualified_scans,qual_secs,errors,last_error)
                         SELECT minute,?,session_id,scans,markets_seen,best_gross,best_net,min_pair,qualified_scans,qualified_scans,
                         CASE WHEN qualified_scans>0 THEN (CAST(substr(minute,15,2) AS INTEGER)%15)*60+30 END,errors,last_error
                         FROM scan_stats_old""", (asset,))
        c.execute('DROP TABLE scan_stats_old')
    c.execute('CREATE INDEX IF NOT EXISTS idx_scan_stats_asset ON scan_stats(asset, minute)')
    c.execute('CREATE INDEX IF NOT EXISTS idx_trades_asset ON trades(asset)')
    c.commit()


def connect():
    global _ready
    # SQLite may need to create/update the DB, WAL and SHM files. The installer
    # deliberately creates this directory and database as the `underhaven`
    # service user, so the long-running bot can persist paper/live history.
    DB_PATH.parent.mkdir(parents=True, exist_ok=True)
    c = sqlite3.connect(DB_PATH, timeout=30, factory=_Conn)
    c.row_factory = sqlite3.Row
    c.execute('PRAGMA journal_mode=WAL')
    c.execute('PRAGMA synchronous=NORMAL')
    if not _ready:
        with _init_lock:
            if not _ready:
                c.executescript(SCHEMA)
                _migrate(c)
                _ready = True
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
    row = tuple(row) + (('BTC',) if len(row) == 16 else ())
    with connect() as c:
        c.execute('''INSERT INTO opportunities(
            session_id,ts,market,slug,yes_ask,no_ask,pair_cost,gross_edge,fee_estimate,net_edge,
            executable_shares,executable_spend,yes_liquidity,no_liquidity,qualified,rejection_reason,asset
        ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)''', row)


def add_trade(row):
    row = tuple(row) + (('BTC',) if len(row) == 14 else ())
    with connect() as c:
        c.execute('''INSERT INTO trades(
            session_id,ts,mode,market,yes_order,no_order,status,spend,gross_edge,fee_estimate,net_edge,pnl,win,note,asset
        ) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)''', row)


def recent_opportunities(limit=100):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM opportunities ORDER BY id DESC LIMIT ?', (limit,))]


def recent_trades(limit=100, asset=None):
    with connect() as c:
        return [dict(r) for r in c.execute("SELECT * FROM trades WHERE (? IS NULL OR COALESCE(asset,'BTC')=?) ORDER BY id DESC LIMIT ?", (asset, asset, limit))]


def session_opportunity_count(session_id):
    with connect() as c:
        return c.execute('SELECT COUNT(*) FROM opportunities WHERE session_id=?', (session_id,)).fetchone()[0]


# ---- Scan history (powers the /report page) ---------------------------------

def _minute(ts=None):
    return time.strftime('%Y-%m-%dT%H:%MZ', time.gmtime(ts))


_UPSERT = '''INSERT INTO scan_stats(minute,asset,session_id,scans,markets_seen,best_gross,best_net,min_pair,raw_scans,qualified_scans,qual_secs,errors,last_error)
    VALUES(?,?,?,1,?,?,?,?,?,?,?,?,?)
    ON CONFLICT(minute,asset) DO UPDATE SET
      scans=scans+1,
      session_id=excluded.session_id,
      markets_seen=MAX(markets_seen,excluded.markets_seen),
      best_gross=CASE WHEN best_gross IS NULL THEN excluded.best_gross WHEN excluded.best_gross IS NULL THEN best_gross ELSE MAX(best_gross,excluded.best_gross) END,
      best_net=CASE WHEN best_net IS NULL THEN excluded.best_net WHEN excluded.best_net IS NULL THEN best_net ELSE MAX(best_net,excluded.best_net) END,
      min_pair=CASE WHEN min_pair IS NULL THEN excluded.min_pair WHEN excluded.min_pair IS NULL THEN min_pair ELSE MIN(min_pair,excluded.min_pair) END,
      raw_scans=raw_scans+excluded.raw_scans,
      qualified_scans=qualified_scans+excluded.qualified_scans,
      qual_secs=CASE WHEN qual_secs IS NULL THEN excluded.qual_secs WHEN excluded.qual_secs IS NULL THEN qual_secs ELSE MIN(qual_secs,excluded.qual_secs) END,
      errors=errors+excluded.errors,
      last_error=CASE WHEN excluded.last_error<>'' THEN excluded.last_error ELSE last_error END'''


def record_scan(session_id, markets, error='', coins=None, now=None):
    """Fold one scan into the current UTC-minute rows: a '*' total row plus one row per coin. One row per
    minute per coin keeps the table small (~10k rows/day for 7 coins) yet answers, for any window: how many
    scans ran, the best gross/NET edge, how often a gap looked profitable (raw) and how often it survived
    every guard and the re-check (qualified), and how far into its 15-minute window that happened."""
    now = now or time.time()
    groups = {c: [] for c in (coins or [])}
    for m in markets: groups.setdefault(m.get('asset', 'BTC'), []).append(m)
    with connect() as c:
        for asset, ms in [('*', list(markets))] + list(groups.items()):
            # Gaps that vanished on the re-check are phantoms: counted in raw_scans, but kept out of the
            # best-edge figures so the report cannot be flattered by a book that was never really there.
            ok = [x for x in ms if not x.get('disproved')]
            gross = [x['gross_edge'] for x in ok if x.get('gross_edge') is not None]
            net = [x['net_edge'] for x in ok if x.get('net_edge') is not None]
            pair = [x['pair_cost'] for x in ok if x.get('pair_cost') is not None]
            raw = 1 if any(x.get('raw_qualified', x.get('qualified')) for x in ms) else 0
            qual = [x for x in ms if x.get('qualified')]
            secs = min([now - x['start_ts'] for x in qual if x.get('start_ts')], default=None)
            c.execute(_UPSERT, (_minute(now), asset, session_id, len(ms),
                                max(gross) if gross else None, max(net) if net else None, min(pair) if pair else None,
                                raw, 1 if qual else 0, secs,
                                1 if (error and asset == '*') else 0, error if asset == '*' else ''))


def prune(days=30):
    """Drop old history so the database cannot grow without limit on a long-running VPS."""
    cutoff = time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(time.time() - days * 86400))
    with connect() as c:
        c.execute('DELETE FROM scan_stats WHERE minute < ?', (cutoff[:16] + 'Z',))
        c.execute('DELETE FROM opportunities WHERE ts < ?', (cutoff,))


def report_data(since_ts, asset=None):
    """Everything the report page needs for [since_ts, now]. since_ts is an ISO UTC string such as
    2026-10-04T00:09:51Z. `asset` narrows every section to one coin (None = all coins)."""
    since_min = since_ts[:16] + 'Z'
    key = asset or '*'
    with connect() as c:
        tot = dict(c.execute('''SELECT COUNT(*) AS minutes, COALESCE(SUM(scans),0) AS scans,
              COALESCE(MAX(markets_seen),0) AS markets, MAX(best_gross) AS best_gross, MAX(best_net) AS best_net,
              MIN(min_pair) AS min_pair, COALESCE(SUM(raw_scans),0) AS raw_scans, COALESCE(SUM(qualified_scans),0) AS qualified_scans,
              COALESCE(SUM(errors),0) AS errors, MIN(minute) AS first_minute, MAX(minute) AS last_minute
              FROM scan_stats WHERE minute>=? AND asset=?''', (since_min, key)).fetchone())
        hours = [dict(r) for r in c.execute('''SELECT substr(minute,1,13)||':00:00Z' AS hour, COUNT(*) AS minutes,
              SUM(scans) AS scans, MAX(markets_seen) AS markets, MAX(best_gross) AS best_gross, MAX(best_net) AS best_net,
              MIN(min_pair) AS min_pair, SUM(raw_scans) AS raw_scans, SUM(qualified_scans) AS qualified_scans, SUM(errors) AS errors
              FROM scan_stats WHERE minute>=? AND asset=? GROUP BY substr(minute,1,13) ORDER BY hour DESC''', (since_min, key))]
        pt = {r['h']: (r['n'], r['pnl']) for r in c.execute('''SELECT substr(ts,1,13)||':00:00Z' AS h, COUNT(*) AS n,
              COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='PAPER' AND ts>=? AND (? IS NULL OR asset=?)
              GROUP BY substr(ts,1,13)''', (since_ts, asset, asset))}
        for h in hours:
            h['paper_trades'], h['paper_pnl'] = pt.get(h['hour'], (0, 0.0))
        best = [dict(r) for r in c.execute('''SELECT minute,asset,best_gross,best_net,min_pair,qualified_scans FROM scan_stats
              WHERE minute>=? AND asset<>'*' AND (? IS NULL OR asset=?) AND best_net IS NOT NULL
              ORDER BY best_net DESC LIMIT 10''', (since_min, asset, asset))]
        errs = [dict(r) for r in c.execute('''SELECT minute,errors,last_error FROM scan_stats
              WHERE minute>=? AND asset='*' AND last_error<>'' ORDER BY minute DESC LIMIT 10''', (since_min,))]
        tr = {r['asset']: (r['n'], r['pnl']) for r in c.execute('''SELECT COALESCE(asset,'BTC') AS asset, COUNT(*) AS n,
              COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='PAPER' AND ts>=? GROUP BY COALESCE(asset,'BTC')''', (since_ts,))}
        by_asset = []
        for r in c.execute('''SELECT asset, SUM(scans) AS scans, MAX(best_gross) AS best_gross, MAX(best_net) AS best_net,
              SUM(raw_scans) AS raw, SUM(qualified_scans) AS confirmed FROM scan_stats
              WHERE minute>=? AND asset<>'*' GROUP BY asset ORDER BY asset''', (since_min,)):
            n, pnl = tr.get(r['asset'], (0, 0.0))
            by_asset.append(dict(r, trades=n, pnl=pnl))
        buckets = [0] * 16   # [0] = before the window opened, [1..15] = minute 0..14 of the window
        for r in c.execute('''SELECT qual_secs FROM scan_stats WHERE minute>=? AND asset<>'*' AND (? IS NULL OR asset=?)
              AND qualified_scans>0 AND qual_secs IS NOT NULL''', (since_min, asset, asset)):
            v = r['qual_secs']; buckets[0 if v < 0 else min(14, int(v // 60)) + 1] += 1
        timing = [{'label': 'pre' if i == 0 else str(i - 1), 'n': n} for i, n in enumerate(buckets)]
        paper = dict(c.execute("SELECT COUNT(*) AS n, COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='PAPER' AND ts>=? AND (? IS NULL OR asset=?)", (since_ts, asset, asset)).fetchone())
        live = dict(c.execute("SELECT COUNT(*) AS n, COALESCE(SUM(CASE WHEN status='BOTH_LEGS_FILLED' THEN 1 ELSE 0 END),0) AS filled, COALESCE(SUM(pnl),0) AS pnl FROM trades WHERE mode='LIVE' AND ts>=?", (since_ts,)).fetchone())
        sessions = c.execute('SELECT COUNT(*) FROM paper_sessions WHERE started_at>=?', (since_ts,)).fetchone()[0]
        trades = [dict(r) for r in c.execute('SELECT ts,mode,COALESCE(asset,\'BTC\') AS asset,market,status,spend,net_edge,pnl FROM trades WHERE ts>=? AND (? IS NULL OR asset=?) ORDER BY id DESC LIMIT 25', (since_ts, asset, asset))]
    return dict(total=tot, hours=hours, best=best, errors=errs, paper=paper, live=live, sessions=sessions,
                trades=trades, assets=by_asset, timing=timing)


def scan_rows(since_ts, asset=None):
    with connect() as c:
        return [dict(r) for r in c.execute('SELECT * FROM scan_stats WHERE minute>=? AND asset=? ORDER BY minute', (since_ts[:16] + 'Z', asset or '*'))]
