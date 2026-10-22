/* UNDERHAVEN BYBIT TRADING LAB — dashboard client (no third-party JS) */
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    session: null,
    snapshot: null,
    config: null,
    charts: new Map(),
    stream: null,
    toastTimer: null,
    report: null,
    scanFilter: "",
  };

  const STRATEGY_LABEL = {
    trend: "Trend Following",
    mean_reversion: "Mean Reversion",
    breakout: "Breakout / Momentum",
  };

  // ---------------------------------------------------------------------
  // Utilities
  // ---------------------------------------------------------------------
  function esc(value) {
    return String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  function money(value, digits = 2) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    const number = Number(value);
    const sign = number < 0 ? "-" : "";
    return sign + "$" + Math.abs(number).toLocaleString(undefined, {
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
    });
  }

  function signedMoney(value, digits = 2) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    const number = Number(value);
    return (number > 0 ? "+" : number < 0 ? "-" : "") + "$" +
      Math.abs(number).toLocaleString(undefined, {
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
      });
  }

  function pct(value, digits = 2) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    const number = Number(value);
    return (number > 0 ? "+" : "") + number.toFixed(digits) + "%";
  }

  function rawPct(value, digits = 1) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    return Number(value).toFixed(digits) + "%";
  }

  function price(value) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    const number = Number(value);
    const digits = number >= 1000 ? 2 : number >= 1 ? 3 : number >= 0.01 ? 5 : 7;
    return number.toLocaleString(undefined, {
      minimumFractionDigits: digits,
      maximumFractionDigits: digits,
    });
  }

  function qty(value) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    return Number(value).toLocaleString(undefined, { maximumFractionDigits: 6 });
  }

  function ratio(value, digits = 2) {
    if (value == null || !Number.isFinite(Number(value))) return "—";
    return Number(value).toFixed(digits);
  }

  function signClass(value) {
    if (value == null || !Number.isFinite(Number(value))) return "";
    return Number(value) > 0 ? "pos" : Number(value) < 0 ? "neg" : "";
  }

  function clock(ms) {
    if (!ms) return "—";
    const date = new Date(Number(ms));
    return date.toLocaleString(undefined, {
      month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
    });
  }

  function timeOnly(ms) {
    if (!ms) return "—";
    return new Date(Number(ms)).toLocaleTimeString(undefined, {
      hour: "2-digit", minute: "2-digit", second: "2-digit",
    });
  }

  function age(ms) {
    if (!ms) return "—";
    const seconds = Math.max(0, Math.floor((Date.now() - Number(ms)) / 1000));
    if (seconds < 60) return seconds + "s";
    if (seconds < 3600) return Math.floor(seconds / 60) + "m";
    if (seconds < 86400) return Math.floor(seconds / 3600) + "h " + Math.floor((seconds % 3600) / 60) + "m";
    return Math.floor(seconds / 86400) + "d " + Math.floor((seconds % 86400) / 3600) + "h";
  }

  function duration(seconds) {
    if (seconds == null) return "—";
    const s = Math.max(0, Math.floor(Number(seconds)));
    const d = Math.floor(s / 86400);
    const h = Math.floor((s % 86400) / 3600);
    const m = Math.floor((s % 3600) / 60);
    if (d > 0) return d + "d " + h + "h";
    if (h > 0) return h + "h " + m + "m";
    return m + "m " + (s % 60) + "s";
  }

  function bytes(value) {
    if (value == null) return "—";
    const n = Number(value);
    if (n < 1024) return n + " B";
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + " KB";
    return (n / 1024 / 1024).toFixed(2) + " MB";
  }

  function setText(id, value, className) {
    const el = $(id);
    if (!el) return;
    el.textContent = value == null ? "—" : String(value);
    if (className !== undefined) el.className = className;
  }

  function tag(value, cls) {
    return '<span class="tag ' + (cls || "") + '">' + esc(value) + "</span>";
  }

  function kvRow(key, value, cls) {
    return '<span class="k">' + esc(key) + '</span><span class="v ' + (cls || "") + '">' + esc(value) + "</span>";
  }

  function toast(message, bad = false) {
    const el = $("toast");
    if (!el) return;
    el.textContent = message;
    el.classList.toggle("bad", !!bad);
    el.hidden = false;
    if (state.toastTimer) clearTimeout(state.toastTimer);
    state.toastTimer = setTimeout(() => { el.hidden = true; }, 5200);
  }

  function hexFill(hex, alpha) {
    const clean = String(hex).replace("#", "");
    const r = parseInt(clean.slice(0, 2), 16);
    const g = parseInt(clean.slice(2, 4), 16);
    const b = parseInt(clean.slice(4, 6), 16);
    return "rgba(" + r + "," + g + "," + b + "," + alpha + ")";
  }

  // ---------------------------------------------------------------------
  // HTTP
  // ---------------------------------------------------------------------
  async function api(path, options = {}) {
    const method = (options.method || "GET").toUpperCase();
    const headers = Object.assign({ Accept: "application/json" }, options.headers || {});
    let body = options.body;
    if (body && typeof body !== "string") {
      headers["Content-Type"] = "application/json";
      body = JSON.stringify(body);
    }
    if (method !== "GET" && method !== "HEAD" && state.session && state.session.csrf_token) {
      headers["X-CSRF-Token"] = state.session.csrf_token;
    }
    const response = await fetch(path, {
      method,
      headers,
      body,
      credentials: "same-origin",
      cache: "no-store",
    });
    let payload = null;
    const type = response.headers.get("content-type") || "";
    if (type.includes("application/json")) {
      payload = await response.json().catch(() => null);
    } else {
      payload = await response.text().catch(() => "");
    }
    if (!response.ok) {
      const message = payload && payload.error ? payload.error : "HTTP " + response.status;
      const error = new Error(message);
      error.status = response.status;
      if (response.status === 401 && path !== "/api/login") {
        showLogin("Session expired. Sign in again.");
      }
      throw error;
    }
    return payload;
  }

  // ---------------------------------------------------------------------
  // Session lifecycle
  // ---------------------------------------------------------------------
  async function boot() {
    try {
      const session = await api("/api/session");
      if (session.authenticated) {
        enterApp(session);
      } else {
        showLogin();
      }
    } catch (error) {
      showLogin(error.message);
    }
  }

  function showLogin(message) {
    if (state.stream) {
      state.stream.close();
      state.stream = null;
    }
    $("app").hidden = true;
    $("loginShell").hidden = false;
    const error = $("loginError");
    if (error) error.textContent = message || "";
  }

  function enterApp(session) {
    state.session = session;
    $("loginShell").hidden = true;
    $("app").hidden = false;
    setText("userName", session.username || "admin");
    setText("footerVersion", session.version || "paper-only build");
    renderTls(session);
    startStream();
    loadConfig();
    loadBacktests();
    api("/api/dashboard").then(render).catch((error) => toast(error.message, true));
  }

  async function login(event) {
    event.preventDefault();
    const button = $("loginButton");
    const errorEl = $("loginError");
    if (errorEl) errorEl.textContent = "";
    button.disabled = true;
    try {
      const session = await api("/api/login", {
        method: "POST",
        body: {
          username: $("loginUser").value.trim(),
          password: $("loginPass").value,
        },
      });
      state.session = {
        authenticated: true,
        username: session.username,
        csrf_token: session.csrf_token,
        expires_at_ms: session.expires_at_ms,
      };
      $("loginPass").value = "";
      enterApp(state.session);
    } catch (error) {
      if (errorEl) errorEl.textContent = error.message;
    } finally {
      button.disabled = false;
    }
  }

  async function logout() {
    try {
      await api("/api/logout", { method: "POST" });
    } catch (error) {
      // A dead session is the same end state from the operator's perspective.
    }
    state.session = null;
    showLogin("Signed out.");
  }

  function renderTls(session) {
    const chip = $("chipTls");
    if (!chip) return;
    const mode = String(session.tls_mode || "unknown").toLowerCase();
    const labels = {
      letsencrypt: ["TLS Let's Encrypt", "ok"],
      self_signed: ["TLS self-signed", "warn"],
      proxy: ["TLS via proxy", "ok"],
      none: ["TLS off", "bad"],
      unknown: ["TLS unverified", "warn"],
    };
    const entry = labels[mode] || labels.unknown;
    chip.hidden = false;
    chip.className = "chip " + entry[1];
    chip.title = session.public_url ? "Public URL: " + session.public_url : "No public URL configured";
    chip.innerHTML = '<span class="dot"></span><span class="chip-text">' + esc(entry[0]) + "</span>";
  }

  function startStream() {
    if (state.stream) state.stream.close();
    if (!window.EventSource) {
      toast("This browser has no EventSource support; dashboard will not update.", true);
      return;
    }
    const stream = new EventSource("/api/stream");
    state.stream = stream;
    stream.addEventListener("snapshot", (event) => {
      try {
        render(JSON.parse(event.data));
      } catch (error) {
        // Ignore a single malformed frame; the next snapshot will recover.
      }
    });
    stream.addEventListener("error", () => {
      const chip = $("chipFeed");
      if (chip) chip.className = "chip bad";
    });
  }

  // ---------------------------------------------------------------------
  // Charts
  // ---------------------------------------------------------------------
  function ensureChart(key, elementId, options) {
    if (!window.uPlot) return null;
    const existing = state.charts.get(key);
    if (existing) return existing;
    const el = $(elementId);
    if (!el) return null;
    const width = Math.max(el.clientWidth || 260, 120);
    const height = Math.max(el.clientHeight || options.height || 120, 60);
    const opts = Object.assign({}, options, { width, height });
    let chart;
    try {
      chart = new window.uPlot(opts, [[], []], el);
    } catch (error) {
      return null;
    }
    state.charts.set(key, chart);
    if (window.ResizeObserver) {
      const observer = new ResizeObserver(() => {
        const w = el.clientWidth;
        const h = el.clientHeight || height;
        if (w > 0) {
          try { chart.setSize({ width: w, height: h }); } catch (error) { /* detached */ }
        }
      });
      observer.observe(el);
    }
    return chart;
  }

  const timeAxis = {
    stroke: "#52687c",
    grid: { stroke: "rgba(23,38,54,0.55)" },
    ticks: { stroke: "rgba(23,38,54,0.8)" },
    font: "10px ui-monospace, monospace",
    values: (u, splits) => splits.map((v) => {
      const d = new Date(v * 1000);
      return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
    }),
  };

  const valueAxis = {
    stroke: "#52687c",
    grid: { stroke: "rgba(23,38,54,0.55)" },
    ticks: { stroke: "rgba(23,38,54,0.8)" },
    font: "10px ui-monospace, monospace",
    size: 56,
  };

  function equityOptions(height) {
    return {
      height: height,
      legend: { show: false },
      cursor: { show: true, y: false },
      scales: { x: { time: true } },
      axes: [timeAxis, valueAxis],
      series: [
        {},
        {
          stroke: "#39e2c3",
          width: 1.8,
          fill: hexFill("#39e2c3", 0.1),
          points: { show: false },
        },
      ],
    };
  }

  function sparkOptions(color) {
    return {
      height: 74,
      legend: { show: false },
      cursor: { show: false },
      scales: { x: { time: true } },
      axes: [{ show: false }, { show: false }],
      series: [
        {},
        {
          stroke: color,
          width: 1.6,
          fill: hexFill(color, 0.12),
          points: { show: false },
        },
      ],
    };
  }

  function curveData(points, valueKey) {
    if (!Array.isArray(points)) return [[], []];
    const xs = [];
    const ys = [];
    points.forEach((point) => {
      const x = Number(point.ts_ms) / 1000;
      const y = Number(point[valueKey || "equity"]);
      if (Number.isFinite(x) && Number.isFinite(y)) {
        xs.push(x);
        ys.push(y);
      }
    });
    return [xs, ys];
  }

  function mergedEquity(strategies) {
    const times = new Set();
    const curves = strategies.map((strategy) => {
      const points = (strategy.equity_curve || []).filter(
        (point) => Number.isFinite(Number(point.ts_ms)) && Number.isFinite(Number(point.equity))
      );
      points.forEach((point) => times.add(Number(point.ts_ms)));
      return { points, index: 0, last: Number(strategy.initial_equity) };
    });
    const sorted = Array.from(times).sort((a, b) => a - b);
    const xs = [];
    const ys = [];
    sorted.forEach((time) => {
      let total = 0;
      curves.forEach((curve) => {
        while (curve.index < curve.points.length && Number(curve.points[curve.index].ts_ms) <= time) {
          curve.last = Number(curve.points[curve.index].equity);
          curve.index += 1;
        }
        total += curve.last;
      });
      xs.push(time / 1000);
      ys.push(total);
    });
    return [xs, ys];
  }

  function updateChart(key, elementId, options, data) {
    const chart = ensureChart(key, elementId, options);
    if (!chart) return;
    if (data[0].length === 1) {
      data[0].push(data[0][0] + 1);
      data[1].push(data[1][0]);
    }
    try { chart.setData(data); } catch (error) { /* ignored */ }
  }

  // ---------------------------------------------------------------------
  // Rendering
  // ---------------------------------------------------------------------
  function render(snapshot) {
    if (!snapshot) return;
    state.snapshot = snapshot;
    try { renderHeader(snapshot); } catch (e) { console.error(e); }
    try { renderAccount(snapshot); } catch (e) { console.error(e); }
    try { renderStrategies(snapshot); } catch (e) { console.error(e); }
    try { renderScanner(snapshot); } catch (e) { console.error(e); }
    try { renderPositions(snapshot); } catch (e) { console.error(e); }
    try { renderTrades(snapshot); } catch (e) { console.error(e); }
    try { renderRisk(snapshot); } catch (e) { console.error(e); }
    try { renderSystem(snapshot); } catch (e) { console.error(e); }
    try { renderSecurity(snapshot); } catch (e) { console.error(e); }
    try { renderAudit(snapshot); } catch (e) { console.error(e); }
    try { renderAlerts(snapshot); } catch (e) { console.error(e); }
  }

  function renderHeader(s) {
    const feedAge = s.generated_at_ms - (s.ws_last_message_ms || s.generated_at_ms);
    setText("mEquity", money(s.equity), "neutral");
    setText("mEquitySub", "initial " + money(s.initial_equity));
    setText("mDaily", signedMoney(s.daily_pnl), signClass(s.daily_pnl));
    setText("mDailySub", "realized " + signedMoney(s.realized) + " · unrealized " + signedMoney(s.unrealized));
    setText("mDrawdown", rawPct(s.drawdown_pct), s.drawdown_pct >= 8 ? "neg" : s.drawdown_pct >= 4 ? "" : "neutral");
    setText("mDrawdownSub", "limit " + rawPct(s.risk ? s.risk.drawdown_limit_pct : null));
    setText("mExposure", money(s.exposure), "neutral");
    setText("mOpen", (s.open_positions || 0) + " open position" + (s.open_positions === 1 ? "" : "s"));
    setText("mCosts", money(s.fees + s.funding), "neutral");
    setText("mCostsSub", "fees " + money(s.fees, 4) + " · funding " + money(s.funding, 4));
    setText("mFeedAge", age(s.ws_last_message_ms), s.feed_ok ? "neutral" : "neg");
    setText("mFeedDetail", s.feed_ok ? "live public feed" : (s.feed_detail || "degraded"));
    setText("deckTimestamp", "snapshot " + timeOnly(s.generated_at_ms));
    setText("equityCaption", "updated " + timeOnly(s.generated_at_ms));

    const feed = $("chipFeed");
    if (feed) {
      feed.className = "chip " + (s.feed_ok ? "ok" : "bad");
      feed.innerHTML = '<span class="dot"></span><span class="chip-text">feed ' + esc(s.ws_state || "?") + "</span>";
    }
    const engine = $("chipEngine");
    if (engine) {
      const blocked = s.paused || s.emergency_stop || s.risk.daily_lockout || s.risk.drawdown_lockout;
      engine.className = "chip " + (s.emergency_stop ? "bad" : blocked ? "warn" : "ok");
      engine.innerHTML = '<span class="dot"></span><span class="chip-text">' +
        esc(s.emergency_stop ? "emergency" : s.paused ? "paused" : "running") + "</span>";
    }
    const ws = $("chipWs");
    if (ws) {
      ws.className = "chip " + (s.feed_ok ? "ok" : "warn");
      ws.innerHTML = '<span class="dot"></span><span class="chip-text">ws ' + esc(s.ws_state || "?") +
        (s.ws_reconnects ? " · " + s.ws_reconnects + " rc" : "") + "</span>";
    }
    setText("chipClock", "clock " + (s.clock_offset_ms >= 0 ? "+" : "") + s.clock_offset_ms + "ms");
    setText("chipUptime", "up " + duration(s.system.uptime_seconds));
    setText("footerVersion", (s.version || "underhaven") + " · " + (s.system.git_commit || "unknown"));

    // aggregate equity
    const data = mergedEquity(s.strategies || []);
    updateChart("aggregate", "equityChart", equityOptions(225), data);

    // gates
    const blocked = (s.risk && s.risk.blocked_reasons) || [];
    const gates = [
      kvRow("Engine", s.emergency_stop ? "EMERGENCY STOP" : s.paused ? "PAUSED" : "running",
        s.emergency_stop ? "neg" : s.paused ? "warn-text" : "pos"),
      kvRow("Daily loss lockout", s.risk.daily_lockout ? "ACTIVE" : "clear", s.risk.daily_lockout ? "neg" : ""),
      kvRow("Drawdown lockout", s.risk.drawdown_lockout ? "ACTIVE" : "clear", s.risk.drawdown_lockout ? "neg" : ""),
      kvRow("Cooldown", s.risk.cooldown_until_ms ? clock(s.risk.cooldown_until_ms) : "none"),
      kvRow("Max positions", String(s.risk.max_open_positions) + " · open " + s.open_positions),
      kvRow("Public REST", s.bybit.public_rest_ok ? "ok" : "not confirmed", s.bybit.public_rest_ok ? "pos" : "warn-text"),
      kvRow("Read-only key", s.bybit.read_only_verified ? "verified" : "not configured",
        s.bybit.read_only_verified ? "pos" : "dim"),
      blocked.length
        ? '<span class="full">Blocked: ' + esc(blocked.join(" · ")) + "</span>"
        : '<span class="full">No active engine block.</span>',
    ];
    const gateList = $("gateList");
    if (gateList) gateList.innerHTML = gates.join("");
  }

  function renderAccount(s) {
    const body = $("walletBody");
    if (!body) return;
    if (!s.strategies || !s.strategies.length) {
      body.innerHTML = '<tr><td colspan="10" class="empty">No wallets.</td></tr>';
      return;
    }
    body.innerHTML = s.strategies.map((strategy) => {
      return "<tr>" +
        "<td>" + esc(strategy.name) + '<span class="dim"> · ' + esc(strategy.timeframe) + "</span></td>" +
        "<td>" + money(strategy.initial_equity) + "</td>" +
        "<td>" + money(strategy.cash) + "</td>" +
        "<td>" + money(strategy.equity) + "</td>" +
        '<td class="' + signClass(strategy.realized) + '">' + signedMoney(strategy.realized) + "</td>" +
        '<td class="' + signClass(strategy.unrealized) + '">' + signedMoney(strategy.unrealized) + "</td>" +
        "<td>" + money(strategy.fees, 4) + "</td>" +
        "<td>" + money(strategy.funding, 4) + "</td>" +
        '<td class="' + signClass(strategy.daily_pnl) + '">' + signedMoney(strategy.daily_pnl) + "</td>" +
        "<td>" + strategy.trades + "</td>" +
        "</tr>";
    }).join("");
  }

  function strategyCard(strategy) {
    const id = strategy.strategy;
    return '<article class="strategy-card" id="card-' + esc(id) + '">' +
      '<div class="strategy-head"><div><h3>' + esc(strategy.name) + '</h3>' +
      '<span class="tf">' + esc(strategy.timeframe) + " · " + esc(id) + "</span></div>" +
      '<span class="badge" id="badge-' + esc(id) + '">—</span></div>' +
      '<div class="strategy-equity" id="equity-' + esc(id) + '">—</div>' +
      '<div class="strategy-sub" id="sub-' + esc(id) + '">—</div>' +
      '<div class="spark" id="spark-' + esc(id) + '"></div>' +
      '<div class="strategy-stats">' +
        '<div><span>Trades</span><b id="trades-' + esc(id) + '">—</b></div>' +
        '<div><span>Win</span><b id="win-' + esc(id) + '">—</b></div>' +
        '<div><span>PF</span><b id="pf-' + esc(id) + '">—</b></div>' +
        '<div><span>Exp.</span><b id="exp-' + esc(id) + '">—</b></div>' +
        '<div><span>Max DD</span><b id="dd-' + esc(id) + '">—</b></div>' +
        '<div><span>Fees</span><b id="fees-' + esc(id) + '">—</b></div>' +
        '<div><span>Funding</span><b id="fund-' + esc(id) + '">—</b></div>' +
        '<div><span>Open</span><b id="open-' + esc(id) + '">—</b></div>' +
      '</div>' +
      '<div class="validation"><span class="label">Out-of-sample</span>' +
      '<div class="oos" id="oos-' + esc(id) + '">NOT YET VALIDATED</div>' +
      '<span class="label">Forward test</span>' +
      '<div class="forward" id="forward-' + esc(id) + '">NOT YET VALIDATED</div></div>' +
      '</article>';
  }

  function renderStrategies(s) {
    const grid = $("strategyGrid");
    if (!grid || !s.strategies) return;
    if (!grid.querySelector(".strategy-card")) {
      grid.innerHTML = s.strategies.map(strategyCard).join("");
    }
    s.strategies.forEach((strategy) => {
      const id = strategy.strategy;
      const card = $("card-" + id);
      if (!card) return;
      card.classList.toggle("disabled", !strategy.enabled);
      const badge = $("badge-" + id);
      if (badge) {
        if (!strategy.enabled) {
          badge.className = "badge";
          badge.textContent = "disabled";
        } else if (strategy.open_positions > 0) {
          badge.className = "badge live";
          badge.textContent = strategy.open_positions + " open";
        } else {
          badge.className = "badge";
          badge.textContent = "flat";
        }
      }
      setText("equity-" + id, money(strategy.equity));
      setText("sub-" + id, "day " + signedMoney(strategy.daily_pnl) + " · realized " + signedMoney(strategy.realized));
      const sub = $("sub-" + id);
      if (sub) sub.className = "strategy-sub " + signClass(strategy.daily_pnl);
      setText("trades-" + id, strategy.trades);
      setText("win-" + id, rawPct(strategy.win_rate, 1));
      setText("pf-" + id, ratio(strategy.profit_factor));
      setText("exp-" + id, signedMoney(strategy.expectancy, 3));
      const exp = $("exp-" + id);
      if (exp) exp.className = signClass(strategy.expectancy);
      setText("dd-" + id, rawPct(strategy.max_drawdown_pct));
      setText("fees-" + id, money(strategy.fees, 3));
      setText("fund-" + id, money(strategy.funding, 3));
      setText("open-" + id, strategy.open_positions);
      setText("oos-" + id, strategy.oos_status);
      setText("forward-" + id, strategy.forward_status);
      const oos = $("oos-" + id);
      if (oos) {
        oos.className = "oos " + (/^POSITIVE/.test(strategy.oos_status) ? "pos"
          : /NEGATIVE|INSUFFICIENT|NOT YET/.test(strategy.oos_status) ? "warn-text" : "");
      }
      const forward = $("forward-" + id);
      if (forward) {
        forward.className = "forward " + (/^FORWARD|^EXPERIMENTAL/.test(strategy.forward_status) ? "pos" : "warn-text");
      }
      const colors = { trend: "#39e2c3", mean_reversion: "#5aa8ff", breakout: "#f5b84b" };
      updateChart("spark-" + id, "spark-" + id, sparkOptions(colors[id] || "#39e2c3"),
        curveData(strategy.equity_curve));
    });
  }

  function renderScanner(s) {
    const body = $("scanBody");
    if (!body) return;
    const rows = (s.scanner || []).filter((row) => {
      if (!state.scanFilter) return true;
      return String(row.symbol).toLowerCase().includes(state.scanFilter.toLowerCase());
    });
    const eligible = (s.scanner || []).filter((row) => row.eligible).length;
    const selected = (s.scanner || []).filter((row) => row.selected).length;
    setText("scanSummary", eligible + " eligible · " + selected + " selected · " +
      (s.scanner || []).length + " evaluated");
    if (!rows.length) {
      body.innerHTML = '<tr><td colspan="9" class="empty">No matching scanner rows.</td></tr>';
      return;
    }
    body.innerHTML = rows.map((row) => {
      const status = row.eligible
        ? (row.selected ? tag("selected", "sel") : tag("eligible"))
        : tag(row.reject_reason || "rejected", "rej");
      return "<tr>" +
        "<td><b>" + esc(row.symbol) + "</b></td>" +
        "<td>" + price(row.last_price) + "</td>" +
        '<td class="' + (row.spread_bps > 8 ? "neg" : "") + '">' + ratio(row.spread_bps) + " bps</td>" +
        "<td>" + money(row.depth_bid_usd, 0) + "</td>" +
        "<td>" + money(row.depth_ask_usd, 0) + "</td>" +
        "<td>" + money(row.turnover_24h, 0) + "</td>" +
        "<td>" + rawPct(row.volatility_pct) + "</td>" +
        "<td>" + ratio(row.liquidity_score, 1) + "</td>" +
        "<td>" + status + "</td>" +
        "</tr>";
    }).join("");
  }

  function renderPositions(s) {
    const body = $("positionBody");
    if (!body) return;
    const positions = s.positions || [];
    setText("positionSummary", positions.length + " open · notional " + money(s.exposure));
    if (!positions.length) {
      body.innerHTML = '<tr><td colspan="12" class="empty">No open paper positions.</td></tr>';
      return;
    }
    body.innerHTML = positions.map((position) => {
      return "<tr>" +
        "<td>" + esc(position.strategy) + "</td>" +
        "<td><b>" + esc(position.symbol) + "</b></td>" +
        "<td>" + tag(position.direction, position.direction) + "</td>" +
        "<td>" + qty(position.qty) + "</td>" +
        "<td>" + price(position.entry_price) + "</td>" +
        "<td>" + price(position.mark_price) + "</td>" +
        '<td class="dim">' + price(position.stop) + "</td>" +
        '<td class="dim">' + price(position.target) + "</td>" +
        '<td class="' + signClass(position.unrealized_pnl) + '">' + signedMoney(position.unrealized_pnl, 4) + "</td>" +
        "<td>" + money(position.notional) + "</td>" +
        "<td>" + age(position.entry_time_ms) + "</td>" +
        '<td><button class="btn danger small" type="button" data-position="' + esc(position.id) + '">Flatten</button></td>' +
        "</tr>";
    }).join("");
  }

  function renderTrades(s) {
    const body = $("tradeBody");
    if (!body) return;
    const trades = s.trades || [];
    if (!trades.length) {
      body.innerHTML = '<tr><td colspan="11" class="empty">No closed paper trades yet.</td></tr>';
      return;
    }
    body.innerHTML = trades.map((trade) => {
      return "<tr>" +
        "<td>" + clock(trade.exit_time_ms) + "</td>" +
        "<td>" + esc(trade.strategy) + "</td>" +
        "<td>" + esc(trade.symbol) + "</td>" +
        "<td>" + tag(trade.direction, trade.direction) + "</td>" +
        "<td>" + price(trade.entry_price) + "</td>" +
        "<td>" + price(trade.exit_price) + "</td>" +
        '<td class="' + signClass(trade.net_pnl) + '">' + signedMoney(trade.net_pnl, 4) + "</td>" +
        "<td>" + money(trade.fees, 4) + "</td>" +
        "<td>" + money(trade.funding, 4) + "</td>" +
        "<td>" + esc(trade.close_reason) + "</td>" +
        "<td>" + trade.bars_held + "</td>" +
        "</tr>";
    }).join("");
  }

  function renderRisk(s) {
    const list = $("riskList");
    if (!list) return;
    const risk = s.risk || {};
    setText("riskState", risk.kill_switch ? "KILL SWITCH" : risk.daily_lockout || risk.drawdown_lockout ? "LOCKED" : "armed",
      risk.kill_switch || risk.daily_lockout || risk.drawdown_lockout ? "neg" : "pos");
    const rows = [
      kvRow("Risk per trade", rawPct(risk.risk_per_trade_pct)),
      kvRow("Daily loss limit", rawPct(risk.daily_loss_limit_pct)),
      kvRow("Max drawdown limit", rawPct(risk.drawdown_limit_pct)),
      kvRow("Max open positions", risk.max_open_positions),
      kvRow("Max total notional", rawPct(risk.max_total_notional_pct)),
      kvRow("Max correlated notional", rawPct(risk.max_correlated_notional_pct)),
      kvRow("Max consecutive losses", risk.max_consecutive_losses),
      kvRow("Current consecutive losses", risk.consecutive_losses),
      kvRow("Daily P&L", signedMoney(risk.daily_pnl), signClass(risk.daily_pnl)),
      kvRow("Daily lockout", risk.daily_lockout ? "ACTIVE" : "clear", risk.daily_lockout ? "neg" : ""),
      kvRow("Drawdown lockout", risk.drawdown_lockout ? "ACTIVE" : "clear", risk.drawdown_lockout ? "neg" : ""),
      kvRow("Cooldown until", risk.cooldown_until_ms ? clock(risk.cooldown_until_ms) : "none"),
    ];
    if (risk.blocked_reasons && risk.blocked_reasons.length) {
      rows.push('<span class="full warn-text">' + esc(risk.blocked_reasons.join(" · ")) + "</span>");
    }
    list.innerHTML = rows.join("");
    document.querySelectorAll("[data-action]").forEach((button) => {
      if (button.dataset.action === "resume") button.disabled = !s.paused;
      if (button.dataset.action === "clear_emergency") button.disabled = !s.emergency_stop;
      if (button.dataset.action === "pause") button.disabled = !!s.paused || !!s.emergency_stop;
    });
  }

  function renderSystem(s) {
    const list = $("systemList");
    if (!list) return;
    const system = s.system || {};
    const bybit = s.bybit || {};
    const rows = [
      kvRow("Build", system.build_version || s.version),
      kvRow("Commit", system.git_commit || "unknown"),
      kvRow("Uptime", duration(system.uptime_seconds)),
      kvRow("Database", system.db_ok ? "ok" : "error", system.db_ok ? "pos" : "neg"),
      kvRow("WAL", system.db_wal ? "enabled" : "disabled"),
      kvRow("DB size", bytes(system.db_size_bytes)),
      kvRow("DB path", system.db_path),
      kvRow("Last backup", system.last_backup_ms ? clock(system.last_backup_ms) : "never"),
      kvRow("API errors", system.api_errors),
      kvRow("Rate-limit hits", system.rate_limit_hits),
      kvRow("REST latency guard", "100 ms pacing"),
      kvRow("Bybit REST", bybit.public_rest_ok ? "ok" : "not confirmed", bybit.public_rest_ok ? "pos" : "warn-text"),
      kvRow("Bybit WS", bybit.public_ws_ok ? "ok" : "not confirmed", bybit.public_ws_ok ? "pos" : "warn-text"),
      kvRow("Private key link", bybit.private_connected ? "connected" : "not configured",
        bybit.private_connected ? "pos" : "dim"),
      kvRow("Fee source", bybit.fee_source),
      bybit.fee_source && bybit.taker_fee != null
        ? kvRow("Fees", "taker " + ratio(bybit.taker_fee, 4) + " · maker " + ratio(bybit.maker_fee, 4))
        : "",
      bybit.error ? '<span class="full warn-text">' + esc(bybit.error) + "</span>" : "",
    ];
    list.innerHTML = rows.join("");
  }

  function renderSecurity(s) {
    const list = $("securityList");
    if (list) {
      const bybit = s.bybit || {};
      list.innerHTML = [
        kvRow("Transport cookies", s.feed_ok ? "Secure policy: " + (state.session && state.session.secure_cookies ? "Secure" : "local HTTP") : "—"),
        kvRow("CSRF", "per-session token enforced"),
        kvRow("Password hash", "Argon2id"),
        kvRow("Session store", "SHA-256 token hash only"),
        kvRow("Read-only key", bybit.read_only_verified ? "verified" : "not configured",
          bybit.read_only_verified ? "pos" : "dim"),
        kvRow("Key UID", bybit.uid || "—"),
        kvRow("Key note", bybit.key_note || "—"),
        bybit.permissions_warning
          ? '<span class="full warn-text">' + esc(bybit.permissions_warning) + "</span>"
          : '<span class="full">Secrets are never returned to this browser.</span>',
      ].join("");
    }
    const credential = $("credentialState");
    if (credential) {
      const bybit = s.bybit || {};
      credential.innerHTML = [
        kvRow("Verification", bybit.read_only_verified ? "read-only confirmed" : "not configured",
          bybit.read_only_verified ? "pos" : "dim"),
        kvRow("Last checked", bybit.last_checked_ms ? clock(bybit.last_checked_ms) : "—"),
        kvRow("UID", bybit.uid || "—"),
        bybit.error ? '<span class="full warn-text">' + esc(bybit.error) + "</span>" : "",
      ].join("");
    }
  }

  function renderAudit(s) {
    const list = $("auditList");
    if (!list) return;
    const events = (s.system && s.system.audit_events) || [];
    if (!events.length) {
      list.innerHTML = '<div class="empty-panel">No audit events yet.</div>';
      return;
    }
    list.innerHTML = events.map((event) => {
      return '<div class="audit-item">' +
        '<span class="ts">' + clock(event.ts_ms) + "</span>" +
        "<div><span class=\"type\">" + esc(event.event_type) + "</span> " +
        '<span class="detail">' + esc(event.event_type === "auth.login" ? event.actor : event.detail) + "</span></div>" +
        "</div>";
    }).join("");
  }

  function renderAlerts(s) {
    const strip = $("alertStrip");
    if (!strip) return;
    const messages = [];
    if (s.emergency_stop) messages.push("EMERGENCY STOP: " + (s.emergency_reason || "active"));
    if (s.risk.daily_lockout) messages.push("Daily loss lockout active — new entries blocked.");
    if (s.risk.drawdown_lockout) messages.push("Drawdown lockout active — new entries blocked.");
    if (!s.feed_ok) messages.push("Market feed unhealthy: " + (s.feed_detail || s.ws_state));
    if (s.paused) messages.push("Entries paused: " + (s.pause_reason || "operator pause"));
    if (messages.length) {
      strip.textContent = messages.join("  ·  ");
      strip.hidden = false;
    } else {
      strip.hidden = true;
    }
  }

  // ---------------------------------------------------------------------
  // Configuration and backtests
  // ---------------------------------------------------------------------
  async function loadConfig() {
    try {
      const config = await api("/api/config");
      state.config = config;
      const view = $("configView");
      if (view) view.textContent = JSON.stringify(config, null, 2);
    } catch (error) {
      const view = $("configView");
      if (view) view.textContent = "configuration unavailable: " + error.message;
    }
  }

  function statsCells(stats) {
    return [
      stats.trades,
      signedMoney(stats.net_pnl, 4),
      rawPct(stats.win_rate, 1),
      ratio(stats.profit_factor),
      rawPct(stats.max_drawdown_pct),
    ];
  }

  function renderReport(report) {
    state.report = report;
    const result = report.result || {};
    const stats = result.stats || {};
    setText("btStatus", (result.strategy || "") + " · " + (result.symbol || "") + " · " + (result.interval || ""));
    $("btEmpty").hidden = true;
    $("btReport").hidden = false;

    const metrics = [
      ["Net P&L", signedMoney(stats.net_pnl, 4), signClass(stats.net_pnl)],
      ["Expectancy", signedMoney(stats.expectancy, 4), signClass(stats.expectancy)],
      ["Win rate", rawPct(stats.win_rate, 1), ""],
      ["Profit factor", ratio(stats.profit_factor), ""],
      ["Max drawdown", rawPct(stats.max_drawdown_pct), ""],
      ["Trades", String(stats.trades), ""],
      ["Fees", money(stats.fees, 4), ""],
      ["Funding", money(stats.funding, 4), ""],
      ["Sharpe", ratio(stats.sharpe), ""],
      ["Cost / gross", stats.cost_to_gross == null ? "—" : (stats.cost_to_gross * 100).toFixed(1) + "%", ""],
    ];
    $("btMetrics").innerHTML = metrics.map(([label, value, cls]) => {
      return '<article class="metric"><span class="metric-label">' + esc(label) +
        '</span><strong class="' + cls + '">' + esc(value) + "</strong></article>";
    }).join("");

    updateChart("bt", "btChart", equityOptions(160), curveData(result.equity_curve));

    const warnings = []
      .concat(Array.isArray(report.warnings) ? report.warnings : [])
      .concat(Array.isArray(result.warnings) ? result.warnings : []);
    $("btWarnings").innerHTML = warnings.map((warning) => "<div>" + esc(warning) + "</div>").join("");

    const segments = report.segments || [];
    $("btSegments").innerHTML = segments.length ? segments.map((segment) => {
      let status;
      if (segment.name === "out_of_sample" && segment.trades < 30) status = "INSUFFICIENT DATA";
      else if (segment.stats.net_pnl > 0) status = "POSITIVE";
      else status = "NEGATIVE";
      return "<tr><td>" + esc(segment.name) + "</td><td>" + segment.trades + "</td>" +
        '<td class="' + signClass(segment.stats.net_pnl) + '">' + signedMoney(segment.stats.net_pnl, 4) + "</td>" +
        "<td>" + rawPct(segment.stats.win_rate, 1) + "</td>" +
        "<td>" + ratio(segment.stats.profit_factor) + "</td>" +
        "<td>" + rawPct(segment.stats.max_drawdown_pct) + "</td>" +
        "<td>" + tag(status, status === "POSITIVE" ? "sel" : "rej") + "</td></tr>";
    }).join("") : '<tr><td colspan="7" class="empty">No segments.</td></tr>';

    const folds = report.walk_forward || [];
    $("btWalk").innerHTML = folds.length ? folds.map((fold) => {
      return "<tr><td>" + fold.fold + "</td><td>" + clock(fold.test_start_ms) + " → " + clock(fold.test_end_ms) + "</td>" +
        "<td>" + fold.test_trades + "</td>" +
        '<td class="' + signClass(fold.test_net_pnl) + '">' + (fold.test_net_pnl == null ? "—" : signedMoney(fold.test_net_pnl, 4)) + "</td>" +
        "<td>" + rawPct(fold.test_max_drawdown_pct) + "</td>" +
        "<td>" + esc(fold.note) + "</td></tr>";
    }).join("") : '<tr><td colspan="6" class="empty">NOT RUN — insufficient history or no variants.</td></tr>';

    const sensitivity = report.sensitivity || [];
    $("btSensitivity").innerHTML = sensitivity.length ? sensitivity.map((item) => {
      return "<tr><td>" + esc(item.name) + "</td><td>" + item.trades + "</td>" +
        '<td class="' + signClass(item.net_pnl) + '">' + signedMoney(item.net_pnl, 4) + "</td>" +
        "<td>" + signedMoney(item.expectancy, 4) + "</td>" +
        "<td>" + rawPct(item.max_drawdown_pct) + "</td>" +
        "<td>" + ratio(item.profit_factor) + "</td>" +
        "<td>" + tag(item.passed ? "PASS" : "FAIL", item.passed ? "sel" : "rej") + "</td></tr>";
    }).join("") : '<tr><td colspan="7" class="empty">No sensitivity cases.</td></tr>';

    $("btCsv").href = "/api/backtests/" + encodeURIComponent(result.run_id) + "/trades.csv";
    $("btJson").href = "/api/backtests/" + encodeURIComponent(result.run_id);
    $("btJson").target = "_blank";
    loadBacktests();
  }

  async function runBacktest(event) {
    event.preventDefault();
    const button = $("btRun");
    button.disabled = true;
    button.textContent = "Running…";
    setText("btStatus", "RUNNING");
    try {
      const report = await api("/api/backtest", {
        method: "POST",
        body: {
          strategy: $("btStrategy").value,
          symbol: $("btSymbol").value.trim().toUpperCase(),
          interval: $("btInterval").value,
          candles: Number($("btCandles").value || 3000),
        },
      });
      renderReport(report);
      toast("Backtest complete: " + report.result.trades.length + " trades.");
    } catch (error) {
      setText("btStatus", "FAILED");
      toast(error.message, true);
    } finally {
      button.disabled = false;
      button.textContent = "Run backtest";
    }
  }

  async function importHistory(event) {
    event.preventDefault();
    const button = $("histRun");
    button.disabled = true;
    button.textContent = "Importing…";
    try {
      const result = await api("/api/history/import", {
        method: "POST",
        body: {
          symbol: $("histSymbol").value.trim().toUpperCase(),
          interval: $("histInterval").value,
          days: Number($("histDays").value || 90),
        },
      });
      toast("Imported " + result.candles + " candles for " + result.symbol + " " + result.interval + ".");
    } catch (error) {
      toast(error.message, true);
    } finally {
      button.disabled = false;
      button.textContent = "Import from Bybit";
    }
  }

  async function loadBacktests() {
    try {
      const rows = await api("/api/backtests");
      const body = $("storedBody");
      setText("storedSummary", rows.length + " stored run" + (rows.length === 1 ? "" : "s"));
      if (!body) return;
      body.innerHTML = rows.length ? rows.map((row) => {
        return "<tr><td>" + clock(row.created_at_ms) + "</td><td>" + esc(row.strategy) + "</td>" +
          "<td>" + esc(row.symbol) + "</td><td>" + (row.trades == null ? "—" : row.trades) + "</td>" +
          '<td class="' + signClass(row.net_pnl) + '">' + (row.net_pnl == null ? "—" : signedMoney(row.net_pnl, 4)) + "</td>" +
          "<td>" + (row.max_drawdown_pct == null ? "—" : rawPct(row.max_drawdown_pct)) + "</td>" +
          "<td>" + esc(row.oos_status) + "</td>" +
          '<td><button class="btn ghost small" type="button" data-run="' + esc(row.run_id) + '">Load</button></td></tr>';
      }).join("") : '<tr><td colspan="8" class="empty">No stored backtests.</td></tr>';
    } catch (error) {
      setText("storedSummary", "unavailable");
    }
  }

  async function loadStoredReport(runId) {
    try {
      const report = await api("/api/backtests/" + encodeURIComponent(runId));
      renderReport(report);
      document.getElementById("section-f").scrollIntoView({ behavior: "smooth", block: "start" });
      toast("Loaded backtest " + runId.slice(0, 8) + "…");
    } catch (error) {
      toast(error.message, true);
    }
  }

  // ---------------------------------------------------------------------
  // Controls
  // ---------------------------------------------------------------------
  async function control(action) {
    const reason = ($("controlReason") && $("controlReason").value.trim()) || "operator action";
    if (action === "emergency_stop" && !window.confirm(
      "Emergency stop flattens every open paper position and blocks new entries. Continue?"
    )) return;
    try {
      const result = await api("/api/control", {
        method: "POST",
        body: { action, reason },
      });
      if (action === "emergency_stop") {
        toast("Emergency stop active; flattened " + (result.closed_positions || 0) + " position(s).", true);
      } else {
        toast("Control applied: " + action.replace(/_/g, " ") + ".");
      }
      const snapshot = await api("/api/dashboard");
      render(snapshot);
    } catch (error) {
      toast(error.message, true);
    }
  }

  async function flatten(positionId) {
    if (!window.confirm("Flatten this paper position at the current mark?")) return;
    try {
      await api("/api/control", {
        method: "POST",
        body: { action: "close_position", position_id: positionId, reason: "manual flatten" },
      });
      toast("Position flattened.");
      const snapshot = await api("/api/dashboard");
      render(snapshot);
    } catch (error) {
      toast(error.message, true);
    }
  }

  async function changePassword(event) {
    event.preventDefault();
    try {
      await api("/api/password", {
        method: "POST",
        body: {
          current_password: $("pwCurrent").value,
          new_password: $("pwNext").value,
        },
      });
      $("pwCurrent").value = "";
      $("pwNext").value = "";
      state.session = null;
      showLogin("Password changed. All sessions revoked; sign in again.");
    } catch (error) {
      toast(error.message, true);
    }
  }

  async function storeCredential(event) {
    event.preventDefault();
    try {
      const result = await api("/api/credentials", {
        method: "POST",
        body: {
          name: $("credName").value.trim() || "primary",
          api_key: $("credKey").value.trim(),
          api_secret: $("credSecret").value,
          note: $("credNote").value.trim() || null,
        },
      });
      $("credKey").value = "";
      $("credSecret").value = "";
      toast(result.message || "Read-only key stored.");
      const snapshot = await api("/api/dashboard");
      render(snapshot);
    } catch (error) {
      toast(error.message, true);
    }
  }

  async function deleteCredential() {
    if (!window.confirm("Delete the stored read-only Bybit key?")) return;
    try {
      await api("/api/credentials", { method: "DELETE" });
      toast("Stored key deleted.");
      const snapshot = await api("/api/dashboard");
      render(snapshot);
    } catch (error) {
      toast(error.message, true);
    }
  }

  // ---------------------------------------------------------------------
  // Wire-up
  // ---------------------------------------------------------------------
  function wire() {
    $("loginForm").addEventListener("submit", login);
    $("logoutButton").addEventListener("click", logout);
    $("backtestForm").addEventListener("submit", runBacktest);
    $("historyForm").addEventListener("submit", importHistory);
    $("passwordForm").addEventListener("submit", changePassword);
    $("credentialForm").addEventListener("submit", storeCredential);
    $("credDelete").addEventListener("click", deleteCredential);

    document.querySelectorAll("[data-action]").forEach((button) => {
      button.addEventListener("click", () => control(button.dataset.action));
    });

    $("positionBody").addEventListener("click", (event) => {
      const button = event.target.closest("[data-position]");
      if (button) flatten(button.dataset.position);
    });
    $("storedBody").addEventListener("click", (event) => {
      const button = event.target.closest("[data-run]");
      if (button) loadStoredReport(button.dataset.run);
    });
    $("scanFilter").addEventListener("input", (event) => {
      state.scanFilter = event.target.value.trim();
      if (state.snapshot) renderScanner(state.snapshot);
    });

    window.addEventListener("online", () => {
      if (state.session) startStream();
    });
  }

  document.addEventListener("DOMContentLoaded", () => {
    wire();
    boot();
    setInterval(async () => {
      if (!state.session) return;
      try {
        const session = await api("/api/session");
        if (!session.authenticated) showLogin("Session expired. Sign in again.");
      } catch (error) {
        // Keep the current view; the API helper handles hard 401s.
      }
    }, 45000);
  });
})();
