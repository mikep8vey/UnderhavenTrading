use crate::config::{Config, ExecCfg};
use crate::dashboard;
use crate::data::cli_report::CliClient;
use crate::data::ensemble::EnsembleClient;
use crate::data::metar::{swing_c, MetarClient};
use crate::error::{BotError, Result};
use crate::execution::live_broker::LiveBroker;
use crate::execution::maker_layer::{ExecOutcome, MakerLayer};
use crate::execution::paper_broker::PaperBroker;
use crate::execution::Broker;
use crate::polymarket::auth::Wallet;
use crate::polymarket::clob::ClobClient;
use crate::polymarket::gamma::{GammaClient, WeatherEvent};
use crate::risk::exposure::{Exposure, PositionRec};
use crate::risk::kill_switch::{crypto_range_vol, drawdown_tripped, kill_file, Halt};
use crate::storage::sqlite::Storage;
use crate::strategies::calibration::{score, Calibrator, Scores};
use crate::strategies::{forecast_edge, no_harvester};
use crate::util::lock;
use crate::util::time::climate_window_utc;
use chrono::{NaiveTime, Utc};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct LiveCreds {
    pub wallet: Wallet,
    pub funder: [u8; 20],
    pub signature_type: u8,
}

/// One trading account. "paper" (simulated fills on real books) always exists; "live" exists only when started with
/// `weather-bot live --enable`. Each account has its own tables, positions, exposure book, halt state and maker layer,
/// but both act on the same signals, edge and probabilities.
pub struct Account {
    pub name: &'static str,
    pub live: bool,
    pub bankroll: Decimal,
    pub storage: Arc<Storage>,
    pub exposure: Arc<Mutex<Exposure>>,
    pub halt: Arc<Halt>,
    pub maker: Arc<MakerLayer>,
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
    day_start: Mutex<(chrono::NaiveDate, Decimal)>,
}

impl Account {
    /// Retry limit: at most `max` unfilled attempts per market per UTC day, `cooldown_secs` apart.
    pub fn can_attempt(&self, token: &str, max: u32, cooldown_secs: u64) -> bool {
        let key = format!("{token}|{}", Utc::now().date_naive());
        let g = lock(&self.attempts);
        match g.get(&key) {
            None => true,
            Some((n, last)) => *n < max && last.elapsed() >= Duration::from_secs(cooldown_secs),
        }
    }

