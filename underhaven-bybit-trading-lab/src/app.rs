//! Application state, background orchestration and dashboard aggregation.

use crate::auth::{load_master_key, Auth};
use crate::backtest::{self, BacktestInput, BacktestVariant};
use crate::bybit::{clock_offset_ms, ApiCredentials, PublicClient};
use crate::config::{AppConfig, CostConfig, RiskConfig, StrategyConfigs, BUILD_VERSION};
use crate::market::{merge_seed_symbols, scan, MarketCache, ScannerConfig};
use crate::model::{
    AuditEvent, BybitStatus, ConfigView, CostConfigView, DashboardSnapshot, EquityPoint,
    Instrument, Interval, PaperPosition, RiskConfigView, RiskStatus, ScanRow, StrategyId,
    StrategySummary, SystemStatus, WsEventView,
};
use crate::paper::{EngineConfig, PaperEngine};
use crate::store::{stats_from_closed, Store};
use crate::ws::{self, WsStatus};
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

pub const GIT_COMMIT: &str = match option_env!("UNDERHAVEN_GIT_COMMIT") {
    Some(value) => value,
    None => "unknown",
};

pub struct AppState {
    pub cfg: Arc<AppConfig>,
    pub store: Store,
    pub auth: Auth,
    pub engine: Arc<PaperEngine>,
    pub cache: Arc<RwLock<MarketCache>>,
    pub ws_status: Arc<RwLock<WsStatus>>,
    pub scan: Arc<RwLock<Vec<ScanRow>>>,
    pub bybit: Arc<RwLock<BybitStatus>>,
    pub selected: Arc<RwLock<Vec<String>>>,
    pub started_at_ms: i64,
    pub public_rest_ok: Arc<AtomicBool>,
    pub clock_offset_ms: Arc<AtomicI64>,
    pub backtest_slots: Arc<Semaphore>,
    pub rest_client: PublicClient,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BacktestRequest {
    pub strategy: String,
    pub symbol: String,
    pub interval: String,
    #[serde(default)]
    pub candles: Option<usize>,
}

impl AppState {
    pub async fn dashboard(&self) -> Result<DashboardSnapshot> {
        let now = Utc::now().timestamp_millis();
        let runtime = self.engine.runtime_snapshot();
        let ws = self.ws_status.read().await.clone();
        let bybit = self.bybit.read().await.clone();
        let wallets = self.store.wallets().await?;
        let positions = self.store.open_positions().await?;
        let recent_trades = self.store.closed_trades(None, 40).await?;
        let audit = self.store.recent_audit(20).await?;
        let scan_rows = self.scan.read().await.clone();

        let live_unrealized: f64 = positions
            .iter()
            .map(|p| (p.mark_price - p.entry_price) * p.qty * p.direction.sign())
            .sum();
        let equity: f64 = wallets.iter().map(|w| w.equity).sum();
        let cash: f64 = wallets.iter().map(|w| w.cash).sum();
        let peak: f64 = wallets.iter().map(|w| w.peak_equity).sum();
        let drawdown = if peak > 0.0 {
            ((peak - equity) / peak * 100.0).max(0.0)
        } else {
            0.0
        };
        let daily_pnl: f64 = wallets.iter().map(|w| w.equity - w.day_start_equity).sum();
        let exposure: f64 = positions.iter().map(|p| p.notional).sum();

        let mut strategies = Vec::new();
        let backtests = self.store.list_backtests(40).await.unwrap_or_default();
        for strategy in StrategyId::ALL {
            let wallet = wallets
                .iter()
                .find(|w| w.strategy == strategy)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing wallet for {strategy}"))?;
            let trades = self.store.closed_trades(Some(strategy), 1000).await?;
            let curve = self.store.equity_curve(strategy, 600).await?;
            let stats = stats_from_closed(strategy, &trades, &curve, wallet.initial_equity);
            let forward_status = if stats.trades == 0 {
                "NOT YET VALIDATED: 0 closed paper trades".to_string()
            } else if stats.trades < 30 {
                format!("INSUFFICIENT DATA: {} closed paper trades", stats.trades)
            } else if stats.trades < 100 {
                format!("EXPERIMENTAL: {} closed paper trades", stats.trades)
            } else {
                format!("FORWARD SAMPLE: {} closed paper trades", stats.trades)
            };
            let oos_status = oos_status_for(&backtests, strategy);
            strategies.push(StrategySummary {
                strategy,
                name: strategy.display_name().to_string(),
                enabled: self.cfg.strategies.enabled(strategy),
                timeframe: self.cfg.strategies.timeframe(strategy).label().to_string(),
                initial_equity: wallet.initial_equity,
                equity: wallet.equity,
                cash: wallet.cash,
                realized: wallet.realized,
                unrealized: curve.last().map(|p| p.unrealized).unwrap_or(0.0),
                daily_pnl: wallet.equity - wallet.day_start_equity,
                fees: wallet.fees,
                funding: wallet.funding,
                // Recorded per-trade slippage estimates for the closed trades
                // loaded for this card (not a re-derived market measurement).
                estimated_slippage: trades.iter().map(|t| t.slippage_est).sum(),
                open_positions: positions.iter().filter(|p| p.strategy == strategy).count(),
                trades: stats.trades,
                win_rate: stats.win_rate,
                profit_factor: stats.profit_factor,
                expectancy: stats.expectancy,
                max_drawdown_pct: stats.max_drawdown_pct,
                oos_status,
                forward_status,
                equity_curve: curve,
                stats,
            });
        }

        let mut blocked = Vec::new();
        if runtime.paused {
            blocked.push("manually paused".to_string());
        }
        if runtime.emergency_stop {
            blocked.push("emergency stop active".to_string());
        }
        if runtime.daily_locked {
            blocked.push("daily loss lockout".to_string());
        }
        if runtime.drawdown_locked {
            blocked.push("drawdown lockout".to_string());
        }
        if let Some(until) = runtime.cooldown_until_ms {
            if now < until {
                blocked.push(format!("cooldown until {until}"));
            }
        }
        if !ws.healthy(now, 15_000) {
            blocked.push(format!("market feed unhealthy: {}", ws.detail));
        }

        let risk = RiskStatus {
            risk_per_trade_pct: self.cfg.risk.risk_per_trade_pct,
            daily_loss_limit_pct: self.cfg.risk.daily_loss_limit_pct,
            drawdown_limit_pct: self.cfg.risk.drawdown_limit_pct,
            max_open_positions: self.cfg.risk.max_open_positions,
            max_total_notional_pct: self.cfg.risk.max_total_notional_pct,
            max_correlated_notional_pct: self.cfg.risk.max_correlated_notional_pct,
            max_consecutive_losses: self.cfg.risk.max_consecutive_losses,
            consecutive_losses: runtime
                .consecutive_losses
                .values()
                .copied()
                .max()
                .unwrap_or(0),
            daily_pnl,
            daily_lockout: runtime.daily_locked,
            drawdown_lockout: runtime.drawdown_locked,
            cooldown_until_ms: runtime.cooldown_until_ms,
            kill_switch: runtime.emergency_stop,
            blocked_reasons: blocked,
        };

        let system = SystemStatus {
            uptime_seconds: (now - self.started_at_ms) / 1000,
            db_ok: true,
            db_path: self.cfg.db_path.to_string_lossy().to_string(),
            db_wal: self.store.wal_enabled().await,
            last_backup_ms: self
                .store
                .get_setting("last_backup_ms")
                .await
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok()),
            build_version: BUILD_VERSION.to_string(),
            git_commit: GIT_COMMIT.to_string(),
            api_errors: self.rest_client.metrics.errors(),
            rate_limit_hits: self.rest_client.metrics.rate_limits(),
            db_size_bytes: self.store.db_size_bytes().await,
            audit_events: audit,
            ws_history: vec![WsEventView {
                ts_ms: ws.last_message_ms.unwrap_or(now),
                state: ws.state.clone(),
                detail: ws.detail.clone(),
            }],
        };

