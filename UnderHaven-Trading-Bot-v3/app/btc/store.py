"""Persistence for the BTC engine (SQLite). Every signal, order, fill, position change and event is written here."""
import json, time
from decimal import Decimal
from .. import db
from .calc import Position, d, ZERO

POS_FIELDS = ('qty', 'paid', 'entry_fees', 'realized', 'bought', 'sold', 'buy_cost', 'exit_proceeds', 'exit_fees', 'fees_total')


def now(): return time.time()


def event(level, kind, message, data=None, ts=None):
    with db.connect() as c:
        c.execute('INSERT INTO b_events(ts,level,kind,message,data) VALUES(?,?,?,?,?)', (ts or now(), level, kind, message[:500], json.dumps(data or {}, default=str)))


def health_set(key, value):
    with db.connect() as c:
        c.execute('INSERT INTO b_health(key,value,ts) VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value, ts=excluded.ts', (key, str(value), now()))


def health_get(key, default=None):
    with db.connect() as c:
        r = c.execute('SELECT value,ts FROM b_health WHERE key=?', (key,)).fetchone()
    return (r['value'], r['ts']) if r else (default, None)


def signal(ts, strategy, market, side, accepted, reason='', edge=None, p_model=None, price=None, qty=None, features=None, group_id=None, mode='PAPER'):
    with db.connect() as c:
        c.execute('INSERT INTO b_signals(ts,strategy,duration,venue,market_id,side,accepted,reason,edge,p_model,price,qty,features,group_id,mode) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)',
                  (ts, strategy, getattr(market, 'duration', None), getattr(market, 'venue', ''), getattr(market, 'market_id', ''), side, 1 if accepted else 0, reason,
                   None if edge is None else float(edge), p_model, None if price is None else float(price), None if qty is None else float(qty),
                   json.dumps(features or {}, default=str), group_id, mode))


def order(**k):
    cols = ('ts', 'mode', 'group_id', 'strategy', 'venue', 'market_id', 'duration', 'window_start', 'side', 'action', 'tif', 'qty_req', 'limit_price', 'status',
            'qty_filled', 'avg_price', 'fees', 'slippage', 'reason', 'order_ref', 'estimated')
    vals = [(float(k.get(c)) if isinstance(k.get(c), Decimal) else k.get(c)) for c in cols]
    with db.connect() as c:
        return c.execute(f'INSERT INTO b_orders({",".join(cols)}) VALUES({",".join("?" * len(cols))})', vals).lastrowid


def accounts_ensure(mode, balances):
    with db.connect() as c:
        for venue, bal in balances.items():
            c.execute('INSERT OR IGNORE INTO b_accounts(venue,mode,start_balance) VALUES(?,?,?)', (venue, mode, float(bal)))


def account_set_start(mode, venue, bal):
    with db.connect() as c:
        c.execute('INSERT INTO b_accounts(venue,mode,start_balance) VALUES(?,?,?) ON CONFLICT(venue,mode) DO UPDATE SET start_balance=excluded.start_balance', (venue, mode, float(bal)))


def start_balance(mode, venue):
    with db.connect() as c:
        r = c.execute('SELECT start_balance FROM b_accounts WHERE venue=? AND mode=?', (venue, mode)).fetchone()
    return d(r['start_balance']) if r else ZERO


def cash(mode, venue):
    """start balance + every cash flow: -buy cost -entry fees +sell proceeds -exit fees +settlement payouts (exact, reconstructed from positions)."""
    with db.connect() as c:
        r = c.execute('SELECT COALESCE(SUM(exit_proceeds - exit_fees - buy_cost - entry_total),0) AS flow FROM '
                      '(SELECT exit_proceeds, exit_fees, buy_cost, (fees_total - exit_fees) AS entry_total FROM b_positions WHERE mode=? AND venue=?)', (mode, venue)).fetchone()
    return start_balance(mode, venue) + d(r['flow'])


def pos_to_row(p: Position): return {k: float(getattr(p, k)) for k in POS_FIELDS}


def pos_from_row(r):
    p = Position()
    for k in POS_FIELDS: setattr(p, k, d(r[k]))
    return p


def position_create(mode, group_id, strategy, market, side, pos: Position, status='OPEN', note=''):
    row = pos_to_row(pos)
    with db.connect() as c:
        return c.execute('INSERT INTO b_positions(mode,group_id,strategy,venue,market_id,duration,window_start,window_end,side,qty,paid,entry_fees,realized,bought,sold,buy_cost,'
                         'exit_proceeds,exit_fees,fees_total,status,opened_ts,note) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)',
                         (mode, group_id, strategy, market.venue, market.market_id, market.duration, market.start_ts, market.end_ts, side, row['qty'], row['paid'], row['entry_fees'],
                          row['realized'], row['bought'], row['sold'], row['buy_cost'], row['exit_proceeds'], row['exit_fees'], row['fees_total'], status, now(), note)).lastrowid


