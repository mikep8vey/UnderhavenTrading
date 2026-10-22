//! Authenticated HTTP layer: dashboard, SSE snapshots and JSON control API.
//!
//! Security posture:
//!
//! * Every `/api/*` route except `/api/session` and `/api/login` requires a
//!   live session cookie.
//! * Every mutation additionally requires the per-session `X-CSRF-Token`.
//! * The session cookie is `HttpOnly; SameSite=Strict` and `Secure` whenever the
//!   deployment terminates TLS.
//! * Failed logins are rate limited per client IP before Argon2 is invoked.
//! * The optional Bybit key is verified as read-only over a signed public REST
//!   call before it is encrypted and stored; it is never returned to a browser.
//! * The UI is embedded in the binary; there is no CDN and no third-party
//!   JavaScript at runtime.

use crate::app::{AppState, BacktestRequest};
use crate::auth::{cookie_value, csrf_matches, COOKIE_NAME};
use crate::model::{BacktestReport, ConfigView, Interval, StrategyId};
use crate::store::SessionRow;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

const SESSION_COOKIE_MAX_AGE_SECS: i64 = 12 * 60 * 60;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/styles.css", get(styles_css))
        .route("/assets/uplot.iife.min.js", get(uplot_js))
        .route("/assets/uplot.min.css", get(uplot_css))
        .route("/healthz", get(health))
        .route("/api/session", get(session_info))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .route("/api/dashboard", get(dashboard))
        .route("/api/stream", get(stream))
        .route("/api/config", get(config))
        .route("/api/control", post(control))
        .route("/api/backtest", post(run_backtest))
        .route("/api/backtests", get(list_backtests))
        .route("/api/backtests/{run_id}", get(get_backtest))
        .route("/api/backtests/{run_id}/trades.csv", get(backtest_csv))
        .route("/api/history/import", post(import_history))
        .route("/api/trades.csv", get(trades_csv))
        .route("/api/credentials", post(store_credential))
        .route("/api/credentials", delete(delete_credential))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state)
}

/// Bind the configured address and serve until shutdown is cancelled.
pub async fn serve(state: Arc<AppState>, shutdown: CancellationToken) -> anyhow::Result<()> {
    let bind = state.cfg.bind;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!(%bind, "dashboard listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.cancelled().await })
    .await?;
    Ok(())
}

async fn security_headers(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; font-src 'self'; \
             frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    if state.cfg.secure_cookies {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Embedded UI assets
// ---------------------------------------------------------------------------

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        include_str!("../assets/index.html"),
    )
        .into_response()
}

async fn app_js() -> Response {
    static_asset(
        "application/javascript; charset=utf-8",
        include_str!("../assets/app.js"),
    )
}

async fn styles_css() -> Response {
    static_asset(
        "text/css; charset=utf-8",
        include_str!("../assets/styles.css"),
    )
}

async fn uplot_js() -> Response {
    static_asset(
        "application/javascript; charset=utf-8",
        include_str!("../assets/uplot.iife.min.js"),
    )
}

async fn uplot_css() -> Response {
    static_asset(
        "text/css; charset=utf-8",
        include_str!("../assets/uplot.min.css"),
    )
}

fn static_asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // Assets are embedded in the binary and their URLs are not
            // content-hashed. Revalidate on every load so an upgrade cannot be
            // paired with a stale dashboard script or stylesheet.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
    retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication required")
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }

    fn too_many(retry_after: u64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed login attempts; wait before trying again",
        );
        error.retry_after = Some(retry_after);
        error
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::internal(format!("{error:#}"))
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::internal(format!("database error: {error}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Sessions and authentication
// ---------------------------------------------------------------------------

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| cookie_value(value, COOKIE_NAME))
}

async fn require_session(state: &AppState, headers: &HeaderMap) -> ApiResult<SessionRow> {
    let token = session_token(headers).ok_or_else(ApiError::unauthorized)?;
    state
        .auth
        .session(&token)
        .await
        .ok_or_else(ApiError::unauthorized)
}

fn require_csrf(session: &SessionRow, headers: &HeaderMap) -> ApiResult<()> {
    let provided = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if csrf_matches(&session.csrf_token, provided) {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "missing or invalid CSRF token",
        ))
    }
}

