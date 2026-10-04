import json, re, time
from decimal import Decimal
import requests
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

class PolymarketService:
    def __init__(self):
        self.vault = Vault()
        self.last_error = ''
        self._market_cache = []
        self._market_cache_at = 0.0

    def creds(self):
        with db.connect() as c:
            r = c.execute('SELECT * FROM credentials WHERE id=1').fetchone()
        return dict(r) if r and r['private_key'] else None

    def save_connection(self, private_key, funder=None, signature_type=0, seed_phrase=None):
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
        return {'signer': signer, 'funder': funder or signer, 'api_key': api_key, 'signature_type': int(signature_type)}

    def generate_wallet(self):
        from mnemonic import Mnemonic
        from eth_account import Account
        mn = Mnemonic('english')
        phrase = mn.generate(strength=128)
        Account.enable_unaudited_hdwallet_features()
        acct = Account.from_mnemonic(phrase)
        return {'address': acct.address, 'private_key': acct.key.hex(), 'seed_phrase': phrase}

    def client(self):
        r = self.creds()
        if not r: return None
        from py_clob_client_v2 import ClobClient, ApiCreds
        creds = ApiCreds(
            api_key=self.vault.decrypt(r['api_key']),
            api_secret=self.vault.decrypt(r['api_secret']),
            api_passphrase=self.vault.decrypt(r['api_passphrase'])
        )
        return ClobClient(
            host=CLOB_URL, chain_id=CHAIN_ID, key=self.vault.decrypt(r['private_key']),
            creds=creds, signature_type=int(r['signature_type']), funder=r['funder']
        )

    def balance(self):
        from py_clob_client_v2 import BalanceAllowanceParams, AssetType
        client = self.client()
        if not client: return None
        return client.get_balance_allowance(BalanceAllowanceParams(asset_type=AssetType.COLLATERAL))

    def markets(self):
        now = int(time.time())
        base = now - (now % 900)

        # Cache discovery results for 10 seconds so the scanner does not
        # repeatedly hit Gamma while scanning every few seconds.
        if (
            getattr(self, '_market_cache', None)
            and time.time() - getattr(self, '_market_cache_at', 0.0) < 10
        ):
            return self._market_cache

        found = []
        seen = set()

        # Search the previous 15-minute slot plus the current and next
        # 8 slots. This keeps discovery working across market transitions.
        for offset in range(-1, 9):
            ts = base + (offset * 900)
            slug = f'btc-updown-15m-{ts}'

            try:
                r = requests.get(
                    GAMMA_URL + '/events',
                    params={'slug': slug},
                    timeout=10
                )
                r.raise_for_status()
                events = r.json()

                if not isinstance(events, list):
                    continue

            except Exception as exc:
                self.last_error = f'Gamma discovery {slug}: {exc}'
                continue

            for event in events:
                for market in event.get('markets') or []:
                    market = dict(market)

                    mslug = str(market.get('slug') or '')

                    if not mslug or mslug in seen:
                        continue

                    # Make sure this is one of our BTC 15-minute markets.
                    if not self.is_btc_15m(market):
                        continue

                    # Must have an enabled CLOB order book.
                    if market.get('enableOrderBook') is False:
                        continue

                    # Must currently be active and not closed.
                    if market.get('active') is not True:
                        continue

                    if market.get('closed') is True:
                        continue

                    # Do not include markets that are no longer accepting orders.
                    if market.get('acceptingOrders') is False:
                        continue

                    # Normalize CLOB token IDs.
                    toks = market.get('clobTokenIds') or []

                    if isinstance(toks, str):
                        try:
                            toks = json.loads(toks)
                        except Exception:
                            continue

                    if not isinstance(toks, list) or len(toks) < 2:
                        continue

                    market['clobTokenIds'] = toks

                    found.append(market)
                    seen.add(mslug)

        # Keep the last successful discovery result during a temporary
        # Gamma/API failure so the scanner does not suddenly go empty.
        if found:
            self._market_cache = found
            self._market_cache_at = time.time()

        return getattr(self, '_market_cache', [])

    def orderbook(self, token):
        public = None
        client = self.client()
        if client:
            public = client.get_order_book(token)
        else:
            from py_clob_client_v2 import ClobClient
            public = ClobClient(host=CLOB_URL, chain_id=CHAIN_ID).get_order_book(token)
        return public

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

    def scan(self, min_net_edge=0.01, trade_size=10.0, fee_mode='taker'):
        self.last_error = ''
        markets = self.markets(); out=[]
        for m in markets:
            if not self.is_btc_15m(m) or m.get('enableOrderBook') is False: continue
            try:
                toks = m.get('clobTokenIds', [])
                if isinstance(toks, str):
                    toks = json.loads(toks)
                if len(toks) < 2: continue
                yb, nb = self.orderbook(toks[0]), self.orderbook(toks[1])
                yasks, nasks = self._levels(yb, 'asks'), self._levels(nb, 'asks')
                if not yasks or not nasks: continue
                yes_ask, no_ask = yasks[0][0], nasks[0][0]
                pair = yes_ask + no_ask
                gross = 1.0 - pair
                fee_rate = self.market_fee_rate(m)
                # Estimate executable shares under the configured spend cap, including a fee reserve.
                raw_target = max(0.01, trade_size / max(pair + (2 * fee_rate * 0.25), 0.01))
                max_shares = min(sum(s for _,s in yasks), sum(s for _,s in nasks), raw_target)
                # Walk both books at the same share count; if the spend cap is exceeded, shrink.
                shares = max_shares
                for _ in range(8):
                    yq = self.quote_buy(yasks, shares); nq = self.quote_buy(nasks, shares)
                    if not yq or not nq: shares *= 0.8; continue
                    spend = yq[0] + nq[0]
                    if spend <= trade_size * 1.000001: break
                    shares *= (trade_size / spend) * 0.98
                yq = self.quote_buy(yasks, shares); nq = self.quote_buy(nasks, shares)
                if not yq or not nq: continue
                yes_cost, yes_vwap = yq; no_cost, no_vwap = nq
                gross_dollars = shares - (yes_cost + no_cost)
                fee = 0.0 if fee_mode == 'maker' else self.fee_for(shares, yes_vwap, fee_rate) + self.fee_for(shares, no_vwap, fee_rate)
                net_dollars = gross_dollars - fee
                net_edge = net_dollars / shares if shares else -1
                qualified = net_edge >= min_net_edge and shares > 0
                out.append({
                    'question': str(m.get('question','')), 'slug': str(m.get('slug','')),
                    'yes_ask': yes_ask, 'no_ask': no_ask, 'pair_cost': pair,
                    'gross_edge': gross, 'fee_estimate': fee / shares if shares else 0,
                    'net_edge': net_edge, 'net_profit': net_dollars, 'executable_shares': shares,
                    'executable_spend': yes_cost + no_cost, 'liquidity_yes': sum(s for _,s in yasks),
                    'liquidity_no': sum(s for _,s in nasks), 'condition_id': m.get('conditionId'),
                    'market_id': m.get('id'), 'tokens': toks, 'fee_rate': fee_rate,
                    'min_order_size': float(m.get('minimumOrderSize') or 0) if str(m.get('minimumOrderSize','')).replace('.','',1).isdigit() else 0,
                    'qualified': qualified, 'neg_risk': bool(m.get('negRisk')),
                })
            except Exception as e:
                self.last_error = str(e)
        out.sort(key=lambda x: x['net_edge'], reverse=True)
        return out
