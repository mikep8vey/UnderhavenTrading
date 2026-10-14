"""Dynamic discovery of BTC Up/Down 5-minute and 15-minute markets on Polymarket and Kalshi. Nothing is hard-coded except the
documented slug / series naming conventions; every identifier comes from the live exchange response."""
import json, re, time
from datetime import datetime, timezone
from decimal import Decimal
from concurrent.futures import as_completed, TimeoutError as FutTimeout
from .models import BtcMarket, detect_averaging, detect_oracle, detect_tie

DURATIONS = {300: '5m', 900: '15m'}


def _iso_ts(s):
    if not s: return None
    try:
        d = datetime.fromisoformat(str(s).replace('Z', '+00:00'))
        return (d if d.tzinfo else d.replace(tzinfo=timezone.utc)).timestamp()
    except ValueError: return None


def _dec(v, default='0'):
    try: return Decimal(str(v))
    except Exception: return Decimal(default)


def _jlist(v):
    if isinstance(v, str):
        try: v = json.loads(v)
        except ValueError: return []
    return v if isinstance(v, list) else []


# ----------------------------------------------------------------------------- Polymarket
def normalize_polymarket(m, duration, start_ts, default_fee='0.07'):
    """Gamma market JSON -> BtcMarket. V2 markets carry `positionIds`; V1 carry `clobTokenIds`; `version` decides."""
    ids = _jlist(m.get('positionIds')) if m.get('version') == 'v2' else _jlist(m.get('clobTokenIds'))
    if not ids: ids = _jlist(m.get('clobTokenIds')) or _jlist(m.get('positionIds'))
    labels = [str(x).strip().lower() for x in _jlist(m.get('outcomes'))]
    rules = ' '.join(str(x) for x in (m.get('description'), m.get('resolutionSource')) if x)
    mk = BtcMarket('polymarket', str(m.get('id') or m.get('conditionId') or ''), duration, float(start_ts), float(start_ts + duration),
                   title=str(m.get('question') or ''), rules=rules, slug=str(m.get('slug') or ''))
    if len(ids) == 2 and labels == ['up', 'down']: mk.up_id, mk.down_id, mk.outcome_ok = str(ids[0]), str(ids[1]), True
    elif len(ids) == 2 and labels == ['down', 'up']: mk.up_id, mk.down_id, mk.outcome_ok = str(ids[1]), str(ids[0]), True
    elif len(ids) == 2: mk.up_id, mk.down_id = str(ids[0]), str(ids[1]); mk.flags.append(f'outcome labels {labels} are not Up/Down')
    mk.tradable = bool(m.get('active') is True and m.get('closed') is not True and m.get('acceptingOrders') is not False
                       and m.get('enableOrderBook') is not False and len(ids) == 2)
    mk.status = 'open' if mk.tradable else 'not tradable'
    mk.tick = _dec(m.get('orderPriceMinTickSize') or '0.01')
    mk.min_notional = _dec(m.get('orderMinSize') or '0')          # USDC notional (official docs)
    mk.seconds_delay = float(m.get('secondsDelay') or 0)
    fs = m.get('feeSchedule')
    if isinstance(fs, dict) and fs.get('rate') is not None:
        mk.fee_rate, mk.fee_exponent, mk.fee_known = _dec(fs['rate']), _dec(fs.get('exponent') or 1), True
    elif m.get('feesEnabled') is False: mk.fee_rate, mk.fee_known = Decimal('0'), True
    else: mk.fee_rate = _dec(default_fee)       # documented crypto rate; flagged as an estimate (fee_known False)
    mk.oracle, mk.averaging, mk.tie_rule = detect_oracle(rules), detect_averaging(rules), detect_tie(rules)
    if m.get('closed') and m.get('outcomePrices'):
        pr = [float(x) for x in _jlist(m.get('outcomePrices')) if str(x).replace('.', '', 1).isdigit()]
        if len(pr) == 2 and max(pr) >= 0.99: mk.result = 'up' if (pr[0] >= 0.99) == (labels[:1] == ['up']) else 'down'
    return mk