fn peer_ip(headers: &HeaderMap, peer: SocketAddr, trust_forwarded: bool) -> String {
    if trust_forwarded {
        for name in ["x-forwarded-for", "x-real-ip"] {
            if let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) {
                let first = value.split(',').next().unwrap_or("").trim();
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    peer.ip().to_string()
}

#[derive(Serialize)]
struct SessionInfo {
    authenticated: bool,
    paper_only: bool,
    version: String,
    username: Option<String>,
    csrf_token: Option<String>,
    expires_at_ms: Option<i64>,
    secure_cookies: bool,
    public_url: Option<String>,
    tls_mode: String,
    build_commit: String,
}

async fn session_info(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<SessionInfo> {
    let session = match session_token(&headers) {
        Some(token) => state.auth.session(&token).await,
        None => None,
    };
    Json(SessionInfo {
        authenticated: session.is_some(),
        paper_only: true,
        version: crate::config::BUILD_VERSION.to_string(),
        username: session.as_ref().map(|s| s.username.clone()),
        csrf_token: session.as_ref().map(|s| s.csrf_token.clone()),
        expires_at_ms: session.as_ref().map(|s| s.expires_at_ms),
        secure_cookies: state.cfg.secure_cookies,
        public_url: state.cfg.public_url.clone(),
        tls_mode: state.cfg.tls_mode.clone(),
        build_commit: crate::app::GIT_COMMIT.to_string(),
    })
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    username: String,
    csrf_token: String,
    expires_at_ms: i64,
}

async fn login(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let username = request.username.trim();
    if username.is_empty() || username.len() > 64 {
        return Err(ApiError::bad_request("invalid username or password"));
    }
    if request.password.is_empty() || request.password.len() > 1024 {
        return Err(ApiError::bad_request("invalid username or password"));
    }

    let ip = peer_ip(
        &headers,
        peer,
        state.cfg.bind.ip().is_loopback() || state.cfg.allow_insecure_http,
    );
    let now = chrono::Utc::now().timestamp_millis();
    let window_start = now - state.cfg.login_window_secs.max(60) * 1000;
    let failures = state.store.recent_failed_logins(&ip, window_start).await?;
    if failures >= i64::from(state.cfg.login_max_attempts.max(1)) {
        warn!(%ip, failures, "login rate limit hit");
        let retry = state.cfg.login_window_secs.max(60) as u64;
        return Err(ApiError::too_many(retry));
    }

    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown");
    let session = state
        .auth
        .login(username, &request.password, &ip, user_agent)
        .await
        .map_err(|error| {
            warn!(%ip, "failed login");
            ApiError::new(StatusCode::UNAUTHORIZED, error.to_string())
        })?;

    let cookie = state
        .auth
        .cookie_header(&session.token, SESSION_COOKIE_MAX_AGE_SECS);
    let mut response = Json(LoginResponse {
        username: username.to_string(),
        csrf_token: session.csrf_token.clone(),
        expires_at_ms: session.expires_at_ms,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    Ok(response)
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let session = require_session(&state, &headers).await?;
    let token = session_token(&headers).ok_or_else(ApiError::unauthorized)?;
    require_csrf(&session, &headers)?;
    state.auth.logout(&token, &session.username).await?;
    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&state.auth.clear_cookie()).map_err(ApiError::internal)?,
    );
    Ok(response)
}

#[derive(Deserialize)]
struct PasswordRequest {
    current_password: String,
    new_password: String,
}

async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<PasswordRequest>,
) -> ApiResult<Response> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    let user = state
        .store
        .user_by_name(&session.username)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    state
        .auth
        .change_password(&user, &request.current_password, &request.new_password)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    // All sessions, including this one, are revoked by the auth layer.
    let mut response =
        Json(serde_json::json!({ "ok": true, "sessions_revoked": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&state.auth.clear_cookie()).map_err(ApiError::internal)?,
    );
    Ok(response)
}

// ---------------------------------------------------------------------------
// Read API
// ---------------------------------------------------------------------------

async fn health() -> Json<crate::app::Health> {
    Json(crate::app::health())
}

async fn dashboard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<crate::model::DashboardSnapshot>> {
    require_session(&state, &headers).await?;
    Ok(Json(state.dashboard().await?))
}

async fn config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<ConfigView>> {
    require_session(&state, &headers).await?;
    Ok(Json(state.config_view()))
}

async fn stream(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    require_session(&state, &headers).await?;
    let seed = tokio::time::interval(Duration::from_secs(2));
    let stream = futures_util::stream::unfold((state, seed), |(state, mut tick)| async move {
        tick.tick().await;
        let event = match state.dashboard().await {
            Ok(snapshot) => Event::default()
                .event("snapshot")
                .json_data(snapshot)
                .unwrap_or_else(|_| {
                    Event::default()
                        .event("error")
                        .data("snapshot encoding failed")
                }),
            Err(error) => Event::default()
                .event("error")
                .data(format!("dashboard unavailable: {error}")),
        };
        Some((Ok::<_, Infallible>(event), (state, tick)))
    });
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("underhaven"),
    ))
}

