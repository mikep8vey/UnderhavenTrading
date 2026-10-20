//! Built-in web dashboard: login page, session cookies, JSON API and the embedded single-page UI.
//! It is read-only (it cannot place orders or change settings) and binds to loopback by default;
//! put nginx (HTTPS) in front of it - scripts/install.sh does that for you.
use crate::app::{Account, App};
use crate::error::{BotError, Result};
use crate::util::lock;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rand::Rng;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INDEX: &str = include_str!("index.html");
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);
const CSP: &str = "default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";

#[derive(Clone, Serialize, Deserialize)]
pub struct Admin {
    pub user: String,
    pub hash: String,
}

pub fn hash_password(pw: &str) -> Result<String> {
    let mut raw = [0u8; 16];
    rand::thread_rng().fill(&mut raw);
    let salt = SaltString::encode_b64(&raw).map_err(|e| BotError::Config(format!("salt: {e}")))?;
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| BotError::Config(format!("password hashing failed: {e}")))
}

pub fn verify_pw(hash: &str, pw: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(p) => Argon2::default().verify_password(pw.as_bytes(), &p).is_ok(),
        Err(_) => false,
    }
}

/// Store the admin login (Argon2 hash, never the password) with 0600 permissions.
pub fn write_admin(path: &Path, user: &str, pw: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let adm = Admin { user: user.to_string(), hash: hash_password(pw)? };
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path)?;
    f.write_all(serde_json::to_string_pretty(&adm)?.as_bytes())?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub fn load_admin(path: &Path) -> Result<Option<Admin>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| BotError::Config(format!("cannot read {}: {e}", path.display())))?;
    let adm: Admin = serde_json::from_str(&text)
        .map_err(|e| BotError::Config(format!("invalid {}: {e}", path.display())))?;
    Ok(Some(adm))
}

struct Dash {
    app: Arc<App>,
    admin: Admin,
    sessions: Mutex<HashMap<String, Instant>>,
    fails: Mutex<(u32, Instant)>,
    started: Instant,
}

#[derive(Deserialize)]
struct LoginReq {
    user: String,
    password: String,
}

pub async fn serve(app: Arc<App>, admin: Admin, bind: String) {
    let st = Arc::new(Dash {
        app,
        admin,
        sessions: Mutex::new(HashMap::new()),
        fails: Mutex::new((0, Instant::now())),
        started: Instant::now(),
    });
    let router = Router::new()
        .route("/", get(index))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/summary", get(summary))
        .with_state(st);
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("dashboard: cannot bind {bind}: {e}");
            return;
        }
    };
    tracing::info!("dashboard listening on http://{bind} (served over HTTPS by Caddy)");
    if let Err(e) = axum::serve(listener, router).await {
        tracing::warn!("dashboard stopped: {e}");
    }
}

fn new_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill(&mut b);
    hex::encode(b)
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let c = headers.get(header::COOKIE)?.to_str().ok()?;
    c.split(';').filter_map(|p| p.trim().strip_prefix("wb_session=")).next().map(|s| s.to_string())
}

fn authed(st: &Dash, headers: &HeaderMap) -> bool {
    let Some(tok) = cookie_token(headers) else { return false };
    let mut s = lock(&st.sessions);
    let valid = s.get(&tok).map_or(false, |exp| *exp > Instant::now());
    if valid {
        s.insert(tok, Instant::now() + SESSION_TTL);
    } else {
        s.remove(&tok);
    }
    valid
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_SECURITY_POLICY, CSP),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Html(INDEX),
    )
}

