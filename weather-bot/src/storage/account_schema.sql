-- Created once per account; {P} is replaced by "paper_" and "live_".
CREATE TABLE IF NOT EXISTS {P}orders (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  signal_id INTEGER,
  exchange_id TEXT,
  side TEXT NOT NULL,
  price TEXT NOT NULL,
  size TEXT NOT NULL,
  post_only INTEGER NOT NULL,
  status TEXT NOT NULL,
  filled TEXT NOT NULL DEFAULT '0',
  fee TEXT NOT NULL DEFAULT '0'
);
CREATE TABLE IF NOT EXISTS {P}fills (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  order_id INTEGER NOT NULL,
  ts TEXT NOT NULL,
  price TEXT NOT NULL,
  size TEXT NOT NULL,
  is_maker INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS {P}positions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  city TEXT NOT NULL,
  bracket TEXT NOT NULL,
  side TEXT NOT NULL,
  size TEXT NOT NULL,
  avg_price TEXT NOT NULL,
  status TEXT NOT NULL,
  market_id TEXT NOT NULL,
  token_id TEXT NOT NULL UNIQUE,
  outcome_idx INTEGER NOT NULL,
  event_key TEXT NOT NULL,
  pnl TEXT NOT NULL DEFAULT '0'
);
CREATE TABLE IF NOT EXISTS {P}rebates (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  maker_volume TEXT NOT NULL,
  accrued_usdc TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS {P}equity_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  equity TEXT NOT NULL,
  drawdown TEXT NOT NULL
);
