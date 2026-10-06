"""A tiny stand-in for Polymarket's Gamma + CLOB HTTP APIs so the scanner can be tested offline.

Behaviours you can switch on per coin/window:
  - normal book (pair > 1, no gap)           - a real gap that stays put
  - a PHANTOM gap that disappears on re-check - a window whose book does not exist yet (404)
  - a Gamma failure for one lookup            - batch /books unsupported (forces the parallel fallback)
"""
import json, time

WINDOW = 900


class Resp:
    def __init__(self, status, payload):
        self.status_code = status; self._p = payload; self.text = json.dumps(payload)
    def json(self): return self._p
    def raise_for_status(self):
        if self.status_code >= 400:
            import requests
            raise requests.HTTPError(f'{self.status_code} error', response=self)


class FakeExchange:
    def __init__(self):
        self.books_calls = 0; self.single_calls = 0; self.gamma_calls = 0; self.batch_calls = 0
        self.batch_supported = True
        self.gamma_fail = set()        # slugs that raise
        self.no_gamma = set()          # coins with no event at all
        self.nobook = set()            # (coin, k) windows whose book does not exist yet
        self.gap = {}                  # (coin, k) -> (up_ask, down_ask) stable gap
        self.phantom = {}              # (coin, k) -> (up_ask, down_ask) shown ONCE, then back to normal
        self._phantom_seen = set()
        self.calls = []
        self.tokens = {}

    @staticmethod
    def base(now=None):
        now = int(now or time.time()); return now - now % WINDOW

    def _slug_parts(self, slug):
        coin, _, _, ts = slug.split('-')
        return coin.upper(), (int(ts) - self.base()) // WINDOW

    def get(self, url, params=None, timeout=None, **kw):
        self.calls.append(('GET', url))
        if '/events' in url:
            self.gamma_calls += 1
            slug = params['slug']
            if slug in self.gamma_fail: raise ConnectionError('simulated Gamma outage for ' + slug)
            coin, k = self._slug_parts(slug)
            if coin in self.no_gamma: return Resp(200, [])
            ts = int(slug.rsplit('-', 1)[1]); y, n = f'{coin}-{k}-UP', f'{coin}-{k}-DN'
            self.tokens[y] = (coin, k, 'up'); self.tokens[n] = (coin, k, 'down')
            return Resp(200, [{'slug': slug, 'markets': [{
                'slug': slug, 'question': f'{coin} Up or Down - window {ts}', 'id': f'm{coin}{k}', 'conditionId': f'0xc{coin}{k}',
                'active': True, 'closed': k < 0 and False, 'acceptingOrders': True, 'enableOrderBook': True,
                'clobTokenIds': json.dumps([y, n]), 'negRisk': False}]}])
        if url.endswith('/book'):
            self.single_calls += 1
            b = self._book(params['token_id'])
            return Resp(404, {'error': 'No orderbook exists for the requested token id'}) if b is None else Resp(200, b)
        raise AssertionError('unexpected GET ' + url)

    def post(self, url, json=None, timeout=None, **kw):
        self.calls.append(('POST', url))
        if url.endswith('/books'):
            self.batch_calls += 1
            if not self.batch_supported: return Resp(404, {'error': 'not found'})
            out = []
            for item in json:
                b = self._book(item['token_id'])
                if b is not None: out.append(b)
            return Resp(200, out)
        raise AssertionError('unexpected POST ' + url)

    def _book(self, token):
        coin, k, side = self.tokens[token]
        if (coin, k) in self.nobook: return None
        up, down = (0.50, 0.51)  # pair 1.01: no gap
        if (coin, k) in self.gap: up, down = self.gap[(coin, k)]
        if (coin, k) in self.phantom:
            if (coin, k) not in self._phantom_seen:
                if side == 'down': self._phantom_seen.add((coin, k))   # flips after the Down book is read once
                up, down = self.phantom[(coin, k)]
        ask = up if side == 'up' else down
        return {'asset_id': token, 'market': f'0xc{coin}{k}', 'timestamp': str(int(time.time() * 1000)), 'tick_size': '0.01', 'min_order_size': '5',
                'asks': [{'price': f'{ask:.2f}', 'size': '40'}, {'price': f'{ask + 0.02:.2f}', 'size': '200'}],
                'bids': [{'price': f'{ask - 0.02:.2f}', 'size': '100'}]}