async fn login(State(st): State<Arc<Dash>>, Json(req): Json<LoginReq>) -> Response {
    let locked = {
        let f = lock(&st.fails);
        f.0 >= 5 && f.1.elapsed() < Duration::from_secs(60)
    };
    if locked {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "Too many attempts. Wait a minute."}))).into_response();
    }
    let hash = st.admin.hash.clone();
    let pw = req.password.clone();
    let pw_ok = tokio::task::spawn_blocking(move || verify_pw(&hash, &pw)).await.unwrap_or(false);
    if !(pw_ok && req.user == st.admin.user) {
        {
            let mut f = lock(&st.fails);
            if f.1.elapsed() > Duration::from_secs(60) {
                f.0 = 0;
            }
            f.0 += 1;
            f.1 = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "Wrong username or password."}))).into_response();
    }
    {
        let mut f = lock(&st.fails);
        f.0 = 0;
    }
    let tok = new_token();
    lock(&st.sessions).insert(tok.clone(), Instant::now() + SESSION_TTL);
    let cookie = format!("wb_session={tok}; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=43200");
    ([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response()
}

async fn logout(State(st): State<Arc<Dash>>, headers: HeaderMap) -> Response {
    if let Some(tok) = cookie_token(&headers) {
        lock(&st.sessions).remove(&tok);
    }
    let cookie = "wb_session=; HttpOnly; Secure; SameSite=Strict; Path=/; Max-Age=0".to_string();
    ([(header::SET_COOKIE, cookie)], Json(json!({"ok": true}))).into_response()
}

async fn summary(
    State(st): State<Arc<Dash>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !authed(&st, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let account = q.get("account").map(|s| s.as_str()).unwrap_or("paper");
    Json(build_summary(&st, account)).into_response()
}

fn count(acct: &Account, sql: &str) -> i64 {
    acct.storage
        .query_json(sql)
        .ok()
        .and_then(|r| r.first().and_then(|v| v.get("n").and_then(|n| n.as_i64())))
        .unwrap_or(0)
}

fn account_pills(app: &App) -> Vec<Value> {
    ["paper", "live"]
        .iter()
        .map(|n| match app.account(n) {
            Some(a) => {
                let eq = lock(&a.exposure).equity();
                json!({"name": n, "enabled": true, "equity": eq.to_string(), "bankroll": a.bankroll.to_string(), "halted": a.halt.is_halted()})
            }
            None => json!({"name": n, "enabled": false}),
        })
        .collect()
}

fn calibration_json(app: &App) -> (Value, Vec<Value>) {
    let cal = lock(&app.calibrator);
    let sc = lock(&app.scores).clone();
    let oos = lock(&app.oos).clone();
    let rows: Vec<Value> = cal
        .report()
        .iter()
        .map(|b| {
            let range = format!("{:.0}-{:.0}%", b.lo * 100.0, b.hi * 100.0);
            let (pred, obs, gap) = if b.n > 0 {
                (
                    format!("{:.1}%", b.predicted * 100.0),
                    format!("{:.1}%", b.observed * 100.0),
                    format!("{:+.1} pts", (b.observed - b.predicted) * 100.0),
                )
            } else {
                ("-".to_string(), "-".to_string(), "-".to_string())
            };
            json!({"range": range, "n": b.n, "predicted": pred, "observed": obs, "gap": gap})
        })
        .collect();
    let summary = json!({
        "active": cal.active(),
        "settled": cal.total(),
        "min_samples": cal.min_total(),
        "brier": cal.brier(),
        "shrink_weight": app.cfg.risk.forecast.shrink_weight.to_string(),
        "scores": {"n": sc.n, "model": sc.model, "market": sc.market, "blend": sc.blend},
        // Walk-forward, city-day-clustered: fitted only on earlier event dates, one vote per city-day.
        "oos": {
            "forecasts": oos.forecasts,
            "clusters": oos.clusters,
            "model": oos.model,
            "market": oos.market,
            "blend": oos.blend,
        },
    });
    (summary, rows)
}

fn build_summary(st: &Dash, account: &str) -> Value {
    let app = &st.app;
    let accounts = account_pills(app);
    let (calibration, calib_rows) = calibration_json(app);
    let Some(acct) = app.account(account) else {
        return json!({
            "account": account,
            "enabled": false,
            "accounts": accounts,
            "calibration": calibration,
            "calib_rows": calib_rows,
            "version": env!("CARGO_PKG_VERSION"),
        });
    };
    let (equity, cash, dd, exp_total, positions, by_city) = {
        let mut e = lock(&acct.exposure);
        let dd = e.drawdown();
        let mut by_city: HashMap<String, Decimal> = HashMap::new();
        let mut positions: Vec<Value> = Vec::new();
        for p in e.positions() {
            let mark = e.last_mark(&p.token_id).unwrap_or(p.avg_price);
            let cost = p.avg_price * p.size;
            *by_city.entry(p.city.clone()).or_insert(Decimal::ZERO) += cost;
            positions.push(json!({
                "city": p.city,
                "bracket": p.bracket,
                "outcome": p.outcome,
                "size": p.size.to_string(),
                "avg_price": p.avg_price.to_string(),
                "mark": mark.to_string(),
                "cost": cost.to_string(),
                "value": (mark * p.size).to_string(),
            }));
        }
        (e.equity(), e.cash(), dd, e.total_exposure(), positions, by_city)
    };
    let by_city: Vec<Value> = by_city.into_iter().map(|(c, v)| json!({"city": c, "cost": v.to_string()})).collect();
    let (vol, accrued) = acct.storage.rebate_totals().unwrap_or((Decimal::ZERO, Decimal::ZERO));
    let pfx = acct.storage.prefix().to_string();
    let q = |sql: &str| Value::Array(acct.storage.query_json(sql).unwrap_or_default());
    let shared = |sql: &str| Value::Array(app.storage.query_json(sql).unwrap_or_default());
    let shared_count = |sql: &str| -> i64 {
        app.storage
            .query_json(sql)
            .ok()
            .and_then(|r| r.first().and_then(|v| v.get("n").and_then(|n| n.as_i64())))
            .unwrap_or(0)
    };
    let sig_trade = shared_count("SELECT COUNT(*) AS n FROM signals WHERE decision='TRADE'");
    let sig_skip = shared_count("SELECT COUNT(*) AS n FROM signals WHERE decision<>'TRADE'");
    let n_orders = count(acct, &format!("SELECT COUNT(*) AS n FROM {pfx}orders"));
    let n_fills = count(acct, &format!("SELECT COUNT(*) AS n FROM {pfx}fills"));
    // Orders/fills from the current process only. The orders table survives restarts, so the plain
    // all-time counts hid the fact that a 20-hour run had placed nothing.
    let run_start = app
        .storage
        .query_json(&format!(
            "SELECT ts FROM runs WHERE mode LIKE '%:{}' ORDER BY id DESC LIMIT 1",
            acct.name
        ))
        .ok()
        .and_then(|r| r.first().and_then(|v| v.get("ts").and_then(|s| s.as_str()).map(|s| s.to_string())));
    let (orders_since_start, fills_since_start) = match run_start {
        Some(ts) => (
            count(acct, &format!("SELECT COUNT(*) AS n FROM {pfx}orders WHERE ts >= '{ts}'")),
            count(acct, &format!("SELECT COUNT(*) AS n FROM {pfx}fills WHERE ts >= '{ts}'")),
        ),
        None => (0, 0),
    };
    // Per-account execution outcome for each shared signal (paper and live can differ).
    let signals = shared(&format!(
        "SELECT s.ts,s.city,s.strategy,s.bracket,s.model_prob,s.market_ask,s.edge,s.settlement_check,s.decision,s.reason,\
         COALESCE(e.status,'') AS exec_status \
         FROM signals s LEFT JOIN signal_exec e ON e.signal_id=s.id AND e.account='{}' \
         ORDER BY s.id DESC LIMIT 80",
        acct.name
    ));
    json!({
        "account": acct.name,
        "enabled": true,
        "accounts": accounts,
        "version": env!("CARGO_PKG_VERSION"),
        "mode": acct.name,
        "uptime_secs": st.started.elapsed().as_secs(),
        "halted": acct.halt.is_halted(),
        "permanent_halt": acct.halt.is_permanent(),
        "halt_reason": acct.halt.reason(),
        "equity": equity.to_string(),
        "cash": cash.to_string(),
        "drawdown": dd.to_string(),
        "bankroll": acct.bankroll.to_string(),
        "exposure_total": exp_total.to_string(),
        "exposure_cap": (app.cfg.risk.max_total_exposure_pct * equity).to_string(),
        "drawdown_limit": app.cfg.risk.drawdown_kill_pct.to_string(),
        "daily_loss_cap": app.cfg.risk.daily_loss_cap_pct.to_string(),
        "maker_volume": vol.to_string(),
        "accrued_rebate": accrued.to_string(),
        "counts": {
            "signals_trade": sig_trade,
            "signals_skip": sig_skip,
            "orders": n_orders,
            "fills": n_fills,
            "orders_since_start": orders_since_start,
            "fills_since_start": fills_since_start,
        },
        "calibration": calibration,
        "calib_rows": calib_rows,
        "positions": positions,
        "by_city": by_city,
        "equity_history": q(&format!("SELECT ts, equity FROM (SELECT id, ts, equity FROM {pfx}equity_snapshots ORDER BY id DESC LIMIT 600) ORDER BY id")),
        "signals": signals,
        "orders": q(&format!("SELECT ts,side,price,size,post_only,status,filled,fee FROM {pfx}orders ORDER BY id DESC LIMIT 50")),
        "fills": q(&format!("SELECT ts,order_id,price,size,is_maker FROM {pfx}fills ORDER BY id DESC LIMIT 50")),
        "events": shared("SELECT ts,level,kind,message FROM events ORDER BY id DESC LIMIT 80"),
    })
}