def discover_polymarket(pm, now=None, ahead=1, durations=(300, 900)):
    """Fetch the previous, live and next `ahead` windows for each duration via the documented slug convention btc-updown-<5m|15m>-<start>."""
    now = now or time.time(); jobs = {}
    for dur in durations:
        base = int(now) - int(now) % dur
        for k in range(-1, ahead + 1):
            ts = base + k * dur
            slug = f'btc-updown-{DURATIONS[dur]}-{ts}'
            jobs[pm._pool.submit(pm._gamma_event, slug)] = (dur, ts, slug)
    out, errors, seen = [], [], set()
    try:
        for fut in as_completed(jobs, timeout=12):
            dur, ts, slug = jobs[fut]
            try: events = fut.result()
            except Exception as exc: errors.append(f'{slug}: {type(exc).__name__}'); continue
            for ev in events:
                for m in ev.get('markets') or []:
                    mk = normalize_polymarket(dict(m), dur, ts)
                    if mk.market_id and mk.market_id not in seen: seen.add(mk.market_id); out.append(mk)
    except FutTimeout: errors.append('timed out')
    return out, errors


# ----------------------------------------------------------------------------- Kalshi
def _kprice(m, name):
    v = m.get(f'{name}_dollars')
    if v is None: v = m.get(name)
    if v is None: return None
    f = float(v)
    return f / 100.0 if (m.get(f'{name}_dollars') is None and f > 1.0) else f


def kalshi_duration(m):
    """Window length from the series ticker (…15M / …5M) or the title ('next 15 mins'); None if it is not a 5/15-minute market."""
    series = str(m.get('series_ticker') or str(m.get('event_ticker') or '').split('-')[0] or str(m.get('ticker') or '').split('-')[0]).upper()
    mt = re.search(r'(\d+)M$', series)
    if mt and int(mt.group(1)) * 60 in DURATIONS: return int(mt.group(1)) * 60
    mt = re.search(r'next (\d+) min', str(m.get('title') or '').lower())
    if mt and int(mt.group(1)) * 60 in DURATIONS: return int(mt.group(1)) * 60
    return None


def is_btc_updown(m):
    text = f"{m.get('title') or ''} {m.get('subtitle') or ''} {m.get('ticker') or ''}".lower()
    return bool(re.search(r'\bbtc\b|bitcoin|kxbtc', text)) and bool(re.search(r'\bup\b|up or down|higher', text))


def normalize_kalshi(m):
    dur = kalshi_duration(m)
    if not dur or not is_btc_updown(m): return None
    end = _iso_ts(m.get('expiration_time')) or _iso_ts(m.get('close_time'))
    if end is None: return None
    rules = ' '.join(str(x) for x in (m.get('rules_primary'), m.get('rules_secondary')) if x)
    mk = BtcMarket('kalshi', str(m.get('ticker') or ''), dur, end - dur, end, title=str(m.get('title') or ''), rules=rules)
    mk.status = str(m.get('status') or '').lower()
    mk.tradable = mk.status in ('open', 'active')
    mk.tick, mk.min_qty = Decimal('0.01'), Decimal('1')
    mk.fee_multiplier = _dec(m.get('fee_multiplier') or 1); mk.fee_known = m.get('fee_multiplier') is not None
    mk.oracle, mk.averaging, mk.tie_rule = detect_oracle(rules), detect_averaging(rules), detect_tie(rules)
    mk.outcome_ok = bool(re.search(r'\bup\b|higher|greater', f"{m.get('title') or ''} {m.get('yes_sub_title') or ''}".lower()))
    if not mk.outcome_ok: mk.flags.append('YES is not clearly "price up"')
    if str(m.get('result') or '').lower() in ('yes', 'no'): mk.result = 'up' if str(m['result']).lower() == 'yes' else 'down'
    return mk


def discover_kalshi(client, series=None, now=None, max_pages=10):
    """Targeted refresh when series are known, otherwise a generic scan of open markets for BTC up/down 5/15-minute contracts."""
    out, found_series, errors = [], set(), []
    try:
        it = []
        for s in (series or []): it += list(client.iter_markets(max_pages=2, status='open', series_ticker=s))
        if not series: it = client.iter_markets(max_pages=max_pages, status='open')
        for m in it:
            mk = normalize_kalshi(m)
            if mk:
                out.append(mk); found_series.add(str(m.get('series_ticker') or str(m.get('event_ticker') or '').split('-')[0]))
    except Exception as exc: errors.append(f'{type(exc).__name__}: {str(exc)[:100]}')
    return out, sorted(s for s in found_series if s), errors
