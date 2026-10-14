"""Kalshi REST client: public market data and RSA-PSS signed trading.

Authentication (Kalshi API v2): every private request carries
    KALSHI-ACCESS-KEY        your API key id
    KALSHI-ACCESS-TIMESTAMP  milliseconds since the epoch
    KALSHI-ACCESS-SIGNATURE  base64( RSA-PSS-SHA256( timestamp + METHOD + path ) )
where `path` is the full URL path (including /trade-api/v2) WITHOUT the query string.

Nothing here prints or returns a secret. The private key is held in memory only.
"""
import base64, math, threading, time, uuid
from urllib.parse import urlparse

import requests
from requests.adapters import HTTPAdapter

ENVS = {
    'prod': 'https://api.elections.kalshi.com/trade-api/v2',
    'demo': 'https://demo-api.kalshi.co/trade-api/v2',
}


class KalshiError(Exception):
    def __init__(self, message, status=None, body=None):
        super().__init__(message)
        self.status, self.body = status, body


class KalshiUncertain(KalshiError):
    """A write request may or may not have been processed (timeout / 5xx). Never blindly retry it."""


class KalshiAuthError(KalshiError):
    pass


def _price(v):
    """Kalshi prices arrive as integer cents (older) or dollar strings (fixed-point). Return dollars."""
    if v is None: return None
    if isinstance(v, str): return float(v)
    if isinstance(v, int): return v / 100.0
    return float(v) / 100.0 if v > 1.0 else float(v)


def _qty(v):
    return float(v) if v is not None else 0.0


def parse_orderbook(js):
    """-> {'yes_bids': [(price, qty)] best first, 'no_bids': [...], 'yes_asks': [...], 'no_asks': [...], 'exotic_ticks': bool}.
    Kalshi only lists BIDS. A YES bid at p is a NO ask at 1-p, and vice versa."""
    ob = (js or {}).get('orderbook') or {}
    fp = (js or {}).get('orderbook_fp') or {}
    yes = ob.get('yes') if ob.get('yes') is not None else fp.get('yes_dollars')
    no = ob.get('no') if ob.get('no') is not None else fp.get('no_dollars')
    yes_b = sorted([(_price(p), _qty(q)) for p, q in (yes or []) if _qty(q) > 0], key=lambda x: -x[0])
    no_b = sorted([(_price(p), _qty(q)) for p, q in (no or []) if _qty(q) > 0], key=lambda x: -x[0])
    yes_a = sorted([(round(1.0 - p, 4), q) for p, q in no_b], key=lambda x: x[0])
    no_a = sorted([(round(1.0 - p, 4), q) for p, q in yes_b], key=lambda x: x[0])
    exotic = any(abs(p * 100 - round(p * 100)) > 1e-6 for p, _ in yes_b + no_b)
    return {'yes_bids': yes_b, 'no_bids': no_b, 'yes_asks': yes_a, 'no_asks': no_a, 'exotic_ticks': exotic}


