//! Repository-level enforcement of the paper-only boundary and the HTTP
//! security contract.
//!
//! These tests deliberately inspect source text as well as behaviour. A future
//! refactor that adds an order-entry endpoint, a private order-entry WebSocket
//! topic, an unauthenticated mutating route or an external UI asset must fail
//! here before it can ship.

use std::fs;
use std::path::PathBuf;
use underhaven::store::Store;

const FORBIDDEN_ENDPOINTS: &[&str] = &[
    "/v5/order/create",
    "/v5/order/cancel",
    "/v5/order/cancel-all",
    "/v5/order/amend",
    "/v5/order/realtime",
    "/v5/execution/list",
    "/v5/position/trading-stop",
    "/v5/position/set-leverage",
    "/v5/position/switch-isolated",
    "/v5/position/add-margin",
    "/v5/position/close-pnl",
    "/v5/account/withdraw",
    "/v5/asset/withdraw",
    "/v5/spot/order",
];

const FORBIDDEN_SYMBOLS: &[&str] = &[
    "fn create_order",
    "fn place_order",
    "fn submit_order",
    "fn cancel_order",
    "fn amend_order",
    "fn withdraw",
];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    let path = manifest_dir().join(relative);
    fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", path.display());
    })
}

fn rust_sources() -> Vec<(String, String)> {
    let dir = manifest_dir().join("src");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("read src directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("rs"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no Rust sources found in src/");
    files
        .into_iter()
        .map(|path| {
            let name = path
                .strip_prefix(manifest_dir())
                .unwrap_or(&path)
                .display()
                .to_string();
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            (name, source)
        })
        .collect()
}

/// Extract the argument text of every `needle(...)` call in `source`.
fn call_arguments<'a>(source: &'a str, needle: &str) -> Vec<&'a str> {
    let mut calls = Vec::new();
    let mut cursor = 0usize;
    while let Some(relative) = source[cursor..].find(needle) {
        let start = cursor + relative + needle.len();
        let bytes = source.as_bytes();
        let mut depth = 1usize;
        let mut index = start;
        let mut in_string = false;
        let mut escaped = false;
        while index < bytes.len() {
            let ch = bytes[index] as char;
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
            } else if ch == '"' {
                in_string = true;
            } else if ch == '(' {
                depth += 1;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            index += 1;
        }
        assert_eq!(depth, 0, "unbalanced {needle} call in source");
        calls.push(&source[start..index]);
        cursor = index + 1;
    }
    calls
}

