"""Backend risk controls. The dashboard only edits the limits; every check runs here, before any order, in paper and live mode alike.
If a check cannot be evaluated the trade is rejected."""
import time
from decimal import Decimal
from . import store
from .calc import d, ZERO
from .. import db


class RiskManager:
    def __init__(self, settings_fn, now=time.time):
        self.S, self.now = settings_fn, now

    # ---- kill switch (persisted: a restart never silently re-enables trading)
    def killed(self): return db.get_setting('b_kill', '0') == '1'
    def kill_reason(self): return db.get_setting('b_kill_reason', '')

    def kill(self, reason):
        db.set_setting('b_kill', '1'); db.set_setting('b_kill_reason', reason[:200]); store.event('CRITICAL', 'KILL_SWITCH', reason)

    def clear_kill(self):
        db.set_setting('b_kill', '0'); db.set_setting('b_kill_reason', ''); store.event('INFO', 'KILL_CLEARED', 'Kill switch cleared by the operator.')

    def day_start(self):
        t = time.gmtime(self.now()); return self.now() - (t.tm_hour * 3600 + t.tm_min * 60 + t.tm_sec)

    def orders_last_minute(self):
        with db.connect() as c: return c.execute('SELECT COUNT(*) FROM b_orders WHERE ts>=?', (self.now() - 60,)).fetchone()[0]

    def check_entry(self, sig, mode, live_enabled=False):
        """-> (ok, [reasons]). Never raises; an internal failure is itself a rejection."""
        S = self.S(); r = []
        try:
            if self.killed(): r.append(f'KILL_SWITCH_ACTIVE ({self.kill_reason() or "no reason recorded"})')
            if mode == 'LIVE':
                if not live_enabled: r.append('LIVE_MODE_NOT_ENABLED')
                if sig.strategy not in S.live_strategies: r.append(f'STRATEGY_NOT_PERMITTED_LIVE ({sig.strategy})')
                if sig.research: r.append('UNVERIFIED_PAIR_NEVER_LIVE')
            costs = {}
            for leg in sig.legs: costs[leg.market.venue] = costs.get(leg.market.venue, ZERO) + leg.qty * leg.vwap + leg.fee
            total = sum(costs.values(), ZERO)
            if total > d(S.max_capital_per_trade): r.append(f'CAPITAL_PER_TRADE_LIMIT (${total:.2f} > ${S.max_capital_per_trade})')
            for leg in sig.legs:
                lim = d(S.max_order_notional)
                if mode == 'LIVE': lim = min(lim, d(getattr(S, 'live_max_order_' + leg.market.venue)))
                if leg.qty * leg.vwap > lim: r.append(f'ORDER_SIZE_LIMIT ({leg.market.venue} ${leg.qty * leg.vwap:.2f} > ${lim})')
            exp = store.exposure(mode); tot = sum(exp.values(), ZERO)
            if tot + total > d(S.max_total_exposure): r.append(f'TOTAL_EXPOSURE_LIMIT (${tot + total:.2f} > ${S.max_total_exposure})')
            for v, c in costs.items():
                lim = d(S.max_exposure_platform)
                if mode == 'LIVE': lim = min(lim, d(getattr(S, 'live_max_exposure_' + v)))
                if exp.get(v, ZERO) + c > lim: r.append(f'PLATFORM_EXPOSURE_LIMIT ({v} ${exp.get(v, ZERO) + c:.2f} > ${lim})')
            groups = {p['group_id'] for p in store.open_positions(mode)}
            if len(groups) >= S.max_open_positions: r.append(f'MAX_OPEN_POSITIONS ({len(groups)})')
            if sig.cross and max(costs.values()) > d(S.max_unmatched): r.append(f'UNMATCHED_LEG_EXPOSURE_LIMIT (${max(costs.values()):.2f} > ${S.max_unmatched})')
            ds = self.day_start()
            if store.realized_since(mode, ds) <= -d(S.max_daily_loss): r.append('DAILY_LOSS_LIMIT'); self.kill('Daily loss limit reached')
            if mode == 'LIVE':
                for v in ('polymarket', 'kalshi'):
                    if store.realized_since(mode, ds, v) <= -d(getattr(S, 'live_daily_loss_' + v)): r.append(f'LIVE_DAILY_LOSS_LIMIT ({v})')
            if S.max_drawdown and store.max_drawdown(mode) >= S.max_drawdown: r.append('MAX_DRAWDOWN'); self.kill('Maximum drawdown reached')
            if sig.edge < d(S.min_net_edge): r.append(f'EDGE_BELOW_MINIMUM ({float(sig.edge):+.4f})')
            if self.orders_last_minute() + len(sig.legs) > S.orders_per_minute: r.append('ORDER_RATE_LIMIT')
            if mode == 'PAPER':
                for v, c in costs.items():
                    if store.cash('PAPER', v) < c: r.append(f'INSUFFICIENT_PAPER_BALANCE ({v} cash ${store.cash("PAPER", v):.2f} < ${c:.2f})')
        except Exception as exc:
            r.append(f'RISK_CHECK_FAILED ({type(exc).__name__}: {exc})')
        return (not r), r