class KalshiClient:
    def __init__(self, env='prod', key_id=None, private_key_pem=None, base_url=None, rate_per_sec=8.0,
                 timeout=(3.05, 8), session=None, sleep=time.sleep):
        self.env = env if env in ENVS else 'prod'
        self.base_url = (base_url or ENVS[self.env]).rstrip('/')
        self._prefix = urlparse(self.base_url).path.rstrip('/')
        self.key_id = key_id
        self._key = self._load_key(private_key_pem) if private_key_pem else None
        self.timeout = timeout
        self._min_gap = 1.0 / max(0.5, float(rate_per_sec))
        self._last = 0.0
        self._lock = threading.Lock()
        self._sleep = sleep
        self.http = session or requests.Session()
        if session is None: self.http.mount('https://', HTTPAdapter(pool_connections=2, pool_maxsize=8, max_retries=0))
        self.last_error = ''

    # ----------------------------------------------------------------- auth
    @staticmethod
    def _load_key(pem):
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric import rsa
        data = pem.encode() if isinstance(pem, str) else pem
        try:
            key = serialization.load_pem_private_key(data, password=None)
        except Exception:
            raise KalshiAuthError('The private key is not a valid unencrypted PEM key.')
        if not isinstance(key, rsa.RSAPrivateKey):
            raise KalshiAuthError('Kalshi requires an RSA private key.')
        return key

    @property
    def can_trade(self):
        return bool(self.key_id and self._key)

    def sign(self, method, path, ts_ms=None):
        from cryptography.hazmat.primitives import hashes
        from cryptography.hazmat.primitives.asymmetric import padding
        if not self.can_trade: raise KalshiAuthError('Kalshi credentials are not configured.')
        ts = str(ts_ms or int(time.time() * 1000))
        msg = (ts + method.upper() + path.split('?')[0]).encode()
        sig = self._key.sign(msg, padding.PSS(mgf=padding.MGF1(hashes.SHA256()), salt_length=padding.PSS.DIGEST_LENGTH), hashes.SHA256())
        return {'KALSHI-ACCESS-KEY': self.key_id, 'KALSHI-ACCESS-TIMESTAMP': ts, 'KALSHI-ACCESS-SIGNATURE': base64.b64encode(sig).decode()}

    # ----------------------------------------------------------------- transport
    def _throttle(self):
        with self._lock:
            wait = self._last + self._min_gap - time.time()
            if wait > 0: self._sleep(wait)
            self._last = time.time()

    def _request(self, method, endpoint, params=None, body=None, auth=False, retries=3, write=False):
        url = self.base_url + endpoint
        path = self._prefix + endpoint
        delay = 0.5
        for attempt in range(retries + 1):
            self._throttle()
            headers = {'Accept': 'application/json'}
            if auth: headers.update(self.sign(method, path))
            try:
                r = self.http.request(method, url, params=params, json=body, headers=headers, timeout=self.timeout)
            except (requests.ConnectionError, requests.Timeout) as exc:
                self.last_error = f'{type(exc).__name__}'
                if write:   # the order may have reached Kalshi: caller must reconcile, never resend blindly
                    raise KalshiUncertain(f'No response for {method} {endpoint} ({type(exc).__name__}); outcome unknown.')
                if attempt == retries: raise KalshiError(f'Kalshi unreachable: {type(exc).__name__}')
                self._sleep(delay); delay *= 2; continue
            if r.status_code == 429:
                wait = float(r.headers.get('Retry-After', delay)) if str(r.headers.get('Retry-After', '')).replace('.', '', 1).isdigit() else delay
                self.last_error = 'rate limited (429)'
                if attempt == retries: raise KalshiError('Kalshi rate limit exceeded', 429, r.text[:200])
                self._sleep(min(wait, 10)); delay *= 2; continue
            if r.status_code >= 500:
                self.last_error = f'HTTP {r.status_code}'
                if write: raise KalshiUncertain(f'HTTP {r.status_code} on {method} {endpoint}; outcome unknown.', r.status_code, r.text[:200])
                if attempt == retries: raise KalshiError(f'Kalshi server error {r.status_code}', r.status_code, r.text[:200])
                self._sleep(delay); delay *= 2; continue
            if r.status_code in (401, 403):
                raise KalshiAuthError(f'Kalshi rejected the credentials (HTTP {r.status_code}).', r.status_code, r.text[:200])
            if r.status_code >= 400:
                raise KalshiError(f'Kalshi HTTP {r.status_code}: {r.text[:200]}', r.status_code, r.text[:300])
            self.last_error = ''
            try:
                return r.json() if r.content else {}
            except ValueError:
                raise KalshiError('Kalshi returned a non-JSON response.', r.status_code)
        raise KalshiError('Kalshi request failed.')

    # ----------------------------------------------------------------- public data
    def iter_markets(self, max_pages=20, **params):
        params = dict(params); params.setdefault('limit', 1000); cursor = None
        for _ in range(max_pages):
            if cursor: params['cursor'] = cursor
            js = self._request('GET', '/markets', params=params)
            for m in js.get('markets') or []: yield m
            cursor = js.get('cursor')
            if not cursor: break

    def iter_events(self, max_pages=20, **params):
        params = dict(params); params.setdefault('limit', 200); cursor = None
        for _ in range(max_pages):
            if cursor: params['cursor'] = cursor
            js = self._request('GET', '/events', params=params)
            for e in js.get('events') or []: yield e
            cursor = js.get('cursor')
            if not cursor: break

    def get_markets_by_tickers(self, tickers):
        out = []
        for i in range(0, len(tickers), 100):
            js = self._request('GET', '/markets', params={'tickers': ','.join(tickers[i:i + 100]), 'limit': 100})
            out += js.get('markets') or []
        return out

    def get_market(self, ticker):
        return (self._request('GET', f'/markets/{ticker}') or {}).get('market') or {}

    def get_orderbook(self, ticker, depth=0):
        params = {'depth': depth} if depth else None
        return parse_orderbook(self._request('GET', f'/markets/{ticker}/orderbook', params=params))

    # ----------------------------------------------------------------- private
    def balance(self):
        js = self._request('GET', '/portfolio/balance', auth=True)
        cents = js.get('balance')
        return {'balance_usd': (cents / 100.0) if isinstance(cents, (int, float)) else None, 'raw_keys': sorted(js.keys())}

    def create_order(self, ticker, side, action, count, price, tif='fill_or_kill', client_order_id=None):
        """Limit order, `price` in dollars (converted to whole cents). tif: fill_or_kill | immediate_or_cancel | good_till_canceled."""
        if side not in ('yes', 'no') or action not in ('buy', 'sell'): raise KalshiError('bad side/action')
        count = int(count)
        if count < 1: raise KalshiError('count must be at least 1')
        cents = int(round(price * 100))
        if not 1 <= cents <= 99: raise KalshiError(f'price {price} outside 1-99 cents')
        body = {'ticker': ticker, 'client_order_id': client_order_id or f'uh-{uuid.uuid4().hex[:20]}', 'side': side, 'action': action,
                'count': count, 'type': 'limit', 'time_in_force': tif}
        body['yes_price' if side == 'yes' else 'no_price'] = cents
        js = self._request('POST', '/portfolio/orders', body=body, auth=True, retries=0, write=True)
        return js.get('order') or js

    def get_order(self, order_id):
        return (self._request('GET', f'/portfolio/orders/{order_id}', auth=True) or {}).get('order') or {}

    def cancel_order(self, order_id):
        return self._request('DELETE', f'/portfolio/orders/{order_id}', auth=True, retries=1)

    def fills(self, order_id=None, ticker=None):
        params = {k: v for k, v in (('order_id', order_id), ('ticker', ticker), ('limit', 200)) if v}
        return (self._request('GET', '/portfolio/fills', params=params, auth=True) or {}).get('fills') or []

    def find_order_by_client_id(self, ticker, client_order_id):
        js = self._request('GET', '/portfolio/orders', params={'ticker': ticker, 'limit': 200}, auth=True)
        for o in js.get('orders') or []:
            if o.get('client_order_id') == client_order_id: return o
        return None

    def positions(self):
        return (self._request('GET', '/portfolio/positions', auth=True) or {}).get('market_positions') or []


def fill_summary(order, fills=None):
    """Normalise a Kalshi order (+ optional fills) to {filled, avg_price, fees, status}. Prices in dollars."""
    filled = None
    for k in ('fill_count', 'taker_fill_count', 'filled_count'):
        if order.get(k) is not None: filled = float(order[k]); break
    cost = fees = 0.0; n = 0.0
    for f in fills or []:
        c = float(f.get('count') or f.get('count_fp') or 0)
        side = f.get('side') or order.get('side')
        p = _price(f.get('yes_price') if side == 'yes' else f.get('no_price'))
        if p is None: p = _price(f.get('price'))
        if c and p is not None: cost += c * p; n += c
        fees += float(f.get('fee_cost') or f.get('fee') or 0.0)
    if fills and n:
        filled = n if filled is None else max(filled, n)
        avg = cost / n
    else:
        px = _price(order.get('yes_price') if order.get('side') == 'yes' else order.get('no_price'))
        avg = px
    fees = fees or float(order.get('taker_fees', 0) or 0) / (100.0 if isinstance(order.get('taker_fees'), int) else 1.0)
    return {'filled': filled or 0.0, 'avg_price': avg, 'fees': fees, 'status': order.get('status'), 'estimated': not bool(fills)}
