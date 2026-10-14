"""Read-only view models for the dashboard. Everything shown is read from the live service state or from recorded rows; nothing is invented, and an
empty result is reported as empty (with the reasons) rather than filled in."""
import time
from decimal import Decimal
from .. import db
from . import store
from .calc import Position, ZERO, d, fee_for_fill, walk
from .strategies import fee_model, time_ok

STRATS = [('arb', 'A · Cross-platform arbitrage', 'Polymarket UP + Kalshi DOWN (or the reverse) on a verified pair'),
          ('momentum', 'B · BTC momentum', 'Model probability from BTC distance, volatility, drift and time left vs the executable ask'),
          ('meanrev', 'C · Mean reversion / overreaction', 'Fade a market jump that BTC itself did not make'),
          ('bundle', 'G · Same-platform bundle (retained)', 'Polymarket UP + DOWN asks for less than $1 after fees')]
NOTES = {'book': 'D · Order-book quality and imbalance: a filter on every strategy above (spread, depth, freshness, valid book).',
         'time': 'E · Expiration-aware entry window: from the contract duration, settlement, latency and the time needed to exit.'}


def _money(x): return float(x) if x is not None else None


def signal_stats(hours=24):
    cut = time.time() - hours * 3600
    with db.connect() as c:
        rows = [dict(r) for r in c.execute('SELECT strategy, accepted, reason, duration FROM b_signals WHERE ts>=? AND mode=\'PAPER\'', (cut,))]
    out = {}
    for r in rows:
        s = out.setdefault(r['strategy'], {'accepted': 0, 'detected_rejected': 0, 'pre_rejected': 0, 'reasons': {}})
        if r['accepted']: s['accepted'] += 1
        elif r['reason'].startswith(('PAPER:', 'DUPLICATE_EXPOSURE_WITH', 'EXECUTION_LIMIT')): s['detected_rejected'] += 1
        else: s['pre_rejected'] += 1
        if not r['accepted']:
            key = r['reason'].split(' (')[0][:80]; s['reasons'][key] = s['reasons'].get(key, 0) + 1
    return out


def top_reasons(stats, n=6):
    tot = {}
    for s in stats.values():
        for k, v in s['reasons'].items(): tot[k] = tot.get(k, 0) + v
    return sorted(tot.items(), key=lambda kv: -kv[1])[:n]


def mark_position(svc, p):
    """Executable exit value of an open position from the latest REAL bids (None when there is no fresh book)."""
    mk = svc.lookup(p['venue'], p['market_id'])
    ob = svc.obs.get(mk.key) if mk else None
    if not ob or ob.age(svc.now()) > svc.settings().max_quote_age * 3: return None, None
    pos = store.pos_from_row(p)
    fill = walk(ob.book(p['side']).bids, pos.qty, side='sell', lot=Decimal(1) if p['venue'] == 'kalshi' else None)
    return pos.unrealized(fill, fee_model(mk)), fill


def position_rows(svc, mode, status=('OPEN', 'AWAITING_SETTLEMENT')):
    rows = []
    for p in store.positions(mode, status, limit=200):
        u, _ = mark_position(svc, p) if p['status'] == 'OPEN' else (None, None)
        rows.append(dict(p, unrealized=_money(u), remaining=max(0, p['window_end'] - svc.now())))
    return rows


def overview(svc):
    S, now = svc.settings(), svc.now()
    stats = signal_stats()
    perf = store.performance('PAPER')
    start, curve = store.equity_curve('PAPER')
    exp = store.exposure('PAPER')
    unreal = ZERO; unknown = 0
    for p in store.positions('PAPER', 'OPEN'):
        u, _ = mark_position(svc, p)
        if u is None: unknown += 1
        else: unreal += u
    lm = svc.live_markets()
    feed = svc.feed.latest()
    pairs = svc.pairs
    detected = sum(s['accepted'] + s['detected_rejected'] for s in stats.values())
    rejected = sum(s['pre_rejected'] for s in stats.values())
    return {
        'mode': 'LIVE' if svc.live else 'PAPER', 'killed': svc.risk.killed(), 'kill_reason': svc.risk.kill_reason(),
        'uptime': now - svc.started, 'last_ok_age': (now - svc.last_ok) if svc.last_ok else None, 'cycles': svc.cycles, 'errors': {k: v for k, v in svc.errors.items() if v},
        'feed': {'price': feed[1] if feed else None, 'age': svc.feed.age(), 'source': feed[2] if feed else None, 'error': svc.feed.last_error},
        'live_markets': {f'{v}-{dur // 60}m': sum(1 for m in lm if m.venue == v and m.duration == dur) for v in ('polymarket', 'kalshi') for dur in (300, 900)},
        'discovered': len(svc.markets), 'pairs': len(pairs), 'verified': sum(1 for p in pairs if p.status == 'VERIFIED'),
        'detected': detected, 'rejected': rejected, 'top_reasons': top_reasons(stats),
        'paper': {'start': float(start), 'cash': {v: float(store.cash('PAPER', v)) for v in ('polymarket', 'kalshi')}, 'exposure': {v: float(exp.get(v, ZERO)) for v in ('polymarket', 'kalshi')},
                  'unrealized': float(unreal), 'unrealized_unknown': unknown, 'perf': perf, 'drawdown': store.max_drawdown('PAPER'),
                  'return_pct': (perf['net_pnl'] / float(start) * 100) if start else None},
        'open': position_rows(svc, 'PAPER'), 'recent': store.trades('PAPER', 8),
    }