// ---------------------------------------------------------------------------
// Control API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ControlRequest {
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    position_id: Option<String>,
}

async fn control(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<ControlRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    let actor = session.username.as_str();
    let reason = request
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("operator action")
        .chars()
        .take(240)
        .collect::<String>();

    match request.action.as_str() {
        "pause" => state.engine.pause(&reason).await?,
        "resume" => state.engine.resume(actor).await?,
        "emergency_stop" => {
            let closed = state.engine.emergency_stop(&reason, actor).await?;
            return Ok(Json(serde_json::json!({
                "ok": true,
                "action": "emergency_stop",
                "closed_positions": closed,
            })));
        }
        "clear_emergency" => state.engine.clear_emergency(actor).await?,
        "reset_lockouts" => state.engine.reset_lockouts(actor).await?,
        "close_position" => {
            let position_id = request
                .position_id
                .as_deref()
                .ok_or_else(|| ApiError::bad_request("position_id is required"))?;
            state.engine.close_by_id(position_id, actor).await?;
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "unknown control action: {other}"
            )))
        }
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "action": request.action,
    })))
}

// ---------------------------------------------------------------------------
// Backtests and exports
// ---------------------------------------------------------------------------

async fn run_backtest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<BacktestRequest>,
) -> ApiResult<Json<BacktestReport>> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    let permit = tokio::time::timeout(
        Duration::from_secs(30),
        state.backtest_slots.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "another backtest is already running",
        )
    })?
    .map_err(ApiError::internal)?;
    let report = state
        .run_web_backtest(request)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    drop(permit);
    Ok(Json(report))
}

#[derive(Serialize)]
struct BacktestListItem {
    run_id: String,
    strategy: StrategyId,
    symbol: String,
    created_at_ms: i64,
    fingerprint: String,
    trades: Option<usize>,
    net_pnl: Option<f64>,
    max_drawdown_pct: Option<f64>,
    oos_status: String,
}

async fn list_backtests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<BacktestListItem>>> {
    require_session(&state, &headers).await?;
    let rows = state.store.list_backtests(60).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            let report = serde_json::from_str::<BacktestReport>(&row.report_json).ok();
            let oos = report
                .as_ref()
                .and_then(|report| report.segments.iter().find(|s| s.name == "out_of_sample"))
                .map(|segment| {
                    if segment.trades < 30 {
                        format!("INSUFFICIENT DATA: {} trades", segment.trades)
                    } else if segment.stats.net_pnl > 0.0 {
                        format!("POSITIVE: net {:.2}", segment.stats.net_pnl)
                    } else {
                        format!("NEGATIVE: net {:.2}", segment.stats.net_pnl)
                    }
                })
                .unwrap_or_else(|| "NOT YET VALIDATED".to_string());
            BacktestListItem {
                run_id: row.run_id,
                strategy: row.strategy,
                symbol: row.symbol,
                created_at_ms: row.created_at_ms,
                fingerprint: row.fingerprint,
                trades: report.as_ref().map(|r| r.result.trades.len()),
                net_pnl: report.as_ref().map(|r| r.result.stats.net_pnl),
                max_drawdown_pct: report.as_ref().map(|r| r.result.stats.max_drawdown_pct),
                oos_status: oos,
            }
        })
        .collect();
    Ok(Json(items))
}

