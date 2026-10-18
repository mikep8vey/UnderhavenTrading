-- Shared by both accounts (the SAME signals/edges feed paper and live).
CREATE TABLE IF NOT EXISTS signals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  city TEXT NOT NULL,
  station_icao TEXT NOT NULL,
  strategy TEXT NOT NULL,
  bracket TEXT NOT NULL,
  model_prob TEXT,
  market_ask TEXT,
  market_bid TEXT,
  edge TEXT,
  settlement_check TEXT,
  decision TEXT NOT NULL,
  reason TEXT
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  level TEXT NOT NULL,
  kind TEXT NOT NULL,
  message TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  mode TEXT NOT NULL,
  starting_balance TEXT NOT NULL,
  ending_balance TEXT,
  net_pnl TEXT
);
-- One raw model forecast per market and phase ("today"/"tomorrow"); outcome is filled in after settlement.
-- This is the dataset the probability calibration is learned from.
CREATE TABLE IF NOT EXISTS forecast_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  market_id TEXT NOT NULL,
  phase TEXT NOT NULL,
  city TEXT NOT NULL,
  bracket TEXT NOT NULL,
  event_date TEXT NOT NULL,
  yes_idx INTEGER NOT NULL,
  p_raw REAL NOT NULL,
  p_mkt REAL NOT NULL,
  outcome INTEGER,
  UNIQUE(market_id, phase)
);