        let _ = live_unrealized;
        Ok(DashboardSnapshot {
            generated_at_ms: now,
            paper_only: true,
            version: BUILD_VERSION.to_string(),
            started_at_ms: self.started_at_ms,
            running: runtime.running,
            paused: runtime.paused,
            pause_reason: runtime.pause_reason,
            emergency_stop: runtime.emergency_stop,
            emergency_reason: runtime.emergency_reason,
            ws_state: ws.state.clone(),
            ws_last_message_ms: ws.last_message_ms,
            ws_reconnects: ws.reconnects,
            feed_ok: ws.healthy(now, 15_000),
            feed_detail: ws.detail,
            clock_offset_ms: self.clock_offset_ms.load(Ordering::Relaxed),
            initial_equity: wallets.iter().map(|w| w.initial_equity).sum(),
            equity,
            cash,
            realized: wallets.iter().map(|w| w.realized).sum(),
            unrealized: live_unrealized,
            daily_pnl,
            drawdown_pct: drawdown,
            fees: wallets.iter().map(|w| w.fees).sum(),
            funding: wallets.iter().map(|w| w.funding).sum(),
            // Estimate over the most recent 40 closed trades shown below.
            estimated_slippage: recent_trades.iter().map(|t| t.slippage_est).sum(),
            exposure,
            open_positions: positions.len(),
            last_update_ms: now,
            strategies,
            scanner: scan_rows,
            positions,
            trades: recent_trades,
            risk,
            system,
            bybit,
        })
    }

