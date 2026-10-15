use crate::config::Config;
use crate::data::cli_report::CliClient;
use crate::data::ensemble::EnsembleClient;
use crate::data::metar::{swing_c, MetarClient};
use crate::error::{BotError, Result};
use crate::execution::live_broker::LiveBroker;
use crate::execution::maker_layer::MakerLayer;
use crate::execution::paper_broker::PaperBroker;
use crate::execution::Broker;
use crate::polymarket::auth::Wallet;
use crate::polymarket::clob::ClobClient;
use crate::polymarket::gamma::{GammaClient, WeatherEvent};
use crate::risk::exposure::{Exposure, PositionRec};
use crate::risk::kill_switch::{crypto_range_vol, drawdown_tripped, kill_file, Halt};
use crate::storage::sqlite::Storage;
use crate::strategies::{forecast_edge, no_harvester};
use crate::util::lock;
use crate::util::time::climate_window_utc;
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct LiveCreds {
    pub wallet: Wallet,
    pub funder: [u8; 20],
    pub signature_type: u8,
}

pub struct App {
    pub cfg: Config,
    pub live: bool,
    pub http: reqwest::Client,
    pub storage: Arc<Storage>,
    pub clob: Arc<ClobClient>,
    pub gamma: GammaClient,
    pub metar: MetarClient,
    pub cli: CliClient,
    pub ensemble: EnsembleClient,
    pub exposure: Arc<Mutex<Exposure>>,
    pub halt: Arc<Halt>,
    pub maker: Arc<MakerLayer>,
    events_cache: Mutex<Option<(Instant, Arc<Vec<WeatherEvent>>)>>,
    ens_cache: Mutex<HashMap<String, (Instant, Arc<Vec<f64>>)>>,
    log_gate: Mutex<HashMap<String, Instant>>,
}