def position_update(pid, pos: Position, status=None, settle_value=None, note=None):
    row = pos_to_row(pos); sets = ', '.join(f'{k}=?' for k in POS_FIELDS); vals = [row[k] for k in POS_FIELDS]
    if status: sets += ', status=?'; vals.append(status)
    if status in ('CLOSED', 'SETTLED'): sets += ', closed_ts=?'; vals.append(now())
    if settle_value is not None: sets += ', settle_value=?'; vals.append(float(settle_value))
    if note is not None: sets += ', note=?'; vals.append(note)
    with db.connect() as c:
        c.execute(f'UPDATE b_positions SET {sets} WHERE id=?', vals + [pid])


def positions(mode=None, status=None, group_id=None, limit=500):
    q, args = 'SELECT * FROM b_positions WHERE 1=1', []
    for col, v in (('mode', mode), ('group_id', group_id)):
        if v: q += f' AND {col}=?'; args.append(v)
    if status:
        sts = status if isinstance(status, (list, tuple)) else [status]
        q += f' AND status IN ({",".join("?" * len(sts))})'; args += list(sts)
    q += ' ORDER BY id DESC LIMIT ?'; args.append(limit)
    with db.connect() as c: return [dict(r) for r in c.execute(q, args)]


def open_positions(mode): return positions(mode, ('OPEN', 'AWAITING_SETTLEMENT'))


def exposure(mode):
    """Capital currently tied up (cost basis of remaining quantity), per venue and in total."""
    out = {}
    for p in open_positions(mode): out[p['venue']] = out.get(p['venue'], ZERO) + d(p['paid'])
    return out


def realized_since(mode, since_ts, venue=None):
    q = 'SELECT COALESCE(SUM(realized),0) AS r FROM b_positions WHERE mode=? AND status IN (\'CLOSED\',\'SETTLED\') AND closed_ts>=?'; a = [mode, since_ts]
    if venue: q += ' AND venue=?'; a.append(venue)
    with db.connect() as c: return d(c.execute(q, a).fetchone()['r'])


def trades(mode, limit=2000):
    """A 'trade' is a group of legs (one or two positions) that are all CLOSED or SETTLED."""
    with db.connect() as c:
        rows = [dict(r) for r in c.execute('SELECT * FROM b_positions WHERE mode=? ORDER BY id', (mode,))]
    groups = {}
    for r in rows: groups.setdefault(r['group_id'], []).append(r)
    out = []
    for gid, legs in groups.items():
        done = all(l['status'] in ('CLOSED', 'SETTLED') for l in legs)
        pnl = sum(l['realized'] for l in legs)
        out.append({'group_id': gid, 'strategy': legs[0]['strategy'], 'duration': legs[0]['duration'], 'venues': sorted({l['venue'] for l in legs}), 'legs': legs,
                    'done': done, 'pnl': pnl, 'fees': sum(l['fees_total'] for l in legs), 'opened': min(l['opened_ts'] for l in legs),
                    'closed': max((l['closed_ts'] or 0) for l in legs) if done else None, 'cost': sum(l['buy_cost'] + (l['fees_total'] - l['exit_fees']) for l in legs)})
    return sorted(out, key=lambda g: g['opened'], reverse=True)[:limit]


def performance(mode, strategy=None, duration=None):
    ts = [t for t in trades(mode) if t['done'] and (not strategy or t['strategy'] == strategy) and (not duration or t['duration'] == duration)]
    wins = [t['pnl'] for t in ts if t['pnl'] > 0]; losses = [t['pnl'] for t in ts if t['pnl'] < 0]
    gp, gl = sum(wins), -sum(losses)
    return {'trades': len(ts), 'wins': len(wins), 'losses': len(losses), 'win_rate': (len(wins) / len(ts) * 100) if ts else None, 'net_pnl': sum(t['pnl'] for t in ts),
            'avg_win': (gp / len(wins)) if wins else None, 'avg_loss': (-gl / len(losses)) if losses else None, 'profit_factor': (gp / gl) if gl > 0 else None,
            'fees': sum(t['fees'] for t in ts), 'avg_hold': (sum((t['closed'] - t['opened']) for t in ts) / len(ts)) if ts else None}


def equity_curve(mode):
    """Cumulative realized P&L over completed trades, added to the combined starting balance. No point is invented: only recorded closes."""
    start = sum((start_balance(mode, v) for v in ('polymarket', 'kalshi')), ZERO)
    pts, eq = [], start
    for t in sorted([t for t in trades(mode) if t['done']], key=lambda t: t['closed']):
        eq += d(t['pnl']); pts.append((t['closed'], float(eq)))
    return float(start), pts


def max_drawdown(mode):
    start, pts = equity_curve(mode)
    peak, mdd = start, 0.0
    for _, eq in pts:
        peak = max(peak, eq); mdd = max(mdd, peak - eq)
    return mdd


def sample_ref(ts, price, source, keep_days=3):
    with db.connect() as c:
        c.execute('INSERT INTO b_ref(ts,price,source) VALUES(?,?,?)', (ts, price, source))
        if int(ts) % 600 == 0: c.execute('DELETE FROM b_ref WHERE ts<?', (ts - keep_days * 86400,))