    pub fn config_view(&self) -> ConfigView {
        let mut strategies = HashMap::new();
        strategies.insert(
            "trend".into(),
            serde_json::to_value(&self.cfg.strategies.trend).unwrap_or_default(),
        );
        strategies.insert(
            "mean_reversion".into(),
            serde_json::to_value(&self.cfg.strategies.mean_reversion).unwrap_or_default(),
        );
        strategies.insert(
            "breakout".into(),
            serde_json::to_value(&self.cfg.strategies.breakout).unwrap_or_default(),
        );
        ConfigView {
            risk: RiskConfigView {
                risk_per_trade_pct: self.cfg.risk.risk_per_trade_pct,
                daily_loss_limit_pct: self.cfg.risk.daily_loss_limit_pct,
                drawdown_limit_pct: self.cfg.risk.drawdown_limit_pct,
                max_open_positions: self.cfg.risk.max_open_positions,
                max_total_notional_pct: self.cfg.risk.max_total_notional_pct,
                max_correlated_notional_pct: self.cfg.risk.max_correlated_notional_pct,
                max_consecutive_losses: self.cfg.risk.max_consecutive_losses,
                max_spread_bps: self.cfg.risk.max_spread_bps,
                min_depth_usd: self.cfg.risk.min_depth_usd,
                max_data_staleness_ms: self.cfg.risk.max_data_staleness_ms,
            },
            costs: CostConfigView {
                taker_fee_bps: self.cfg.costs.taker_fee_bps,
                maker_fee_bps: self.cfg.costs.maker_fee_bps,
                slippage_bps: self.cfg.costs.slippage_bps,
                fee_source: self.cfg.costs.fee_source.clone(),
                funding_source: self.cfg.costs.funding_source.clone(),
            },
            strategies,
        }
    }

    pub async fn run_web_backtest(
        &self,
        req: BacktestRequest,
    ) -> Result<crate::model::BacktestReport> {
        let strategy: StrategyId = req.strategy.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
        let interval: Interval = req.interval.parse().map_err(|e| anyhow::anyhow!("{e}"))?;
        let limit = req.candles.unwrap_or(3000).clamp(300, 20_000);
        let candles = self
            .store
            .get_candles(&req.symbol, interval, limit as i64)
            .await?;
        if candles.len() < 200 {
            bail!(
                "only {} stored candles for {} {}; import history first",
                candles.len(),
                req.symbol,
                interval
            );
        }
        let start = candles.first().map(|c| c.open_time_ms).unwrap_or(0);
        let end = candles.last().map(|c| c.close_time_ms()).unwrap_or(0);
        let funding = self.store.get_funding(&req.symbol, start, end).await?;
        let cached = self.cache.read().await.instrument(&req.symbol).cloned();
        let instrument = match cached {
            Some(instrument) => instrument,
            None => self
                .store
                .load_instruments()
                .await?
                .into_iter()
                .find(|instrument| instrument.symbol == req.symbol)
                .context("instrument metadata not loaded yet")?,
        };
        let input = BacktestInput {
            run_id: uuid::Uuid::new_v4().to_string(),
            strategy,
            symbol: req.symbol.clone(),
            interval,
            candles,
            funding,
            instrument,
            parameters: self.cfg.backtest.clone(),
            risk: self.cfg.risk.clone(),
            costs: self.cfg.costs.clone(),
            configs: self.cfg.strategies.clone(),
            variants: Vec::<BacktestVariant>::new(),
            min_probability_samples: 30,
            min_trades_for_validation: 10,
        };
        let report = tokio::task::spawn_blocking(move || backtest::run_backtest(&input))
            .await
            .context("backtest task panicked")?;
        let json = serde_json::to_string(&report)?;
        self.store
            .save_backtest(
                &report.result.run_id,
                strategy,
                &report.result.symbol,
                &report.result.data_fingerprint,
                &json,
            )
            .await?;
        self.store
            .audit(
                "backtest.run",
                "admin",
                &format!(
                    "{} {} {} trades={} net={:.4} fingerprint={}",
                    strategy,
                    report.result.symbol,
                    report.result.interval,
                    report.result.trades.len(),
                    report.result.stats.net_pnl,
                    report.result.data_fingerprint
                ),
            )
            .await?;
        Ok(report)
    }
}