async fn get_backtest(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> ApiResult<Json<BacktestReport>> {
    require_session(&state, &headers).await?;
    let row = state
        .store
        .backtest(&run_id)
        .await?
        .ok_or_else(|| ApiError::not_found("backtest not found"))?;
    let report = serde_json::from_str(&row.report_json)
        .map_err(|error| ApiError::internal(format!("stored report is unreadable: {error}")))?;
    Ok(Json(report))
}

async fn backtest_csv(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> ApiResult<Response> {
    require_session(&state, &headers).await?;
    let row = state
        .store
        .backtest(&run_id)
        .await?
        .ok_or_else(|| ApiError::not_found("backtest not found"))?;
    let report: BacktestReport = serde_json::from_str(&row.report_json)
        .map_err(|error| ApiError::internal(format!("stored report is unreadable: {error}")))?;
    csv_response(
        &format!("underhaven-{run_id}-trades.csv"),
        crate::backtest::trades_csv(&report.result),
    )
}

async fn trades_csv(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    require_session(&state, &headers).await?;
    let trades = state.store.closed_trades(None, 10_000).await?;
    let mut out = String::from(
        "id,strategy,symbol,direction,qty,entry_price,exit_price,entry_time_ms,exit_time_ms,gross_pnl,fees,funding,slippage_est,net_pnl,close_reason,bars_held,simulated_fill\n",
    );
    for trade in trades {
        out.push_str(&format!(
            "{},{},{},{},{:.8},{:.8},{:.8},{},{},{:.8},{:.8},{:.8},{:.8},{:.8},{},{},{}\n",
            trade.id,
            trade.strategy,
            trade.symbol,
            trade.direction,
            trade.qty,
            trade.entry_price,
            trade.exit_price,
            trade.entry_time_ms,
            trade.exit_time_ms,
            trade.gross_pnl,
            trade.fees,
            trade.funding,
            trade.slippage_est,
            trade.net_pnl,
            trade.close_reason,
            trade.bars_held,
            trade.simulated_fill
        ));
    }
    csv_response("underhaven-paper-trades.csv", out)
}

fn csv_response(filename: &str, body: String) -> ApiResult<Response> {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    let disposition = format!("attachment; filename=\"{filename}\"");
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).map_err(ApiError::internal)?,
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[derive(Deserialize)]
struct HistoryRequest {
    symbol: String,
    interval: String,
    #[serde(default)]
    days: Option<i64>,
}

async fn import_history(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<HistoryRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    let interval: Interval = request
        .interval
        .parse()
        .map_err(|error: String| ApiError::bad_request(error))?;
    let symbol = request.symbol.trim().to_ascii_uppercase();
    if !symbol.ends_with("USDT") || symbol.len() < 6 {
        return Err(ApiError::bad_request("symbol must be a USDT perpetual"));
    }
    let days = request.days.unwrap_or(90).clamp(1, 365);
    let permit = tokio::time::timeout(
        Duration::from_secs(30),
        state.backtest_slots.clone().acquire_owned(),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "another history/backtest job is running",
        )
    })?
    .map_err(ApiError::internal)?;
    let count = crate::app::import_history(&state, &symbol, interval, days)
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    drop(permit);
    Ok(Json(serde_json::json!({
        "ok": true,
        "symbol": symbol,
        "interval": interval.slug(),
        "candles": count,
        "days": days,
    })))
}

// ---------------------------------------------------------------------------
// Optional read-only Bybit credential
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CredentialRequest {
    #[serde(default = "default_key_name")]
    name: String,
    api_key: String,
    api_secret: String,
    #[serde(default)]
    note: Option<String>,
}

fn default_key_name() -> String {
    "primary".to_string()
}

async fn store_credential(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<CredentialRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    let credentials = crate::app::parse_credentials(&request.api_key, &request.api_secret)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let info = state
        .rest_client
        .verify_read_only_key(&credentials)
        .await
        .map_err(|error| {
            ApiError::bad_request(format!(
                "Bybit key verification failed: {error}. Nothing was stored."
            ))
        })?;
    if !info.read_only {
        return Err(ApiError::bad_request(
            "refusing to store a key that Bybit does not report as read-only",
        ));
    }
    state
        .auth
        .persist_read_only_key(
            request.name.trim(),
            info.uid.as_deref(),
            request.note.as_deref(),
            &request.api_key,
            &request.api_secret,
            &info,
        )
        .await
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    crate::app::refresh_private_status(&state).await;
    Ok(Json(serde_json::json!({
        "ok": true,
        "read_only_verified": true,
        "uid": info.uid,
        "note": info.note,
        "message": "read-only key verified, encrypted and stored; it is never returned to the browser",
    })))
}

async fn delete_credential(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    let session = require_session(&state, &headers).await?;
    require_csrf(&session, &headers)?;
    state.store.delete_credential().await?;
    state
        .store
        .audit(
            "credentials.delete",
            &session.username,
            "read-only key removed",
        )
        .await?;
    {
        let mut status = state.bybit.write().await;
        status.private_connected = false;
        status.private_read_only = false;
        status.read_only_verified = false;
        status.uid = None;
        status.key_note = None;
        status.fee_source = "assumed_conservative (no read-only key configured)".into();
        status.error = None;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