impl App {
    pub async fn new(cfg: Config, creds: Option<LiveCreds>, allow_taker: bool) -> Result<Arc<App>> {
        let http = reqwest::Client::builder()
            .user_agent("weather-bot/0.1")
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| BotError::Config(format!("http client: {e}")))?;
        let ep = cfg.risk.endpoints.clone();
        let clob = Arc::new(ClobClient::new(http.clone(), &ep.clob_url));
        let storage = Arc::new(Storage::open(&cfg.db_path)?);
        let live = creds.is_some();
        let broker: Arc<dyn Broker> = match creds {
            Some(c) => Arc::new(
                LiveBroker::connect(http.clone(), &ep, c.wallet, c.funder, c.signature_type, clob.clone()).await?,
            ),
            None => Arc::new(PaperBroker::new(clob.clone())),
        };
        let exposure = Arc::new(Mutex::new(Exposure::restore(
            cfg.risk.bankroll_usdc,
            storage.open_positions()?,
            storage.realized_pnl()?,
        )));
        let halt = Arc::new(Halt::new());
        let maker = Arc::new(MakerLayer::new(
            broker,
            clob.clone(),
            storage.clone(),
            exposure.clone(),
            halt.clone(),
            allow_taker,
            cfg.risk.execution.clone(),
        ));
        let metar = MetarClient::new(http.clone(), &ep.metar_url);
        let cli = CliClient::new(http.clone(), &ep.cli_url, cfg.risk.harvester.cli_ttl_secs);
        let ensemble = EnsembleClient::new(http.clone(), &ep.ensemble_url, cfg.risk.ensemble.models.clone());
        let gamma = GammaClient::new(http.clone(), &ep.gamma_url);
        Ok(Arc::new(App {
            cfg,
            live,
            http,
            storage,
            clob,
            gamma,
            metar,
            cli,
            ensemble,
            exposure,
            halt,
            maker,
            events_cache: Mutex::new(None),
            ens_cache: Mutex::new(HashMap::new()),
            log_gate: Mutex::new(HashMap::new()),
        }))
    }

    pub fn event(&self, level: &str, kind: &str, msg: &str) {
        if level == "WARN" {
            tracing::warn!("{kind}: {msg}");
        } else {
            tracing::info!("{kind}: {msg}");
        }
        let _ = self.storage.log_event(level, kind, msg);
    }

    /// True at most once per `secs` for a key (de-duplicates repeating warnings).
    pub fn should_log(&self, key: &str, secs: u64) -> bool {
        let mut g = lock(&self.log_gate);
        if let Some(t) = g.get(key) {
            if t.elapsed() < Duration::from_secs(secs) {
                return false;
            }
        }
        g.insert(key.to_string(), Instant::now());
        true
    }

    /// Weather events from Gamma, cached for 60s.
    pub async fn events(&self) -> Result<Arc<Vec<WeatherEvent>>> {
        {
            let g = lock(&self.events_cache);
            if let Some((t, v)) = g.as_ref() {
                if t.elapsed() < Duration::from_secs(60) {
                    return Ok(v.clone());
                }
            }
        }
        let today = Utc::now().date_naive();
        let v = Arc::new(self.gamma.weather_events(&self.cfg.registry, today).await?);
        *lock(&self.events_cache) = Some((Instant::now(), v.clone()));
        Ok(v)
    }

    /// Per-member daily extremes for an event, cached (ensemble_ttl_secs).
    pub async fn members(&self, ev: &WeatherEvent) -> Result<Arc<Vec<f64>>> {
        let key = ev.key();
        let ttl = Duration::from_secs(self.cfg.risk.forecast.ensemble_ttl_secs);
        {
            let g = lock(&self.ens_cache);
            if let Some((t, v)) = g.get(&key) {
                if t.elapsed() < ttl {
                    return Ok(v.clone());
                }
            }
        }
        let city = self
            .cfg
            .registry
            .city_cfg(&ev.city)
            .ok_or_else(|| BotError::Config(format!("city {} missing from cities.toml", ev.city)))?;
        let st = self
            .cfg
            .registry
            .station(&ev.city)
            .ok_or_else(|| BotError::Config(format!("city {} missing from stations.toml", ev.city)))?;
        let window = climate_window_utc(ev.date, city.tz, city.day_basis);
        let m = Arc::new(self.ensemble.member_extremes(st, city.unit, window, ev.kind).await?);
        lock(&self.ens_cache).insert(key, (Instant::now(), m.clone()));
        Ok(m)
    }

    /// Latest cached members regardless of age (used by maker-layer fair-value refresh).
    pub fn cached_members(&self, key: &str) -> Option<Arc<Vec<f64>>> {
        lock(&self.ens_cache).get(key).map(|(_, v)| v.clone())
    }

    async fn pause(&self, reason: &str) {
        if self.halt.is_halted() {
            return;
        }
        self.halt.pause(self.cfg.risk.guards.pause_secs, reason);
        self.event("WARN", "GUARD_TRIPPED", &format!("{reason}: cancelling all open orders"));
        self.maker.cancel_all().await;
    }

    async fn kill(&self, reason: &str) {
        self.halt.trip_permanent(reason);
        self.event("WARN", "KILL_SWITCH", reason);
        let _ = std::fs::write(kill_file(&self.cfg.db_path), format!("{reason}\n"));
        self.maker.cancel_all().await;
    }
}

async fn wait_term() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

pub async fn run_bot(app: Arc<App>, mode: &str) -> Result<()> {
    let kf = kill_file(&app.cfg.db_path);
    if kf.exists() {
        return Err(BotError::Config(format!(
            "kill switch file {} exists (drawdown limit was hit). Review, then delete it to restart.",
            kf.display()
        )));
    }
    let start_equity = lock(&app.exposure).equity();
    let run_id = app.storage.start_run(mode, start_equity)?;
    app.event("INFO", "START", &format!("mode={mode} equity={start_equity}"));

    let handles = vec![
        tokio::spawn(forecast_loop(app.clone())),
        tokio::spawn(harvester_loop(app.clone())),
        tokio::spawn(guard_loop(app.clone())),
        tokio::spawn(housekeeping_loop(app.clone())),
    ];
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = wait_term() => {},
    }
    for h in &handles {
        h.abort();
    }
    app.maker.cancel_all().await;
    let end_equity = lock(&app.exposure).equity();
    let _ = app.storage.end_run(run_id, end_equity, end_equity - start_equity);
    app.event("INFO", "STOP", &format!("equity={end_equity}"));
    Ok(())
}

async fn forecast_loop(app: Arc<App>) {
    let every = Duration::from_secs(app.cfg.risk.forecast.scan_interval_secs.max(30));
    loop {
        if !app.halt.is_halted() {
            match forecast_edge::scan(&app, None, true).await {
                Ok(s) => tracing::info!("forecast scan: {} signals", s.len()),
                Err(e) => tracing::warn!("forecast scan failed: {e}"),
            }
        }
        tokio::time::sleep(every).await;
    }
}