fn oos_status_for(rows: &[crate::store::BacktestRow], strategy: StrategyId) -> String {
    let Some(row) = rows.iter().find(|r| r.strategy == strategy) else {
        return "NOT YET VALIDATED: no stored out-of-sample backtest".into();
    };
    let Ok(report) = serde_json::from_str::<crate::model::BacktestReport>(&row.report_json) else {
        return "NOT YET VALIDATED: stored report is unreadable".into();
    };
    let Some(seg) = report.segments.iter().find(|s| s.name == "out_of_sample") else {
        return "NOT YET VALIDATED: no out-of-sample segment".into();
    };
    if seg.trades < 30 {
        format!("INSUFFICIENT DATA: {} out-of-sample trades", seg.trades)
    } else if seg.stats.net_pnl > 0.0 {
        format!(
            "POSITIVE OOS: {} trades, net {:.2} USDT, MaxDD {:.1}%",
            seg.trades, seg.stats.net_pnl, seg.stats.max_drawdown_pct
        )
    } else {
        format!(
            "NEGATIVE OOS: {} trades, net {:.2} USDT",
            seg.trades, seg.stats.net_pnl
        )
    }
}

pub async fn build_state(cfg: AppConfig) -> Result<AppState> {
    let cfg = Arc::new(cfg);
    let store = Store::connect(&cfg.db_path).await?;
    store.ensure_wallets(crate::model::INITIAL_EQUITY).await?;
    let master_key = load_master_key(&cfg.master_key_path)?;
    let auth = Auth::new(
        store.clone(),
        master_key,
        cfg.secure_cookies,
        Some(12 * 60 * 60 * 1000),
    );
    let cache = Arc::new(RwLock::new(MarketCache::default()));
    if let Ok(instruments) = store.load_instruments().await {
        if !instruments.is_empty() {
            cache.write().await.upsert_instruments(instruments);
        }
    }
    let ws_status = Arc::new(RwLock::new(WsStatus::default()));
    let scanner = ScannerConfig {
        max_symbols: cfg.max_symbols,
        max_spread_bps: cfg.risk.max_spread_bps,
        min_depth_usd: cfg.risk.min_depth_usd,
        min_turnover_24h: 5_000_000.0,
        depth_band_bps: 10.0,
        max_funding_pct: cfg.risk.max_funding_pct,
        max_staleness_ms: cfg.risk.max_data_staleness_ms,
    };
    let engine_cfg = EngineConfig {
        strategies: cfg.strategies.clone(),
        risk: cfg.risk.clone(),
        costs: cfg.costs.clone(),
        scanner,
        intervals: cfg.intervals.clone(),
        seed_symbols: cfg.seed_symbols.clone(),
        min_probability_samples: 30,
        cycle_secs: 5,
        equity_point_interval_ms: 60_000,
    };
    let engine = Arc::new(
        PaperEngine::new(store.clone(), cache.clone(), ws_status.clone(), engine_cfg).await?,
    );
    let rest_client = PublicClient::new(&cfg.rest_base)?;
    let selected = Arc::new(RwLock::new(cfg.seed_symbols.clone()));
    Ok(AppState {
        started_at_ms: Utc::now().timestamp_millis(),
        cfg,
        store,
        auth,
        engine,
        cache,
        ws_status,
        scan: Arc::new(RwLock::new(Vec::new())),
        bybit: Arc::new(RwLock::new(BybitStatus {
            public_rest_ok: false,
            public_ws_ok: false,
            private_connected: false,
            private_read_only: false,
            read_only_verified: false,
            uid: None,
            key_note: None,
            permissions_warning: None,
            maker_fee: None,
            taker_fee: None,
            fee_source: "assumed_conservative".into(),
            last_checked_ms: None,
            error: None,
        })),
        selected,
        public_rest_ok: Arc::new(AtomicBool::new(false)),
        clock_offset_ms: Arc::new(AtomicI64::new(0)),
        backtest_slots: Arc::new(Semaphore::new(1)),
        rest_client,
    })
}