    pub fn note_result(&self, token: &str, res: &ExecOutcome) {
        if matches!(res, ExecOutcome::Unfilled(_) | ExecOutcome::Failed(_)) {
            let key = format!("{token}|{}", Utc::now().date_naive());
            let mut g = lock(&self.attempts);
            let e = g.entry(key).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn make_account(
    name: &'static str,
    live: bool,
    bankroll: Decimal,
    storage: Arc<Storage>,
    broker: Arc<dyn Broker>,
    clob: &Arc<ClobClient>,
    allow_taker: bool,
    exec: ExecCfg,
    kill_path: PathBuf,
) -> Result<Arc<Account>> {
    let exposure = Arc::new(Mutex::new(Exposure::restore(
        bankroll,
        storage.open_positions()?,
        storage.realized_pnl()?,
        storage.peak_equity()?,
    )));
    let halt = Arc::new(Halt::new());
    if kill_path.exists() {
        halt.trip_permanent(&format!("kill switch file {} is present", kill_path.display()));
    }
    let maker = Arc::new(MakerLayer::new(
        name,
        broker,
        clob.clone(),
        storage.clone(),
        exposure.clone(),
        halt.clone(),
        allow_taker,
        exec,
    ));
    let today = Utc::now().date_naive();
    let equity = lock(&exposure).equity();
    let start = storage.first_equity_since(&today.to_string())?.unwrap_or(equity);
    Ok(Arc::new(Account {
        name,
        live,
        bankroll,
        storage,
        exposure,
        halt,
        maker,
        attempts: Mutex::new(HashMap::new()),
        day_start: Mutex::new((today, start)),
    }))
}

pub struct App {
    pub cfg: Config,
    pub http: reqwest::Client,
    /// Shared handle: signals, events, runs, forecast_log. Per-account tables live in `accounts[i].storage`.
    pub storage: Arc<Storage>,
    pub clob: Arc<ClobClient>,
    pub gamma: GammaClient,
    pub metar: MetarClient,
    pub cli: CliClient,
    pub ensemble: EnsembleClient,
    pub accounts: Vec<Arc<Account>>,
    pub calibrator: Mutex<Calibrator>,
    pub scores: Mutex<Scores>,
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

        let mut accounts = Vec::new();
        accounts.push(make_account(
            "paper",
            false,
            cfg.risk.bankroll_usdc,
            Arc::new(storage.account("paper")),
            Arc::new(PaperBroker::new(clob.clone())),
            &clob,
            allow_taker,
            cfg.risk.execution.clone(),
            kill_file(&cfg.db_path, "paper"),
        )?);
        if let Some(c) = creds {
            if cfg.risk.live_bankroll_usdc <= Decimal::ZERO {
                return Err(BotError::Config(
                    "live mode needs live_bankroll_usdc > 0 in risk.toml (the USDC you actually fund)".into(),
                ));
            }
            let lb = LiveBroker::connect(http.clone(), &ep, c.wallet, c.funder, c.signature_type, clob.clone()).await?;
            accounts.push(make_account(
                "live",
                true,
                cfg.risk.live_bankroll_usdc,
                Arc::new(storage.account("live")),
                Arc::new(lb),
                &clob,
                allow_taker,
                cfg.risk.execution.clone(),
                kill_file(&cfg.db_path, "live"),
            )?);
        }

        let f = &cfg.risk.forecast;
        let rows = storage.scoring_rows()?;
        let pairs: Vec<(f64, bool)> = rows.iter().map(|(p, _, o)| (*p, *o)).collect();
        let calibrator = Calibrator::fit(&pairs, f.calibration_min_samples, f.calibration_prior);
        let w = f.shrink_weight.to_f64().unwrap_or(0.3);
        let scores = score(&rows, &calibrator, w);
        let metar = MetarClient::new(http.clone(), &ep.metar_url);
        let cli = CliClient::new(http.clone(), &ep.cli_url, cfg.risk.harvester.cli_ttl_secs);
        let ensemble = EnsembleClient::new(http.clone(), &ep.ensemble_url, cfg.risk.ensemble.models.clone());
        let gamma = GammaClient::new(http.clone(), &ep.gamma_url);
        Ok(Arc::new(App {
            cfg,
            http,
            storage,
            clob,
            gamma,
            metar,
            cli,
            ensemble,
            accounts,
            calibrator: Mutex::new(calibrator),
            scores: Mutex::new(scores),
            events_cache: Mutex::new(None),
            ens_cache: Mutex::new(HashMap::new()),
            log_gate: Mutex::new(HashMap::new()),
        }))
    }

    pub fn account(&self, name: &str) -> Option<&Arc<Account>> {
        self.accounts.iter().find(|a| a.name == name)
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

    /// Calibrated probability (identity until enough forecasts have settled).
    pub fn calibrate(&self, p: Decimal) -> Decimal {
        lock(&self.calibrator).apply_dec(p)
    }

    fn refresh_calibration(&self) {
        let f = &self.cfg.risk.forecast;
        if let Ok(rows) = self.storage.scoring_rows() {
            let pairs: Vec<(f64, bool)> = rows.iter().map(|(p, _, o)| (*p, *o)).collect();
            let cal = Calibrator::fit(&pairs, f.calibration_min_samples, f.calibration_prior);
            let w = f.shrink_weight.to_f64().unwrap_or(0.3);
            *lock(&self.scores) = score(&rows, &cal, w);
            *lock(&self.calibrator) = cal;
        }
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

    /// Pause BOTH accounts (volatility guards). Never shortens an existing pause.
    async fn pause_all(&self, reason: &str) {
        let already = self.accounts.iter().all(|a| a.halt.is_halted());
        if already {
            return;
        }
        self.event("WARN", "GUARD_TRIPPED", &format!("{reason}: pausing all accounts and cancelling open orders"));
        for a in &self.accounts {
            a.halt.pause(self.cfg.risk.guards.pause_secs, reason);
            a.maker.cancel_all().await;
        }
    }

    /// Permanent stop for one account (drawdown limit).
    async fn kill(&self, acct: &Account, reason: &str) {
        acct.halt.trip_permanent(reason);
        self.event("WARN", "KILL_SWITCH", &format!("[{}] {reason}", acct.name));
        let _ = std::fs::write(kill_file(&self.cfg.db_path, acct.name), format!("{reason}\n"));
        acct.maker.cancel_all().await;
    }

    /// Daily loss cap: no new trades for one account until the next UTC midnight.
    async fn daily_stop(&self, acct: &Account, reason: &str) {
        let now = Utc::now();
        let next = now
            .date_naive()
            .succ_opt()
            .unwrap_or_else(|| now.date_naive())
            .and_time(NaiveTime::MIN)
            .and_utc();
        let secs = (next - now).num_seconds().max(60) as u64;
        acct.halt.pause(secs, reason);
        self.event("WARN", "DAILY_LOSS_CAP", &format!("[{}] {reason}: no new trades until 00:00 UTC", acct.name));
        acct.maker.cancel_all().await;
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

/// Mark every open position of every account to the current mid so equity/drawdown readings are not stale.
async fn mark_open_positions(app: &Arc<App>) {
    for acct in app.accounts.iter() {
        let positions: Vec<PositionRec> = lock(&acct.exposure).positions().to_vec();
        for p in &positions {
            if let Ok(b) = app.clob.book(&p.token_id).await {
                if let Some(mid) = b.mid() {
                    lock(&acct.exposure).mark(&p.token_id, mid);
                }
            }
        }
    }
}

pub async fn run_bot(app: Arc<App>, mode: &str) -> Result<()> {
    mark_open_positions(&app).await;
    let mut runs = Vec::new();
    for acct in app.accounts.iter() {
        let eq = lock(&acct.exposure).equity();
        let id = app.storage.start_run(&format!("{mode}:{}", acct.name), eq)?;
        runs.push((id, eq, acct.clone()));
        app.event("INFO", "START", &format!("[{}] mode={mode} equity={eq} halted={}", acct.name, acct.halt.is_halted()));
        if acct.halt.is_permanent() {
            app.event(
                "WARN",
                "KILL_SWITCH",
                &format!("[{}] {} - delete the KILL_SWITCH_{} file after review to re-enable", acct.name, acct.halt.reason(), acct.name),
            );
        }
    }

    let mut handles = vec![
        tokio::spawn(forecast_loop(app.clone())),
        tokio::spawn(harvester_loop(app.clone())),
        tokio::spawn(guard_loop(app.clone())),
        tokio::spawn(housekeeping_loop(app.clone())),
    ];
    if app.cfg.risk.dashboard.enabled {
        match dashboard::load_admin(&app.cfg.admin_file) {
            Ok(Some(adm)) => {
                handles.push(tokio::spawn(dashboard::serve(app.clone(), adm, app.cfg.risk.dashboard.bind.clone())));
            }
            Ok(None) => tracing::info!(
                "dashboard disabled: no admin login at {} (run: weather-bot set-admin)",
                app.cfg.admin_file.display()
            ),
            Err(e) => tracing::warn!("dashboard disabled: {e}"),
        }
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = wait_term() => {},
    }
    for h in &handles {
        h.abort();
    }
    for (id, start, acct) in &runs {
        acct.maker.cancel_all().await;
        let end = lock(&acct.exposure).equity();
        let _ = app.storage.end_run(*id, end, end - *start);
        app.event("INFO", "STOP", &format!("[{}] equity={end}", acct.name));
    }
    Ok(())
}

fn any_active(app: &App) -> bool {
    app.accounts.iter().any(|a| !a.halt.is_halted())
}

async fn forecast_loop(app: Arc<App>) {
    let every = Duration::from_secs(app.cfg.risk.forecast.scan_interval_secs.max(30));
    loop {
        if any_active(&app) {
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
        if any_active(&app) {
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

        // Per-account drawdown kill switch and daily loss cap.
        for acct in app.accounts.iter() {
            let (equity, dd) = {
                let mut e = lock(&acct.exposure);
                let dd = e.drawdown();
                (e.equity(), dd)
            };
            if !acct.halt.is_permanent() && drawdown_tripped(dd, app.cfg.risk.drawdown_kill_pct) {
                app.kill(acct, &format!("drawdown {dd:.3} reached limit {}", app.cfg.risk.drawdown_kill_pct)).await;
                continue;
            }
            let today = Utc::now().date_naive();
            let start = {
                let mut d = lock(&acct.day_start);
                if d.0 != today {
                    *d = (today, equity);
                }
                d.1
            };
            if start > Decimal::ZERO && !acct.halt.is_halted() {
                let loss = (start - equity) / start;
                if loss >= app.cfg.risk.daily_loss_cap_pct {
                    app.daily_stop(acct, &format!("down {loss:.3} since the start of the UTC day")).await;
                }
            }
        }

        // Crypto volatility guard.
        for sym in &g.crypto_symbols {
            match crypto_range_vol(&app.http, &g.crypto_url, sym).await {
                Ok(v) if v > g.crypto_vol_threshold => {
                    app.pause_all(&format!("{sym} 1m range {v:.4} > {}", g.crypto_vol_threshold)).await;
                }
                Ok(_) => {}
                Err(e) => {
                    if app.should_log(&format!("cryptoerr|{sym}"), 900) {
                        tracing::warn!("crypto vol check {sym} failed: {e}");
                    }
                }
            }
        }

        // METAR swing guard.
        let now = Utc::now();
        for st in app.cfg.registry.stations() {
            if let Ok(obs) = app.metar.observations(&st.icao).await {
                let s = swing_c(&obs, now, g.metar_swing_window_mins);
                if s > g.metar_swing_c {
                    app.pause_all(&format!("{} METAR swing {s:.1}C in {}m", st.icao, g.metar_swing_window_mins)).await;
                }
            }
        }
    }
}

async fn housekeeping_loop(app: Arc<App>) {
    let mut tick: u64 = 0;
    let mut last_export: Option<chrono::NaiveDate> = None;
    loop {
        mark_open_positions(&app).await;

        // Settle resolved markets every ~5 minutes (per account).
        if tick % 5 == 0 {
            for acct in app.accounts.iter() {
                let positions: Vec<PositionRec> = lock(&acct.exposure).positions().to_vec();
                for p in &positions {
                    if let Ok(Some(payout)) = app.gamma.resolution(&p.market_id, p.outcome_idx).await {
                        let closed = lock(&acct.exposure).resolve(&p.token_id, payout);
                        if let Some((pos, pnl)) = closed {
                            let _ = acct.storage.close_position(&pos.token_id, pnl);
                            app.event(
                                "INFO",
                                "SETTLED",
                                &format!("[{}] {} {} {} payout={payout} pnl={pnl}", acct.name, pos.city, pos.bracket, pos.outcome),
                            );
                        }
                    }
                }
            }
        }

        // Fill in outcomes of logged forecasts whose market date has passed, then re-learn the calibration (every ~15 min).
        if tick % 15 == 0 {
            let today = Utc::now().date_naive().to_string();
            if let Ok(pending) = app.storage.unresolved_forecasts(&today, 200) {
                for (id, market_id, yes_idx) in pending {
                    if let Ok(Some(payout)) = app.gamma.resolution(&market_id, yes_idx).await {
                        let _ = app.storage.set_forecast_outcome(id, payout > Decimal::ZERO);
                    }
                }
            }
            app.refresh_calibration();
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

        // Dashboard line + equity snapshots, per account.
        for acct in app.accounts.iter() {
            let (equity, dd, open) = {
                let mut e = lock(&acct.exposure);
                (e.equity(), e.drawdown(), e.positions().len())
            };
            let (vol, accrued) = acct.storage.rebate_totals().unwrap_or((Decimal::ZERO, Decimal::ZERO));
            tracing::info!(
                "dashboard[{}]: equity={equity:.2} drawdown={dd:.4} open_positions={open} maker_volume_usdc={vol:.2} accrued_rebate_usdc={accrued:.4} halted={}",
                acct.name,
                acct.halt.is_halted()
            );
            if tick % 5 == 0 {
                let _ = acct.storage.insert_equity(equity, dd);
            }
        }
        tick += 1;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
