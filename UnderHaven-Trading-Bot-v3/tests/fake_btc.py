"""TEST FIXTURES ONLY. A deterministic stand-in for Polymarket's Gamma/CLOB and Kalshi's REST APIs, with the SAME response shapes the real
clients parse. It is never imported by the application and its numbers are never presented as market data."""
import json, math
from concurrent.futures import ThreadPoolExecutor
from app.kalshi import parse_orderbook
from app.polymarket import PolymarketService
from app.btc.feeds import PriceFeed

BASE = (1_900_000_000 // 900) * 900      # a 15-minute-aligned epoch
POLY_RULES = ('This market resolves to Up if the Chainlink BTC/USD price at the end of the time range is greater than or equal to the price at the beginning '
              'of the time range. Otherwise it resolves to Down.')
KAL_RULES = ('Resolves YES if the 60-second simple average of CF Benchmarks BRTI ending just before the later time is greater than or equal to the 60-second '
             'simple average ending just before the earlier time.')


class Clock:
    def __init__(self, t): self.t = t
    def __call__(self): return self.t
    def advance(self, s): self.t += s


def lv(levels): return [{'price': f'{p:.2f}', 'size': str(s)} for p, s in levels]


class FakePm:
    """Quacks like PolymarketService for everything the BTC service touches."""
    _levels = staticmethod(PolymarketService._levels)

    def __init__(self):
        self._pool = ThreadPoolExecutor(2)
        self.events, self.books_by_token, self.results = {}, {}, {}
        self.calls = {'books': 0, 'gamma': 0}
        self.vault = type('V', (), {'decrypt': staticmethod(lambda x: x), 'encrypt': staticmethod(lambda x: x)})()
        self._http = self
        self.creds_ok = False
        self.fail_books = False

    def creds(self): return {'x': 1} if self.creds_ok else None
    def balance(self): return {'balance': '20000000', 'allowances': {}}
    def client(self): return None

    # Gamma event by slug
    def _gamma_event(self, slug):
        self.calls['gamma'] += 1
        return self.events.get(slug, [])

    def books(self, tokens):
        self.calls['books'] += 1
        if self.fail_books: raise ConnectionError('simulated outage')
        return {t: self.books_by_token.get(t) for t in tokens}

    # requests-like .get for the settlement lookup
    def get(self, url, timeout=None):
        mid = url.rsplit('/', 1)[1]
        m = self.results.get(mid)
        r = type('R', (), {})(); r.raise_for_status = lambda: None; r.json = lambda: m
        return r

    def add_window(self, dur, start, mid=None, up=(0.50, 100), down=(0.50, 100), up_bid=0.48, down_bid=0.48, rules=POLY_RULES, **extra):
        slug = f'btc-updown-{"5m" if dur == 300 else "15m"}-{start}'
        mid = mid or f'P{dur}-{start}'; tu, td = f'{mid}-U', f'{mid}-D'
        m = {'id': mid, 'conditionId': '0x' + mid, 'question': f'Bitcoin Up or Down {dur // 60}m', 'description': rules, 'outcomes': '["Up", "Down"]',
             'clobTokenIds': json.dumps([tu, td]), 'active': True, 'closed': False, 'acceptingOrders': True, 'enableOrderBook': True, 'slug': slug,
             'orderPriceMinTickSize': 0.01, 'orderMinSize': 1, 'feesEnabled': True, 'feeSchedule': {'rate': 0.07, 'exponent': 1, 'takerOnly': True}}
        m.update(extra)
        self.events[slug] = [{'slug': slug, 'markets': [m]}]
        self.set_books(mid, up, down, up_bid, down_bid)
        return mid

    def set_books(self, mid, up, down, up_bid=0.48, down_bid=0.48):
        ups = up if isinstance(up, list) else [up]; dns = down if isinstance(down, list) else [down]
        self.books_by_token[f'{mid}-U'] = {'asks': lv(ups), 'bids': lv([(up_bid, 500)])}
        self.books_by_token[f'{mid}-D'] = {'asks': lv(dns), 'bids': lv([(down_bid, 500)])}


class FakeKalshi:
    def __init__(self):
        self.markets, self.books, self.calls, self.fail = {}, {}, 0, False
    env = 'prod'

    def add(self, dur, start, ticker=None, yes_ask=0.50, no_ask=0.50, depth=100, rules=KAL_RULES, status='active', **extra):
        series = f'KXBTC{dur // 60}M'; ticker = ticker or f'{series}-{start}-00'
        m = {'ticker': ticker, 'event_ticker': f'{series}-{start}', 'series_ticker': series, 'title': f'BTC price up in next {dur // 60} mins?', 'rules_primary': rules,
             'expiration_time': _iso(start + dur), 'close_time': _iso(start + dur), 'status': status, 'yes_sub_title': 'Price up'}
        m.update(extra); self.markets[ticker] = m
        # Kalshi lists BIDS only: a NO bid at (1 - yes_ask) is the YES ask
        self.books[ticker] = {'orderbook_fp': {'yes_dollars': [[f'{1 - no_ask:.4f}', f'{depth}.00']], 'no_dollars': [[f'{1 - yes_ask:.4f}', f'{depth}.00']]}}
        return ticker

    def iter_markets(self, max_pages=1, **params):
        for m in self.markets.values():
            if params.get('series_ticker') and m['series_ticker'] != params['series_ticker']: continue
            yield m

    def get_orderbook(self, ticker, depth=0):
        self.calls += 1
        if self.fail: raise ConnectionError('simulated outage')
        return parse_orderbook(self.books[ticker])

    def get_market(self, ticker): return self.markets[ticker]


def _iso(ts):
    from datetime import datetime, timezone
    return datetime.fromtimestamp(ts, timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def make_feed(clock, start_ts, start_price=60000.0, now_price=60000.0, seconds=400, jitter=0.0001):
    """A deterministic price path: flat zig-zag around a linear move from the window start price to now_price."""
    f = PriceFeed(sources=[], now=clock)
    t0 = clock() - seconds
    n = int(seconds)
    for i in range(n + 1):
        t = t0 + i
        frac = max(0.0, min(1.0, (t - start_ts) / max(1.0, clock() - start_ts)))
        trend = start_price + (now_price - start_price) * frac if t >= start_ts else start_price
        f.history.append((t, trend * (1 + jitter * (1 if i % 2 else -1)), 'fixture'))
    return f