/// Fetch historical candles + funding for a symbol and persist them.
pub async fn import_history(
    state: &AppState,
    symbol: &str,
    interval: Interval,
    days: i64,
) -> Result<usize> {
    let end = Utc::now().timestamp_millis();
    let start = end - days.clamp(1, 365) * 86_400_000;
    let candles = state
        .rest_client
        .klines_range(symbol, interval, start, end)
        .await
        .context("fetch klines")?;
    state.store.put_candles(&candles).await?;
    state.cache.write().await.upsert_candles(candles.clone());
    let funding = state
        .rest_client
        .funding_history(symbol, start, end)
        .await
        .context("fetch funding history")?;
    state.store.put_funding(&funding).await?;
    state
        .cache
        .write()
        .await
        .set_funding(symbol, funding.clone());
    state
        .store
        .audit(
            "history.import",
            "cli",
            &format!(
                "{symbol} {interval}: {} candles, {} funding events over {days} days",
                candles.len(),
                funding.len()
            ),
        )
        .await?;
    Ok(candles.len())
}

async fn refresh_instruments(state: &AppState) -> Result<()> {
    let instruments = state.rest_client.instruments().await?;
    state.store.save_instruments(&instruments).await?;
    state.cache.write().await.upsert_instruments(instruments);
    state.public_rest_ok.store(true, Ordering::Relaxed);
    Ok(())
}

async fn refresh_tickers(state: &AppState) -> Result<()> {
    let tickers = state.rest_client.tickers().await?;
    state.cache.write().await.upsert_tickers(tickers);
    state.public_rest_ok.store(true, Ordering::Relaxed);
    Ok(())
}

async fn backfill_candles(
    state: &AppState,
    symbols: &[String],
    intervals: &[Interval],
    limit: u32,
) {
    let end = Utc::now().timestamp_millis();
    for symbol in symbols {
        for interval in intervals {
            let start = end - interval.duration_ms() * limit as i64;
            match state
                .rest_client
                .klines_range(symbol, *interval, start, end)
                .await
            {
                Ok(candles) => {
                    if !candles.is_empty() {
                        let mut cache = state.cache.write().await;
                        cache.upsert_candles(candles.clone());
                        drop(cache);
                        if let Err(e) = state.store.put_candles(&candles).await {
                            warn!(symbol, ?interval, error = %e, "persist candles failed");
                        }
                    }
                }
                Err(e) => debug!(symbol, ?interval, error = %e, "backfill failed"),
            }
        }
    }
}

async fn refresh_funding(state: &AppState, symbols: &[String]) {
    let end = Utc::now().timestamp_millis();
    let start = end - 45 * 86_400_000;
    for symbol in symbols {
        match state.rest_client.funding_history(symbol, start, end).await {
            Ok(rates) => {
                if rates.is_empty() {
                    continue;
                }
                if let Err(e) = state.store.put_funding(&rates).await {
                    warn!(symbol, error = %e, "persist funding failed");
                }
                state.cache.write().await.set_funding(symbol, rates);
            }
            Err(e) => debug!(symbol, error = %e, "funding refresh failed"),
        }
    }
}

