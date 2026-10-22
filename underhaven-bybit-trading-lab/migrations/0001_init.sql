-- UNDERHAVEN BYBIT TRADING LAB - initial schema
-- All monetary values are REAL paper-ledger units, rounded to 1e-8 by the app.
-- This database never represents a live exchange account.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    disabled      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS sessions (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash    TEXT NOT NULL UNIQUE,
    csrf_token    TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    last_seen_ms  INTEGER NOT NULL,
    revoked       INTEGER NOT NULL DEFAULT 0,
    ip            TEXT,
    user_agent    TEXT
);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);

CREATE TABLE IF NOT EXISTS login_attempts (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms    INTEGER NOT NULL,
    ip       TEXT NOT NULL,
    username TEXT NOT NULL,
    success  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_login_attempts ON login_attempts(ip, ts_ms);

CREATE TABLE IF NOT EXISTS audit_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms      INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    actor      TEXT NOT NULL,
    detail     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_events(ts_ms);

-- Append-only audit trail: updates and deletes are rejected by the engine.
CREATE TRIGGER IF NOT EXISTS audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit_events is append-only');
END;

CREATE TRIGGER IF NOT EXISTS audit_events_no_delete
BEFORE DELETE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit_events is append-only');
END;

CREATE TABLE IF NOT EXISTS wallets (
    strategy         TEXT PRIMARY KEY,
    initial_equity   REAL NOT NULL,
    cash             REAL NOT NULL,
    realized         REAL NOT NULL DEFAULT 0,
    fees             REAL NOT NULL DEFAULT 0,
    funding          REAL NOT NULL DEFAULT 0,
    equity           REAL NOT NULL,
    peak_equity      REAL NOT NULL,
    day_start_equity REAL NOT NULL,
    day_date         TEXT NOT NULL,
    updated_at_ms    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS positions (
    id               TEXT PRIMARY KEY,
    strategy         TEXT NOT NULL,
    symbol           TEXT NOT NULL,
    direction        TEXT NOT NULL,
    qty              REAL NOT NULL,
    entry_price      REAL NOT NULL,
    entry_time_ms    INTEGER NOT NULL,
    mark_price       REAL NOT NULL,
    stop             REAL NOT NULL,
    initial_stop     REAL NOT NULL,
    target           REAL NOT NULL,
    trailing_distance REAL NOT NULL,
    highest_price    REAL NOT NULL,
    lowest_price     REAL NOT NULL,
    max_hold_until_ms INTEGER NOT NULL,
    fees_entry       REAL NOT NULL,
    funding          REAL NOT NULL DEFAULT 0,
    notional         REAL NOT NULL,
    signal_id        TEXT,
    status           TEXT NOT NULL DEFAULT 'open',
    updated_at_ms    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_positions_strategy ON positions(strategy, status);
-- At most one open position per independent strategy wallet.
CREATE UNIQUE INDEX IF NOT EXISTS one_open_position_per_strategy
    ON positions(strategy) WHERE status = 'open';

CREATE TABLE IF NOT EXISTS signals (
    id                    TEXT PRIMARY KEY,
    strategy              TEXT NOT NULL,
    symbol                TEXT NOT NULL,
    timeframe             TEXT NOT NULL,
    direction             TEXT NOT NULL,
    candle_time_ms        INTEGER NOT NULL,
    signal_time_ms        INTEGER NOT NULL,
    entry_reference       REAL NOT NULL,
    proposed_entry        REAL NOT NULL,
    stop                  REAL NOT NULL,
    target                REAL NOT NULL,
    atr                   REAL NOT NULL,
    snapshot_json         TEXT NOT NULL,
    expected_fees         REAL NOT NULL,
    expected_funding      REAL NOT NULL,
    expected_spread_cost  REAL NOT NULL,
    expected_slippage     REAL NOT NULL,
    expected_costs_total  REAL NOT NULL,
    win_probability       REAL,
    probability_source    TEXT NOT NULL,
    estimated_net_edge    REAL,
    decision              TEXT NOT NULL,
    rejection_reason      TEXT,
    strategy_version      TEXT NOT NULL,
    created_at_ms         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_signals_strategy ON signals(strategy, created_at_ms);

CREATE TABLE IF NOT EXISTS trades (
    id            TEXT PRIMARY KEY,
    strategy      TEXT NOT NULL,
    symbol        TEXT NOT NULL,
    direction     TEXT NOT NULL,
    qty           REAL NOT NULL,
    entry_price   REAL NOT NULL,
    exit_price    REAL NOT NULL,
    entry_time_ms INTEGER NOT NULL,
    exit_time_ms  INTEGER NOT NULL,
    gross_pnl     REAL NOT NULL,
    fees          REAL NOT NULL,
    funding       REAL NOT NULL,
    slippage_est  REAL NOT NULL,
    net_pnl       REAL NOT NULL,
    close_reason  TEXT NOT NULL,
    bars_held     INTEGER NOT NULL,
    simulated_fill TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_trades_strategy ON trades(strategy, exit_time_ms);

CREATE TABLE IF NOT EXISTS fills (
    id              TEXT PRIMARY KEY,
    strategy        TEXT NOT NULL,
    position_id     TEXT,
    ts_ms           INTEGER NOT NULL,
    side            TEXT NOT NULL,
    qty             REAL NOT NULL,
    price           REAL NOT NULL,
    fee             REAL NOT NULL,
    kind            TEXT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS funding_events (
    strategy         TEXT NOT NULL,
    symbol           TEXT NOT NULL,
    position_id      TEXT NOT NULL,
    funding_time_ms  INTEGER NOT NULL,
    rate             REAL NOT NULL,
    payment          REAL NOT NULL,
    applied_at_ms    INTEGER NOT NULL,
    PRIMARY KEY (strategy, symbol, position_id, funding_time_ms)
);

CREATE TABLE IF NOT EXISTS equity_points (
    strategy     TEXT NOT NULL,
    ts_ms        INTEGER NOT NULL,
    equity       REAL NOT NULL,
    cash         REAL NOT NULL,
    unrealized   REAL NOT NULL,
    drawdown_pct REAL NOT NULL,
    PRIMARY KEY (strategy, ts_ms)
);

CREATE TABLE IF NOT EXISTS candles (
    symbol        TEXT NOT NULL,
    interval      TEXT NOT NULL,
    open_time_ms  INTEGER NOT NULL,
    open          REAL NOT NULL,
    high          REAL NOT NULL,
    low           REAL NOT NULL,
    close         REAL NOT NULL,
    volume        REAL NOT NULL,
    turnover      REAL NOT NULL,
    confirm       INTEGER NOT NULL,
    source        TEXT NOT NULL,
    ingested_at_ms INTEGER NOT NULL,
    PRIMARY KEY (symbol, interval, open_time_ms)
);

CREATE TABLE IF NOT EXISTS funding_rates (
    symbol          TEXT NOT NULL,
    funding_time_ms INTEGER NOT NULL,
    rate            REAL NOT NULL,
    source          TEXT NOT NULL,
    fetched_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (symbol, funding_time_ms)
);

CREATE TABLE IF NOT EXISTS instruments (
    symbol        TEXT PRIMARY KEY,
    payload_json  TEXT NOT NULL,
    fetched_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS scan_snapshots (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms     INTEGER NOT NULL,
    rows_json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS backtests (
    run_id         TEXT PRIMARY KEY,
    strategy       TEXT NOT NULL,
    symbol         TEXT NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    fingerprint    TEXT NOT NULL,
    report_json    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS credentials (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    api_key_enc      TEXT NOT NULL,
    api_secret_enc   TEXT NOT NULL,
    read_only        INTEGER NOT NULL,
    uid              TEXT,
    note             TEXT,
    created_at_ms    INTEGER NOT NULL,
    last_verified_ms INTEGER
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS engine_state (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    paused            INTEGER NOT NULL DEFAULT 0,
    pause_reason      TEXT,
    emergency_stop    INTEGER NOT NULL DEFAULT 0,
    emergency_reason  TEXT,
    updated_at_ms     INTEGER NOT NULL
);
INSERT OR IGNORE INTO engine_state (id, paused, emergency_stop, updated_at_ms)
VALUES (1, 0, 0, 0);

INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '1');
INSERT OR IGNORE INTO meta (key, value) VALUES ('paper_only', 'true');
