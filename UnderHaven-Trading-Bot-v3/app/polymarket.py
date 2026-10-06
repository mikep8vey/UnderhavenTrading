import json, re, time, threading
from concurrent.futures import ThreadPoolExecutor, as_completed, TimeoutError as FutTimeout
from decimal import Decimal
import requests
from requests.adapters import HTTPAdapter
from .config import CLOB_URL, GAMMA_URL, CHAIN_ID
from .assets import ASSETS
from .vault import Vault
from . import db

# Current protocol fee rates documented by Polymarket. The market API is still
# treated as the source of truth when a per-market fee parameter is available.
DEFAULT_FEE_RATES = {
    'crypto': 0.07,
    'sports': 0.05,
    'finance': 0.04,
    'politics': 0.04,
    'economics': 0.05,
    'culture': 0.05,
    'weather': 0.05,
    'tech': 0.04,
    'mentions': 0.04,
    'other': 0.05,
    'geopolitics': 0.0,
}

WINDOW = 900  # 15-minute markets
HTTP_TIMEOUT = (3, 6)  # connect, read: a slow API must never stall the scanner for a minute

class PolymarketService:
    def __init__(self):
        self.vault = Vault()
        self.last_error = ''
        self._market_cache = []
        self._market_cache_at = 0.0
        self._market_cache_key = None
        # ONE pooled HTTP session + ONE worker pool for the life of the process. The old code built a
        # brand-new SDK client for every order-book request, which leaked sockets until the OS refused
        # to open any more files ("Too many open files").
        self._http = requests.Session()
        self._http.mount('https://', HTTPAdapter(pool_connections=4, pool_maxsize=32, max_retries=0))
        self._pool = ThreadPoolExecutor(max_workers=16, thread_name_prefix='uh-fetch')
        self._public_client = None
        self._client = None
        self._client_key = None
        self._public_lock = threading.Lock()
        self._batch_off_until = 0.0
        self.books_via = ''
        self.notes = []

    def creds(self):
        with db.connect() as c:
            r = c.execute('SELECT * FROM credentials WHERE id=1').fetchone()
        return dict(r) if r and r['private_key'] else None

    def save_connection(self, private_key, funder=None, signature_type=0, seed_phrase=None):
        private_key = (private_key or '').strip()
        funder = (funder or '').strip() or None
        signature_type = int(signature_type)
        if funder and not re.fullmatch(r'0x[0-9a-fA-F]{40}', funder):
            raise ValueError('Funder must be a 0x wallet address (42 characters). Check for a missing or extra character.')
        if signature_type != 0 and not funder:
            raise ValueError('Signature types 1, 2 and 3 need a Funder: the address of your Polymarket wallet, which is NOT the address of the signing key.')
        from eth_account import Account
        from py_clob_client_v2 import ClobClient
        acct = Account.from_key(private_key)
        signer = acct.address
        client = ClobClient(
            host=CLOB_URL, chain_id=CHAIN_ID, key=private_key,
            signature_type=int(signature_type), funder=funder or signer
        )
        creds = client.create_or_derive_api_key()
        api_key = getattr(creds, 'api_key', None) or getattr(creds, 'key', None) or creds.get('apiKey')
        api_secret = getattr(creds, 'api_secret', None) or getattr(creds, 'secret', None) or creds.get('secret')
        api_passphrase = getattr(creds, 'api_passphrase', None) or getattr(creds, 'passphrase', None) or creds.get('passphrase')
        with db.connect() as c:
            c.execute('DELETE FROM credentials')
            c.execute('''INSERT INTO credentials(
                id,private_key,seed_phrase,api_key,api_secret,api_passphrase,funder,signer,signature_type
            ) VALUES(1,?,?,?,?,?,?,?,?)''', (
                self.vault.encrypt(private_key),
                self.vault.encrypt(seed_phrase) if seed_phrase else None,
                self.vault.encrypt(api_key), self.vault.encrypt(api_secret), self.vault.encrypt(api_passphrase),
                funder or signer, signer, int(signature_type)
            ))
        with self._public_lock:
            self._client = None; self._client_key = None
        self._bal_sync_at = 0.0
        return {'signer': signer, 'funder': funder or signer, 'api_key': api_key, 'signature_type': int(signature_type)}

    def update_connection(self, funder, signature_type):
        """Change funder / signature type without re-pasting the private key."""
        r = self.creds()
        if not r: raise RuntimeError('No wallet is connected yet.')
        pk = self.vault.decrypt(r['private_key'])
        seed = self.vault.decrypt(r['seed_phrase']) if r.get('seed_phrase') else None
        return self.save_connection(pk, funder, signature_type, seed)

    def generate_wallet(self):
        from mnemonic import Mnemonic
        from eth_account import Account
        mn = Mnemonic('english')
        phrase = mn.generate(strength=128)
        Account.enable_unaudited_hdwallet_features()
        acct = Account.from_mnemonic(phrase)
        return {'address': acct.address, 'private_key': acct.key.hex(), 'seed_phrase': phrase}

    def client(self):
        """Authenticated SDK client. Built once per stored credential set and reused: building a new one on
        every dashboard refresh (as before) leaked a connection pool each time."""
        r = self.creds()
        if not r:
            self._client = None; self._client_key = None
            return None
        key = (r['api_key'], r['funder'], r['signature_type'])
        with self._public_lock:
            if getattr(self, '_client', None) is not None and self._client_key == key:
                return self._client
        from py_clob_client_v2 import ClobClient, ApiCreds
        creds = ApiCreds(
            api_key=self.vault.decrypt(r['api_key']),
            api_secret=self.vault.decrypt(r['api_secret']),
            api_passphrase=self.vault.decrypt(r['api_passphrase'])
        )
        client = ClobClient(
            host=CLOB_URL, chain_id=CHAIN_ID, key=self.vault.decrypt(r['private_key']),
            creds=creds, signature_type=int(r['signature_type']), funder=r['funder']
        )
        with self._public_lock:
            self._client, self._client_key = client, key
        return client

    def balance(self):
        from py_clob_client_v2 import BalanceAllowanceParams, AssetType
        client = self.client()
        if not client: return None
        params = BalanceAllowanceParams(asset_type=AssetType.COLLATERAL)
        # The CLOB keeps a server-side balance cache per signature type. Until it is refreshed from
        # a client carrying the right signature type + funder, a funded wallet can read as 0.
        if time.time() - getattr(self, '_bal_sync_at', 0.0) > 300:
            self._bal_sync_at = time.time()
            try: client.update_balance_allowance(params)
            except Exception as exc: self.last_error = f'BALANCE SYNC: {exc}'
        return client.get_balance_allowance(params)

    def note(self, text):
        """Short operational notes shown on the dashboard (never counted as scan errors)."""
        self.notes = (self.notes + [f"{time.strftime('%H:%M:%S', time.gmtime())}Z {text}"])[-5:]

    # ------------------------------------------------------------------ discovery
    def _gamma_event(self, slug):
        r = self._http.get(GAMMA_URL + '/events', params={'slug': slug}, timeout=HTTP_TIMEOUT)
        r.raise_for_status()
        data = r.json()
        return data if isinstance(data, list) else []

    @staticmethod
    def _normalise_market(market, coin, start_ts, seen):
        """Keep only live, tradable markets that carry two CLOB tokens."""
        slug = str(market.get('slug') or '')
        if not slug or slug in seen: return None
        if market.get('enableOrderBook') is False: return None
        if market.get('active') is not True or market.get('closed') is True: return None
        if market.get('acceptingOrders') is False: return None
        toks = market.get('clobTokenIds') or []
        if isinstance(toks, str):
            try: toks = json.loads(toks)
            except Exception: return None
        if not isinstance(toks, list) or len(toks) < 2: return None
        market['clobTokenIds'] = [str(t) for t in toks]
        market['asset'] = coin['code']
        market['start_ts'] = start_ts
        seen.add(slug)
        return market

    def markets(self, assets=None, slots_ahead=2):
        """Discover '<coin>-updown-15m-<start>' markets: the previous window, the live one and
        `slots_ahead` upcoming ones, for every enabled coin. Requests run in parallel over one
        pooled session; results are cached for 10 s and a failed lookup keeps its last good value."""
        now = time.time()
        coins = [c for c in ASSETS if assets is None or c['code'] in assets]
        key = (tuple(c['code'] for c in coins), int(slots_ahead))
        if self._market_cache and self._market_cache_key == key and now - self._market_cache_at < 10:
            return self._market_cache
        base = int(now) - int(now) % WINDOW
        jobs = {}
        for coin in coins:
            for k in range(-1, int(slots_ahead) + 1):
                ts = base + k * WINDOW
                jobs[self._pool.submit(self._gamma_event, f"{coin['slug']}-updown-15m-{ts}")] = (coin, ts)
        found, seen, failed, errors = [], set(), set(), []
        try:
            for fut in as_completed(jobs, timeout=12):
                coin, ts = jobs[fut]
                try:
                    events = fut.result()
                except Exception as exc:
                    failed.add((coin['code'], ts)); errors.append(f"Gamma discovery {coin['slug']}-updown-15m-{ts}: {exc}"); continue
                for ev in events:
                    for market in ev.get('markets') or []:
                        m = self._normalise_market(dict(market), coin, ts, seen)
                        if m: found.append(m)
        except FutTimeout:
            for fut, (coin, ts) in jobs.items():
                if not fut.done(): failed.add((coin['code'], ts))
            errors.append('Gamma discovery timed out for some markets')
        if errors:
            self.last_error = errors[0] + (f' (+{len(errors) - 1} more)' if len(errors) > 1 else '')
        # A lookup that failed this round keeps its previous result (if it is still current).
        if failed and self._market_cache_key == key:
            have = {m['slug'] for m in found}
            found += [m for m in self._market_cache
                      if (m['asset'], m['start_ts']) in failed and m['slug'] not in have and m['start_ts'] + WINDOW > now]
        if found or not self._market_cache or self._market_cache_key != key:
            self._market_cache, self._market_cache_key, self._market_cache_at = found, key, now
        return self._market_cache

    # ------------------------------------------------------------------ order books
    @staticmethod
    def _book_dict(js):
        if not isinstance(js, dict) or ('asks' not in js and 'bids' not in js):
            raise ValueError('unexpected order-book format')
        return js

    def _sdk_book(self, token):
        """Last-resort path through the official SDK, using ONE shared client."""
        with self._public_lock:
            if self._public_client is None:
                from py_clob_client_v2 import ClobClient
                self._public_client = ClobClient(host=CLOB_URL, chain_id=CHAIN_ID)
            client = self._public_client
        try:
            return client.get_order_book(token)
        except Exception as exc:
            if '404' in str(exc) or 'No orderbook' in str(exc): return None
            raise

    def _one_book(self, token):
        r = self._http.get(CLOB_URL + '/book', params={'token_id': token}, timeout=HTTP_TIMEOUT)
        if r.status_code == 404: return None   # the market exists but its book has not opened yet
        r.raise_for_status()
        try:
            return self._book_dict(r.json())
        except ValueError:
            return self._sdk_book(token)

    def books(self, tokens):
        """{token_id: book | None}. None means "no order book yet", which is normal for a window that
        has not opened and is NOT an error. One batched request where possible, so the Up and Down
        books of a market come from the same moment; parallel single requests otherwise."""
        tokens = list(dict.fromkeys(str(t) for t in tokens)); out = {}
        self.book_errors = []
        if not tokens: return out
        if time.time() >= self._batch_off_until:
            try:
                r = self._http.post(CLOB_URL + '/books', json=[{'token_id': t} for t in tokens], timeout=HTTP_TIMEOUT)
                r.raise_for_status(); data = r.json()
                if not isinstance(data, list): raise ValueError('unexpected /books format')
                for b in data:
                    if isinstance(b, dict) and b.get('asset_id') is not None:
                        out[str(b['asset_id'])] = self._book_dict(b)
                for t in tokens: out.setdefault(t, None)
                self.books_via = 'batch'
                return out
            except Exception as exc:
                self._batch_off_until = time.time() + 300
                self.note(f'batch /books unavailable ({str(exc)[:70]}); using parallel requests for 5 min')
        futs = {self._pool.submit(self._one_book, t): t for t in tokens}
        try:
            for fut in as_completed(futs, timeout=10):
                t = futs[fut]
                try: out[t] = fut.result()
                except Exception as exc: self.book_errors.append(f'order book {t[:10]}…: {exc}')
        except FutTimeout:
            self.book_errors.append('order-book requests timed out')
        self.books_via = 'parallel'
        return out

    @staticmethod
    def _levels(book, side):
        values = getattr(book, side, None) if not isinstance(book, dict) else book.get(side)
        if values is None: return []
        out = []
        for item in values:
            if isinstance(item, dict):
                p, s = item.get('price'), item.get('size')
            else:
                p, s = getattr(item, 'price', None), getattr(item, 'size', None)
            try:
                if p is not None and s is not None and float(s) > 0:
                    out.append((float(p), float(s)))
            except (TypeError, ValueError):
                continue
        out.sort(key=lambda x: x[0], reverse=(side == 'bids'))
        return out

    @staticmethod
    def quote_buy(levels, shares):
        remaining = float(shares); cost = 0.0; filled = 0.0
        for price, size in levels:
            take = min(remaining, size)
            cost += take * price; filled += take; remaining -= take
            if remaining <= 1e-12: break
        if filled + 1e-9 < shares:
            return None
        return cost, cost / filled if filled else None

    @staticmethod
    def fee_for(shares, price, rate):
        # Protocol formula: C * feeRate * p * (1-p). Rounded to 5 decimals per fill.
        fee = shares * rate * price * (1.0 - price)
        return round(fee, 5)

    @staticmethod
    def market_fee_rate(market):
        # If Gamma exposes an explicit fee parameter, prefer it.
        for key in ('feeRate', 'fee_rate', 'takerFeeRate', 'feeRateBps'):
            value = market.get(key)
            if value is None: continue
            try:
                f = float(value)
                return f / 10000.0 if 'bps' in key.lower() or f > 1 else f
            except (TypeError, ValueError):
                pass
        tags = str(market.get('category','')).lower()
        if 'sport' in tags: return DEFAULT_FEE_RATES['sports']
        if 'finance' in tags: return DEFAULT_FEE_RATES['finance']
        if 'politic' in tags: return DEFAULT_FEE_RATES['politics']
        if 'geopolit' in tags: return DEFAULT_FEE_RATES['geopolitics']
        return DEFAULT_FEE_RATES['crypto']

    @staticmethod
    def is_btc_15m(m):
        q = str(m.get('question',''))
        slug = str(m.get('slug','')).lower()
        text = (q + ' ' + slug).lower()
        if 'bitcoin' not in text and re.search(r'\bbtc\b', text) is None: return False
        if not any(x in text for x in ('up or down','up/down','updown')): return False
        if not any(x in text for x in ('15m','15-min','15 minute','15-minute')): return False
        # Exclude 5-minute markets. The digit lookbehind matters: a plain substring
        # test for '5m' also matches inside '15m' and rejected every 15-minute market.
        if re.search(r'(?<!\d)5[\s-]?(m|min|minute)s?\b', text): return False
        return True

    @staticmethod
    def _worst_price(levels, shares):
        remaining = float(shares); worst = levels[0][0]
        for price, size in levels:
            worst = price; remaining -= size
            if remaining <= 1e-9: break
        return worst

    def evaluate(self, m, yb, nb, min_net_edge, trade_size, fee_mode='taker', slip_ticks=0.0, late_skip=0, now=None):
        """Price one Up/Down pair from two order books. Returns a dict, or None if either side has no asks.
        raw_qualified = net edge clears your minimum. qualified = raw AND every guard passes."""
        now = now or time.time()
        yasks, nasks = self._levels(yb, 'asks'), self._levels(nb, 'asks')
        if not yasks or not nasks: return None
        yes_ask, no_ask = yasks[0][0], nasks[0][0]
        pair = yes_ask + no_ask
        gross = 1.0 - pair
        fee_rate = self.market_fee_rate(m)
        raw_target = max(0.01, trade_size / max(pair + (2 * fee_rate * 0.25), 0.01))
        shares = min(sum(s for _, s in yasks), sum(s for _, s in nasks), raw_target)
        for _ in range(8):
            yq = self.quote_buy(yasks, shares); nq = self.quote_buy(nasks, shares)
            if not yq or not nq: shares *= 0.8; continue
            spend = yq[0] + nq[0]
            if spend <= trade_size * 1.000001: break
            shares *= (trade_size / spend) * 0.98
        yq = self.quote_buy(yasks, shares); nq = self.quote_buy(nasks, shares)
        if not yq or not nq: return None
        yes_cost, yes_vwap = yq; no_cost, no_vwap = nq
        def _num(v, d):
            try: return float(v)
            except (TypeError, ValueError): return d
        bk = (lambda b, k: b.get(k) if isinstance(b, dict) else getattr(b, k, None))
        tick = _num(bk(yb, 'tick_size'), 0.01) or 0.01
        min_order = _num(bk(yb, 'min_order_size'), 0.0) or _num(m.get('orderMinSize'), 0.0) or _num(m.get('minimumOrderSize'), 0.0)
        y_worst, n_worst = self._worst_price(yasks, shares), self._worst_price(nasks, shares)
        gross_dollars = shares - (yes_cost + no_cost)
        fee = 0.0 if fee_mode == 'maker' else self.fee_for(shares, yes_vwap, fee_rate) + self.fee_for(shares, no_vwap, fee_rate)
        slip = shares * 2 * slip_ticks * tick           # pessimism: give up N ticks on each leg
        net_dollars = gross_dollars - fee - slip
        net_edge = net_dollars / shares if shares else -1
        start = int(m.get('start_ts') or 0)
        secs_left = (start + WINDOW - now) if start else None
        # Profit lock: even if every share fills at the WORST price touched, fees included, is it still a win?
        locked_net = shares * (1.0 - y_worst - n_worst)
        if fee_mode != 'maker': locked_net -= self.fee_for(shares, y_worst, fee_rate) + self.fee_for(shares, n_worst, fee_rate)
        raw = net_edge >= min_net_edge and shares > 0
        reason = ''
        if raw:
            if locked_net <= 0: reason = 'not profitable at worst fill price'
            elif secs_left is not None and 0 <= secs_left < late_skip: reason = 'too late in window'
            elif min_order and shares + 1e-9 < min_order: reason = 'below minimum order size'
        return {
            'asset': m.get('asset', 'BTC'), 'start_ts': start, 'secs_left': secs_left,
            'question': str(m.get('question', '')), 'slug': str(m.get('slug', '')),
            'yes_ask': yes_ask, 'no_ask': no_ask, 'pair_cost': pair,
            'gross_edge': gross, 'fee_estimate': (fee + slip) / shares if shares else 0,
            'net_edge': net_edge, 'net_profit': net_dollars, 'executable_shares': shares,
            'executable_spend': yes_cost + no_cost, 'liquidity_yes': sum(s for _, s in yasks),
            'liquidity_no': sum(s for _, s in nasks), 'condition_id': m.get('conditionId'),
            'market_id': m.get('id'), 'tokens': list(m.get('clobTokenIds', []))[:2], 'fee_rate': fee_rate,
            'tick_size': tick, 'min_order_size': min_order, 'neg_risk': bool(m.get('negRisk')),
            'raw_qualified': bool(raw), 'qualified': bool(raw and not reason), 'reason': reason,
            'confirmed': False, '_m': m,
        }

    def scan(self, min_net_edge=0.01, trade_size=10.0, fee_mode='taker', assets=None, slots_ahead=2, slip_ticks=0.0, late_skip=0):
        self.last_error = ''
        markets = [m for m in self.markets(assets, slots_ahead) if m['start_ts'] + WINDOW > time.time()]
        tokens = [t for m in markets for t in m['clobTokenIds'][:2]]
        books = self.books(tokens)
        if self.book_errors:
            self.last_error = self.book_errors[0] + (f' (+{len(self.book_errors) - 1} more)' if len(self.book_errors) > 1 else '')
        out = []
        for m in markets:
            yb, nb = books.get(m['clobTokenIds'][0]), books.get(m['clobTokenIds'][1])
            if yb is None or nb is None: continue   # no book yet (or fetch failed): skip quietly
            try:
                o = self.evaluate(m, yb, nb, min_net_edge, trade_size, fee_mode, slip_ticks, late_skip)
                if o: out.append(o)
            except Exception as e:
                self.last_error = f"{m.get('slug', '?')}: {e}"
        out.sort(key=lambda x: x['net_edge'], reverse=True)
        return out

    def requote(self, o, min_net_edge, trade_size, fee_mode='taker', slip_ticks=0.0, late_skip=0):
        """Re-fetch both books for ONE opportunity and re-price it at the same size. A real gap is still
        there a moment later; a phantom (stale or out-of-sync book) is not. Returns the fresh dict or None."""
        y, n = o['tokens'][0], o['tokens'][1]
        books = self.books([y, n])
        yb, nb = books.get(y), books.get(n)
        if yb is None or nb is None: return None
        return self.evaluate(o['_m'], yb, nb, min_net_edge, trade_size, fee_mode, slip_ticks, late_skip)