pub(crate) async fn refresh_private_status(state: &AppState) {
    let credentials: Option<ApiCredentials> = match state.auth.load_credentials().await {
        Ok(c) => c,
        Err(e) => {
            let mut status = state.bybit.write().await;
            status.private_connected = false;
            status.error = Some(e.to_string());
            return;
        }
    };
    let Some(credentials) = credentials else {
        let mut status = state.bybit.write().await;
        status.private_connected = false;
        status.private_read_only = false;
        status.read_only_verified = false;
        status.fee_source = "assumed_conservative (no read-only key configured)".into();
        return;
    };
    let fee_symbol = state
        .cfg
        .seed_symbols
        .first()
        .cloned()
        .unwrap_or_else(|| "BTCUSDT".to_string());
    match state.rest_client.verify_read_only_key(&credentials).await {
        Ok(info) => {
            let fee = state
                .rest_client
                .fee_rate(&credentials, &fee_symbol)
                .await
                .ok();
            let mut status = state.bybit.write().await;
            status.private_connected = true;
            status.private_read_only = info.read_only;
            status.read_only_verified = true;
            status.uid = info.uid;
            status.key_note = Some(info.note);
            status.last_checked_ms = Some(Utc::now().timestamp_millis());
            status.error = None;
            if let Some((taker, maker)) = fee {
                status.taker_fee = Some(taker);
                status.maker_fee = Some(maker);
                status.fee_source = "bybit_account_fee_rate (read-only key)".into();
            }
            let _ = state.store.mark_credential_verified().await;
        }
        Err(e) => {
            let mut status = state.bybit.write().await;
            status.private_connected = false;
            status.private_read_only = false;
            status.read_only_verified = false;
            status.error = Some(e.to_string());
            status.fee_source = "assumed_conservative (key verification failed)".into();
        }
    }
}

pub async fn serve(cfg: AppConfig) -> Result<()> {
    let state = Arc::new(build_state(cfg).await?);
    let shutdown = CancellationToken::new();

    // A brand-new database starts unpaused; honour an explicit auto-start=off
    // without overwriting a previously persisted emergency stop.
    if !state.cfg.auto_start && !state.engine.runtime_snapshot().paused {
        if let Err(error) = state
            .engine
            .pause("auto-start disabled by UNDERHAVEN_AUTO_START=false")
            .await
        {
            warn!(%error, "could not apply auto-start setting");
        }
    }

    // Bind the HTTP listener before the startup REST warm-up. If Bybit is slow
    // or unreachable, the dashboard and /healthz must still answer promptly and
    // show the feed as unhealthy instead of looking like a failed service.
    let server = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::web::serve(state, shutdown).await {
                error!(error = %e, "web server failed");
            }
        })
    };

    // Restore funding history before the first engine cycle.
    let bootstrap = {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = refresh_instruments(&state).await {
                warn!(error = %e, "instrument refresh failed at startup");
            }
            if let Err(e) = refresh_tickers(&state).await {
                warn!(error = %e, "ticker refresh failed at startup");
            }
            match clock_offset_ms(&state.rest_client).await {
                Ok(offset) => {
                    state.clock_offset_ms.store(offset, Ordering::Relaxed);
                    if offset.abs() > 2_000 {
                        warn!(offset, "local clock differs from Bybit by more than 2s");
                    }
                }
                Err(e) => debug!(error = %e, "clock check failed"),
            }
            let symbols = state.cfg.seed_symbols.clone();
            backfill_candles(&state, &symbols, &state.cfg.intervals, 500).await;
            refresh_funding(&state, &symbols).await;
            refresh_private_status(&state).await;
        })
    };
    // The warm-up can take a while when Bybit is unreachable because the REST
    // client retries with backoff. Stay responsive to systemd's SIGINT instead
    // of ignoring it until every retry has finished.
    tokio::select! {
        _ = bootstrap => {}
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received during startup warm-up");
            shutdown.cancel();
            return Ok(());
        }
    }

    let (symbols_tx, symbols_rx) = watch::channel(state.selected.read().await.clone());

    // Public WebSocket supervisor.
    let ws_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            ws::run(
                state.cfg.ws_url.clone(),
                state.cache.clone(),
                state.ws_status.clone(),
                state.rest_client.clone(),
                symbols_rx,
                state.cfg.intervals.clone(),
                shutdown,
            )
            .await;
        })
    };

    // Scanner: rank, persist, and drive the WS subscription set.
    let scanner_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
                state.cfg.scanner_refresh_secs,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        let now = Utc::now().timestamp_millis();
                        let rows = {
                            let cache = state.cache.read().await;
                            scan(&cache, &ScannerConfig {
                                max_symbols: state.cfg.max_symbols,
                                max_spread_bps: state.cfg.risk.max_spread_bps,
                                min_depth_usd: state.cfg.risk.min_depth_usd,
                                min_turnover_24h: 5_000_000.0,
                                depth_band_bps: 10.0,
                                max_funding_pct: state.cfg.risk.max_funding_pct,
                                max_staleness_ms: state.cfg.risk.max_data_staleness_ms,
                            }, state.cfg.intervals[0], now)
                        };
                        let selected: Vec<String> = rows.iter().filter(|r| r.selected).map(|r| r.symbol.clone()).collect();
                        let merged = {
                            let cache = state.cache.read().await;
                            merge_seed_symbols(&selected, &state.cfg.seed_symbols, &cache, state.cfg.max_symbols)
                        };
                        *state.scan.write().await = rows.clone();
                        *state.selected.write().await = merged.clone();
                        let _ = symbols_tx.send(merged.clone());
                        if let Err(e) = state.store.save_scan(&rows).await {
                            debug!(error = %e, "scan snapshot not persisted");
                        }
                        backfill_candles(&state, &merged, &state.cfg.intervals, 300).await;
                    }
                }
            }
        })
    };

    // REST fallback loop: keeps tickers fresh if WS is down and refreshes funding.
    let rest_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut funding_tick = tokio::time::interval(std::time::Duration::from_secs(1800));
            funding_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(e) = refresh_tickers(&state).await {
                            debug!(error = %e, "ticker refresh failed");
                        }
                    }
                    _ = funding_tick.tick() => {
                        let symbols = state.selected.read().await.clone();
                        refresh_funding(&state, &symbols).await;
                    }
                }
            }
        })
    };

    let instrument_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(e) = refresh_instruments(&state).await {
                            warn!(error = %e, "instrument refresh failed");
                        }
                    }
                }
            }
        })
    };

    let engine_task = {
        let state = state.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { crate::paper::run(state.engine.clone(), shutdown).await })
    };

    info!(
        bind = %state.cfg.bind,
        public_url = ?state.cfg.public_url,
        "UNDERHAVEN web server starting (paper only)"
    );

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("shutdown signal received");
        }
        _ = server => {
            warn!("web server task ended unexpectedly");
        }
    }
    shutdown.cancel();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let _ = tokio::join!(
            ws_task,
            scanner_task,
            rest_task,
            instrument_task,
            engine_task
        );
    })
    .await;
    state
        .store
        .audit("system.shutdown", "system", "graceful shutdown")
        .await?;
    Ok(())
}

