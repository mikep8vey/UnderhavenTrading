import json, re, time, threading
from concurrent.futures import ThreadPoolExecutor, as_completed, TimeoutError as FutTimeout
from decimal import Decimal
import requests
from requests.adapters import HTTPAdapter
from .config import CLOB_URL, GAMMA_URL, CHAIN_ID
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

def _retry(fn, *a, **kw):
    """One quick retry for a dropped connection ('Connection reset by peer'), which is routine on long-lived sessions."""
    for i in range(2):
        try:
            return fn(*a, **kw)
        except (requests.ConnectionError, requests.exceptions.ChunkedEncodingError):
            if i == 1: raise
            time.sleep(0.2)


WINDOW = 900  # 15-minute markets
HTTP_TIMEOUT = (3, 6)  # connect, read: a slow API must never stall the scanner for a minute

class PolymarketService:
    def __init__(self):
        self.vault = Vault()
        self.last_error = ''
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
        r = _retry(self._http.get, GAMMA_URL + '/events', params={'slug': slug}, timeout=HTTP_TIMEOUT)
        r.raise_for_status()
        data = r.json()
        return data if isinstance(data, list) else []

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
        r = _retry(self._http.get, CLOB_URL + '/book', params={'token_id': token}, timeout=HTTP_TIMEOUT)
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
                r = _retry(self._http.post, CLOB_URL + '/books', json=[{'token_id': t} for t in tokens], timeout=HTTP_TIMEOUT)
                r.raise_for_status(); data = r.json()
                if not isinstance(data, list): raise ValueError('unexpected /books format')
                for b in data:
                    if isinstance(b, dict) and b.get('asset_id') is not None:
                        out[str(b['asset_id'])] = self._book_dict(b)
                for t in tokens: out.setdefault(t, None)
                self.books_via = 'batch'
                return out
            except (requests.ConnectionError, requests.Timeout) as exc:
                self.note(f'batch /books network hiccup ({str(exc)[:50]}); using parallel requests this round')
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