def markets_view(svc):
    now, S = svc.now(), svc.settings(); rows = []
    for m in sorted(svc.markets.values(), key=lambda m: (m.duration, m.start_ts, m.venue)):
        if m.end_ts < now - 30: continue
        o = svc.obs.get(m.key)
        live = m.start_ts <= now < m.end_ts
        row = {'m': m, 'live': live, 'remaining': max(0, m.end_ts - now), 'obs': o, 'age': o.age(now) if o else None}
        if o:
            row.update(up_bid=o.up.best_bid, up_ask=o.up.best_ask, dn_bid=o.down.best_bid, dn_ask=o.down.best_ask, spread=o.up.spread,
                       depth=o.up.depth('asks', 3) + o.down.depth('asks', 3))
        ok, why, _, _ = time_ok(m, now, S) if live else (False, 'not live', 0, 0)
        row['entry_ok'], row['entry_why'] = ok, why
        rows.append(row)
    return {'rows': rows, 'pairs': svc.pairs, 'available': {f'{v}-{dur // 60}m': any(m.venue == v and m.duration == dur for m in svc.markets.values()) for v in ('polymarket', 'kalshi') for dur in (300, 900)}}


def strategies_view(svc):
    S = svc.settings(); stats = signal_stats(); out = []
    for key, title, desc in STRATS:
        enabled = {'arb': S.s_arb, 'momentum': S.s_momentum, 'meanrev': S.s_meanrev, 'bundle': S.s_bundle}[key]
        st = stats.get(key, {'accepted': 0, 'detected_rejected': 0, 'pre_rejected': 0, 'reasons': {}})
        out.append({'key': key, 'title': title, 'desc': desc, 'enabled': enabled, 'stats': st, 'reasons': sorted(st['reasons'].items(), key=lambda kv: -kv[1])[:5],
                    'perf': store.performance('PAPER', key), 'perf5': store.performance('PAPER', key, 300), 'perf15': store.performance('PAPER', key, 900),
                    'live_ok': key in S.live_strategies, 'live_capable': True})
    return {'strategies': out, 'notes': NOTES}


def paper_view(svc):
    S = svc.settings(); start, curve = store.equity_curve('PAPER')
    exp = store.exposure('PAPER'); perf = store.performance('PAPER')
    accts = []
    for v in ('polymarket', 'kalshi'):
        c, e, sb = store.cash('PAPER', v), exp.get(v, ZERO), store.start_balance('PAPER', v)
        accts.append({'venue': v, 'start': float(sb), 'cash': float(c), 'exposure': float(e), 'equity_at_cost': float(c + e)})
    with db.connect() as c:
        orders = [dict(r) for r in c.execute('SELECT * FROM b_orders WHERE mode=\'PAPER\' ORDER BY id DESC LIMIT 100')]
        tot = c.execute('SELECT COALESCE(SUM(fees),0) f, COALESCE(SUM(slippage),0) s, COUNT(*) n, COALESCE(SUM(status=\'UNFILLED\'),0) u, COALESCE(SUM(status=\'PARTIAL\'),0) p, COALESCE(SUM(status=\'REJECTED\'),0) r FROM b_orders WHERE mode=\'PAPER\'').fetchone()
        events = [dict(r) for r in c.execute('SELECT * FROM b_events ORDER BY id DESC LIMIT 15')]
    by = {}
    for p in store.open_positions('PAPER'):
        for k in (f"venue:{p['venue']}", f"duration:{int(p['duration']) // 60}m", f"strategy:{p['strategy']}"): by[k] = by.get(k, 0.0) + p['paid']
    pts = [(0.0, float(start))] + [(t, e) for t, e in curve] if curve else []
    svg = ''
    if len(pts) >= 2:
        t0, t1 = pts[0][0], pts[-1][0]; lo, hi = min(e for _, e in pts), max(e for _, e in pts)
        svg = ' '.join(f'{(i / (len(pts) - 1)) * 100:.2f},{(100 - ((e - lo) / ((hi - lo) or 1)) * 90 - 5):.2f}' for i, (_, e) in enumerate(pts))
    return {'accounts': accts, 'start': float(start), 'curve': curve, 'curve_svg': svg, 'perf': perf, 'drawdown': store.max_drawdown('PAPER'), 'orders': orders, 'order_totals': dict(tot),
            'open': position_rows(svc, 'PAPER'), 'trades': store.trades('PAPER', 100), 'events': events, 'exposure_by': sorted(by.items()),
            'return_pct': (perf['net_pnl'] / float(start) * 100) if start else None}