/// One-shot init used by `underhaven init-admin`.
pub async fn init_admin(
    cfg: &AppConfig,
    username: &str,
    password: &str,
    force: bool,
) -> Result<()> {
    use crate::auth::{hash_password, validate_password_policy};
    validate_password_policy(password, username)?;
    let store = Store::connect(&cfg.db_path).await?;
    store.ensure_wallets(crate::model::INITIAL_EQUITY).await?;
    if let Some(user) = store.user_by_name(username).await? {
        if !force {
            bail!("admin user '{username}' already exists; use --force to reset the password");
        }
        let hash = hash_password(password)?;
        store.update_password(&user.id, &hash).await?;
        store.revoke_all_sessions(&user.id).await?;
        store
            .audit(
                "auth.admin_reset",
                "cli",
                "admin password reset; sessions revoked",
            )
            .await?;
        println!("password reset for '{username}'; all sessions revoked");
        return Ok(());
    }
    let hash = hash_password(password)?;
    store.create_user(username, &hash).await?;
    store
        .audit("auth.admin_create", "cli", "initial admin created")
        .await?;
    println!("admin '{username}' created in {}", cfg.db_path.display());
    Ok(())
}

pub async fn backup(cfg: &AppConfig, destination: &std::path::Path) -> Result<()> {
    let store = Store::connect(&cfg.db_path).await?;
    store.backup_to(destination).await?;
    store
        .set_setting("last_backup_ms", &Utc::now().timestamp_millis().to_string())
        .await?;
    store
        .audit(
            "system.backup",
            "cli",
            &format!("backup written to {}", destination.display()),
        )
        .await?;
    println!("backup written to {}", destination.display());
    Ok(())
}

/// Import stored history for a symbol, used by the CLI.
pub async fn cli_import(
    cfg: &AppConfig,
    symbol: &str,
    interval: Interval,
    days: i64,
) -> Result<usize> {
    let state = build_state(cfg.clone()).await?;
    let count = import_history(&state, symbol, interval, days).await?;
    println!("imported {count} candles for {symbol} {interval}");
    Ok(count)
}