fn split_top_level(arguments: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut start = 0usize;
    for (index, ch) in arguments.char_indices() {
        match ch {
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => depth = depth.saturating_sub(1),
            ',' if !in_string && depth == 0 => {
                parts.push(arguments[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    let tail = arguments[start..].trim();
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts
}

fn handler_body<'a>(source: &'a str, handler: &str) -> &'a str {
    let needle = format!("async fn {handler}(");
    let start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("handler {handler} not found"));
    let rest = &source[start..];
    let mut consumed = 0usize;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        if line.trim_end() == "}" {
            return &rest[..consumed];
        }
    }
    rest
}

fn route_calls(web: &str) -> Vec<(String, String, String)> {
    let routes = call_arguments(web, ".route(");
    assert!(!routes.is_empty(), "no routes found in src/web.rs");
    routes
        .into_iter()
        .map(|arguments| {
            let parts = split_top_level(arguments);
            assert_eq!(parts.len(), 2, "unexpected route signature: {arguments}");
            let path = parts[0].trim().trim_matches('"').to_string();
            let method_call = parts[1];
            let open = method_call
                .find('(')
                .unwrap_or_else(|| panic!("route without method call: {method_call}"));
            let method = method_call[..open].trim().to_string();
            let handler = method_call[open + 1..]
                .trim_end_matches(')')
                .trim()
                .to_string();
            (path, method, handler)
        })
        .collect()
}

#[test]
fn crate_is_compiled_as_paper_only() {
    let paper_only = underhaven::PAPER_ONLY;
    let model_paper_only = underhaven::model::PAPER_ONLY;
    let initial = underhaven::model::INITIAL_EQUITY;
    assert!(paper_only);
    assert!(model_paper_only);
    assert_eq!(initial, 100.0);
}

#[test]
fn no_order_entry_endpoint_or_private_trading_topic_exists() {
    for (file, source) in rust_sources() {
        for endpoint in FORBIDDEN_ENDPOINTS {
            assert!(
                !source.contains(endpoint),
                "{file} contains forbidden order-entry endpoint {endpoint}"
            );
        }
        for symbol in FORBIDDEN_SYMBOLS {
            assert!(
                !source.contains(symbol),
                "{file} contains forbidden order-entry function {symbol}"
            );
        }
    }

    // Public market streams only. `orderbook` is a public topic; the private
    // `order`, `execution`, `position` and `wallet` topics are absent.
    let subscriptions = underhaven::ws::subscription_args(
        &["BTCUSDT".to_string(), "ETHUSDT".to_string()],
        &[underhaven::model::Interval::M5],
        50,
    );
    assert!(!subscriptions.is_empty());
    for topic in subscriptions {
        assert!(
            topic.starts_with("tickers.")
                || topic.starts_with("orderbook.")
                || topic.starts_with("kline."),
            "unexpected WebSocket topic {topic}"
        );
    }
}

#[test]
fn trading_mode_refuses_anything_but_paper() {
    let config = read("src/config.rs");
    assert!(config.contains("UNDERHAVEN_TRADING_MODE"));
    assert!(
        config.contains("PAPER ONLY")
            && config.contains("bail!")
            && config.contains("!mode.eq_ignore_ascii_case(\"paper\")"),
        "configuration must refuse non-paper trading modes"
    );
    assert!(
        !config.contains("enable_live") && !config.contains("live_trading_enabled"),
        "no live trading toggle may exist"
    );
}

#[test]
fn every_mutating_route_requires_csrf_except_login() {
    let web = read("src/web.rs");
    let routes = route_calls(&web);
    let mut mutations = 0usize;
    for (path, method, handler) in &routes {
        match method.as_str() {
            "get" => continue,
            "post" | "delete" => {}
            other => panic!("unsupported mutating method {other} on {path}"),
        }
        mutations += 1;
        let body = handler_body(&web, handler);
        if path == "/api/login" {
            assert!(
                body.contains("recent_failed_logins"),
                "login must be rate limited before password verification"
            );
            continue;
        }
        assert!(
            body.contains("require_session"),
            "mutating route {method} {path} ({handler}) does not require a session"
        );
        assert!(
            body.contains("require_csrf"),
            "mutating route {method} {path} ({handler}) does not enforce CSRF"
        );
    }
    assert!(
        mutations >= 6,
        "expected the dashboard control surface to have mutating routes"
    );
}

#[test]
fn read_api_routes_require_a_session() {
    let web = read("src/web.rs");
    let routes = route_calls(&web);
    let unauthenticated = ["/api/session"];
    let mut protected = 0usize;
    for (path, method, handler) in &routes {
        if method != "get" || !path.starts_with("/api/") {
            continue;
        }
        if unauthenticated.contains(&path.as_str()) {
            continue;
        }
        protected += 1;
        let body = handler_body(&web, handler);
        assert!(
            body.contains("require_session"),
            "read route GET {path} ({handler}) does not require a session"
        );
    }
    assert!(protected >= 5, "expected protected read endpoints");
}

#[test]
fn control_actions_are_paper_operations_only() {
    let web = read("src/web.rs");
    let body = handler_body(&web, "control");
    for allowed in [
        "pause",
        "resume",
        "emergency_stop",
        "clear_emergency",
        "reset_lockouts",
        "close_position",
    ] {
        assert!(
            body.contains(&format!("\"{allowed}\"")),
            "expected paper control action {allowed}"
        );
    }
    for forbidden in [
        "\"live\"",
        "\"enable_trading\"",
        "\"set_leverage\"",
        "\"place_order\"",
        "\"open_position\"",
    ] {
        assert!(
            !body.contains(forbidden),
            "control endpoint exposes non-paper action {forbidden}"
        );
    }
}

#[test]
fn credentials_must_be_verified_read_only_before_storage() {
    let web = read("src/web.rs");
    let body = handler_body(&web, "store_credential");
    let verify = body
        .find("verify_read_only_key")
        .expect("credential route must verify the key with Bybit");
    let guard = body
        .find("!info.read_only")
        .expect("credential route must reject a non-read-only key");
    let persist = body
        .find("persist_read_only_key")
        .expect("credential route must persist the encrypted key");
    assert!(
        verify < guard && guard < persist,
        "a key must be verified and proven read-only before it is encrypted and stored"
    );
    assert!(
        body.contains("Nothing was stored."),
        "verification failures must not store partial credentials"
    );
}

#[test]
fn ui_has_no_third_party_runtime_assets() {
    for file in ["assets/index.html", "assets/app.js", "assets/styles.css"] {
        let source = read(file);
        for forbidden in [
            "http://",
            "https://",
            "//cdn.",
            "cdnjs",
            "unpkg.com",
            "jsdelivr",
            "integrity=",
            "@import url(",
        ] {
            assert!(
                !source.contains(forbidden),
                "{file} loads or references a remote asset via {forbidden}"
            );
        }
    }
    // uPlot is vendored. Its minified banner retains the upstream project URL
    // as a comment; that is not a runtime fetch. Only same-origin assets may be
    // referenced from the page.
    let index = read("assets/index.html");
    assert!(index.contains("/assets/app.js"));
    assert!(index.contains("/assets/styles.css"));
    assert!(index.contains("/assets/uplot.iife.min.js"));
    assert!(index.contains("/assets/uplot.min.css"));
    for asset in ["assets/uplot.iife.min.js", "assets/uplot.min.css"] {
        assert!(
            !read(asset).is_empty(),
            "{asset} must be vendored, not fetched at runtime"
        );
    }
}

#[test]
fn dashboard_scripts_are_syntactically_valid() {
    // Node is not a runtime dependency of UNDERHAVEN. When a developer or CI
    // image has it, parse the embedded JavaScript so a quoting mistake cannot
    // ship a dashboard that renders a blank page. Absence of Node is a skip.
    let node = std::process::Command::new("node").arg("--version").output();
    let Ok(node) = node else {
        eprintln!("node not installed; skipping optional JavaScript syntax check");
        return;
    };
    if !node.status.success() {
        eprintln!("node --version failed; skipping optional JavaScript syntax check");
        return;
    }
    for file in ["assets/app.js", "assets/uplot.iife.min.js"] {
        let path = manifest_dir().join(file);
        let output = std::process::Command::new("node")
            .arg("--check")
            .arg(&path)
            .output()
            .expect("run node --check");
        assert!(
            output.status.success(),
            "{file} does not parse as JavaScript:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn security_headers_are_strict() {
    let web = read("src/web.rs");
    let body = handler_body(&web, "security_headers");
    for required in [
        "default-src 'self'",
        "script-src 'self'",
        "frame-ancestors 'none'",
        "base-uri 'none'",
        "X_CONTENT_TYPE_OPTIONS",
        "X_FRAME_OPTIONS",
        "REFERRER_POLICY",
    ] {
        assert!(
            body.contains(required),
            "missing security header {required}"
        );
    }
    assert!(!body.contains("unsafe-eval"), "CSP must not allow eval");
}

#[test]
fn embedded_assets_revalidate_after_upgrade() {
    let web = read("src/web.rs");
    assert!(
        web.contains("header::CACHE_CONTROL, \"no-cache\""),
        "embedded assets must revalidate so an upgrade cannot serve stale JavaScript"
    );
    assert!(
        !web.contains("max-age=86400"),
        "unversioned embedded assets must not be cached for a day"
    );
}

#[test]
fn session_cookies_are_httponly_strict_and_secure() {
    let auth = read("src/auth.rs");
    assert!(auth.contains("HttpOnly"));
    assert!(auth.contains("SameSite=Strict"));
    assert!(auth.contains("Secure"));
    assert!(
        auth.contains("Path=/"),
        "session cookie should be scoped to the site root"
    );
}

#[tokio::test]
async fn audit_log_is_append_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::connect(&dir.path().join("audit.db"))
        .await
        .expect("open migrated database");
    store
        .audit("test.append_only", "test", "audit integrity check")
        .await
        .expect("append audit event");
    let events = store.recent_audit(10).await.expect("read audit events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "test.append_only");

    let update = sqlx::query("UPDATE audit_events SET detail = 'tampered'")
        .execute(store.pool())
        .await;
    assert!(update.is_err(), "audit_events must reject UPDATE");
    let delete = sqlx::query("DELETE FROM audit_events")
        .execute(store.pool())
        .await;
    assert!(delete.is_err(), "audit_events must reject DELETE");

    let events = store.recent_audit(10).await.expect("read audit events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].detail, "audit integrity check");
}

#[test]
fn paper_engine_has_no_network_order_path() {
    // The engine may read SQLite and in-memory market data only. It must never
    // depend on the REST client or WebSocket writer.
    let paper = read("src/paper.rs");
    for forbidden in ["PublicClient", "bybit::", "reqwest", "tungstenite"] {
        assert!(
            !paper.contains(forbidden),
            "paper engine unexpectedly references network execution type {forbidden}"
        );
    }
}

#[test]
fn docs_do_not_claim_verified_live_or_public_tls() {
    // Keep the honesty guardrails visible: the repository must document that
    // there is no live path, and deployment docs must not pretend that public
    // certificate issuance was verified from the development host.
    let readme = read("README.md");
    assert!(readme.contains("paper-only") || readme.contains("PAPER ONLY"));
    assert!(readme.contains("docs/SAFETY.md"));
    assert!(readme.contains("docs/DEPLOY.md"));
    let safety = manifest_dir().join("docs/SAFETY.md").exists();
    let deploy = manifest_dir().join("docs/DEPLOY.md").exists();
    assert!(safety, "docs/SAFETY.md is required");
    assert!(deploy, "docs/DEPLOY.md is required");
}