async fn harvester_loop(app: Arc<App>) {
    let every = Duration::from_secs(app.cfg.risk.harvester.poll_secs.max(5));
    loop {
        if !app.halt.is_halted() {
            if let Err(e) = no_harvester::scan(&app, None, true).await {
                tracing::warn!("harvester scan failed: {e}");
            }
        }
        tokio::time::sleep(every).await;
    }
}

async fn guard_loop(app: Arc<App>) {
    let g = app.cfg.risk.guards.clone();
    loop {
        tokio::time::sleep(Duration::from_secs(g.check_secs.max(5))).await;

        // Drawdown kill switch.
        let dd = lock(&app.exposure).drawdown();
        if !app.halt.is_permanent() && drawdown_tripped(dd, app.cfg.risk.drawdown_kill_pct) {
            app.kill(&format!("drawdown {dd:.3} reached limit {}", app.cfg.risk.drawdown_kill_pct)).await;
            continue;
        }

        // Crypto volatility guard.
        for sym in &g.crypto_symbols {
            match crypto_range_vol(&app.http, &g.crypto_url, sym).await {
                Ok(v) if v > g.crypto_vol_threshold => {
                    app.pause(&format!("{sym} 1m range {v:.4} > {}", g.crypto_vol_threshold)).await;
                }
                Ok(_) => {}
                Err(e) => {
                    if app.should_log(&format!("cryptoerr|{sym}"), 900) {
                        tracing::warn!("crypto vol check {sym} failed: {e}");
                    }
                }
            }
        }

        // METAR swing guard (> swing_c within the window at any station).
        let now = Utc::now();
        for st in app.cfg.registry.stations() {
            if let Ok(obs) = app.metar.observations(&st.icao).await {
                let s = swing_c(&obs, now, g.metar_swing_window_mins);
                if s > g.metar_swing_c {
                    app.pause(&format!("{} METAR swing {s:.1}C in {}m", st.icao, g.metar_swing_window_mins)).await;
                }
            }
        }
    }
}

async fn housekeeping_loop(app: Arc<App>) {
    let mut tick: u64 = 0;
    let mut last_export: Option<NaiveDate> = None;
    loop {
        // Mark open positions to the current mid.
        let positions: Vec<PositionRec> = lock(&app.exposure).positions().to_vec();
        for p in &positions {
            if let Ok(b) = app.clob.book(&p.token_id).await {
                if let Some(mid) = b.mid() {
                    lock(&app.exposure).mark(&p.token_id, mid);
                }
            }
        }
        // Settle resolved markets every ~5 minutes.
        if tick % 5 == 0 {
            for p in &positions {
                if let Ok(Some(payout)) = app.gamma.resolution(&p.market_id, p.outcome_idx).await {
                    let closed = lock(&app.exposure).resolve(&p.token_id, payout);
                    if let Some((pos, pnl)) = closed {
                        let _ = app.storage.close_position(&pos.token_id, pnl);
                        app.event("INFO", "SETTLED", &format!("{} {} {} payout={payout} pnl={pnl}", pos.city, pos.bracket, pos.outcome));
                    }
                }
            }
        }
        // Daily CSV export (UTC date).
        let today = Utc::now().date_naive();
        if last_export != Some(today) {
            match app.storage.export_csv(&app.cfg.export_dir) {
                Ok(f) => {
                    tracing::info!("exported {} csv files to {}", f.len(), app.cfg.export_dir.display());
                    last_export = Some(today);
                }
                Err(e) => tracing::warn!("csv export failed: {e}"),
            }
        }
        // Dashboard.
        let (equity, dd, open) = {
            let mut e = lock(&app.exposure);
            (e.equity(), e.drawdown(), e.positions().len())
        };
        let (vol, accrued) = app.storage.rebate_totals().unwrap_or((Decimal::ZERO, Decimal::ZERO));
        tracing::info!(
            "dashboard: equity={equity:.2} drawdown={dd:.4} open_positions={open} maker_volume_usdc={vol:.2} accrued_rebate_usdc={accrued:.4} halted={}",
            app.halt.is_halted()
        );
        tick += 1;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