/// CLI backtest: import if necessary, run, and save JSON/CSV next to the data dir.
pub async fn cli_backtest(
    cfg: &AppConfig,
    symbol: &str,
    strategy: StrategyId,
    interval: Interval,
    days: i64,
    output_dir: &std::path::Path,
    candles: usize,
) -> Result<std::path::PathBuf> {
    let state = build_state(cfg.clone()).await?;
    let candles = candles.clamp(300, 20_000);
    let available = state
        .store
        .get_candles(symbol, interval, candles as i64)
        .await?;
    if available.len() < 300 {
        println!("insufficient stored candles; importing {days} days...");
        import_history(&state, symbol, interval, days).await?;
    }
    let report = state
        .run_web_backtest(BacktestRequest {
            strategy: strategy.as_str().to_string(),
            symbol: symbol.to_string(),
            interval: interval.slug().to_string(),
            candles: Some(candles),
        })
        .await?;
    std::fs::create_dir_all(output_dir)?;
    let base = output_dir.join(format!(
        "{}-{}-{}-{}",
        strategy.as_str(),
        symbol,
        interval.slug(),
        report.result.run_id
    ));
    std::fs::write(base.with_extension("json"), backtest::report_json(&report)?)?;
    std::fs::write(
        base.with_extension("trades.csv"),
        backtest::trades_csv(&report.result),
    )?;
    println!(
        "{} {} {}: trades={} net={:.4} maxDD={:.2}% => {}.json",
        strategy,
        symbol,
        interval,
        report.result.trades.len(),
        report.result.stats.net_pnl,
        report.result.stats.max_drawdown_pct,
        base.display()
    );
    if report.result.trades.len() < 30 {
        println!("status: INSUFFICIENT DATA (fewer than 30 trades)");
    }
    if report.walk_forward.is_empty() {
        println!("walk-forward: NOT RUN (see warnings)");
    }
    Ok(base.with_extension("json"))
}

/// Doctor: verifies configuration, database and public connectivity.
pub async fn doctor(cfg: &AppConfig) -> Result<()> {
    println!("UNDERHAVEN BYBIT TRADING LAB {}", BUILD_VERSION);
    println!("paper-only: enforced at compile time and by module boundaries");
    println!("bind: {}", cfg.bind);
    println!("database: {}", cfg.db_path.display());
    println!("rest: {}  ws: {}", cfg.rest_base, cfg.ws_url);
    let store = Store::connect(&cfg.db_path).await?;
    println!(
        "database ok: wallets={} size={} bytes wal={}",
        store.wallets().await?.len(),
        store.db_size_bytes().await,
        store.wal_enabled().await
    );
    let admin = store.user_count().await?;
    println!("admin users: {admin}");
    if admin == 0 {
        println!("WARNING: no admin user; run `underhaven init-admin`");
    }
    for strategy in StrategyId::ALL {
        let (delta, detail) = store.reconcile(strategy).await?;
        println!(
            "ledger {strategy}: delta={delta:.8} {}",
            if delta > 1e-6 { "MISMATCH" } else { "ok" }
        );
        if delta > 1e-6 {
            println!("  {detail}");
        }
    }
    let client = PublicClient::new(&cfg.rest_base)?;
    match client.server_time_ms().await {
        Ok(ts) => println!("public REST ok: Bybit server time {ts}"),
        Err(e) => println!("public REST FAILED: {e}"),
    }
    match client.tickers().await {
        Ok(tickers) => println!("tickers ok: {} linear symbols", tickers.len()),
        Err(e) => println!("tickers FAILED: {e}"),
    }
    Ok(())
}

pub fn parse_credentials(key: &str, secret: &str) -> Result<ApiCredentials> {
    if key.trim().is_empty() || secret.trim().is_empty() {
        bail!("both API key and secret are required");
    }
    Ok(ApiCredentials {
        api_key: key.trim().to_string(),
        api_secret: secret.trim().to_string(),
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub paper_only: bool,
}

pub fn health() -> Health {
    Health {
        status: "ok",
        version: BUILD_VERSION,
        paper_only: true,
    }
}

// Keep unused import warnings honest for types used only in feature-complete
// dashboard payloads.
#[allow(dead_code)]
fn _type_anchors(
    _: Instrument,
    _: PaperPosition,
    _: EquityPoint,
    _: AuditEvent,
    _: RiskConfig,
    _: CostConfig,
    _: StrategyConfigs,
) {
}
