```python
import json
import re
import time
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

        # BTC 15-minute market discovery cache.
        #
        # The scanner can run every few seconds, but there is no reason to
        # repeatedly query Gamma for the same market list. A 10-second cache
        # also gives us enough coverage when one 15-minute market rolls into
        # the next.
        self._market_cache = []
        self._market_cache_at = 0.0

    def creds(self):
        with db.connect() as c:
            r = c.execute(
                'SELECT * FROM credentials WHERE id=1'
            ).fetchone()

        return dict(r) if r and r['private_key'] else None

    def save_connection(
        self,
        private_key,
        funder=None,
        signature_type=0,
        seed_phrase=None
    ):
        from eth_account import Account
        from py_clob_client_v2 import ClobClient

        acct = Account.from_key(private_key)
        signer = acct.address

        client = ClobClient(
            host=CLOB_URL,
            chain_id=CHAIN_ID,
            key=private_key,
            signature_type=int(signature_type),
            funder=funder or signer
        )

        creds = client.create_or_derive_api_key()

        api_key = (
            getattr(creds, 'api_key', None)
            or getattr(creds, 'key', None)
            or creds.get('apiKey')
        )

        api_secret = (
            getattr(creds, 'api_secret', None)
            or getattr(creds, 'secret', None)
            or creds.get('secret')
        )

        api_passphrase = (
            getattr(creds, 'api_passphrase', None)
            or getattr(creds, 'passphrase', None)
            or creds.get('passphrase')
        )

        with db.connect() as c:
            c.execute('DELETE FROM credentials')

            c.execute(
                '''INSERT INTO credentials(
                    id,
                    private_key,
                    seed_phrase,
                    api_key,
                    api_secret,
                    api_passphrase,
                    funder,
                    signer,
                    signature_type
                )
                VALUES(1,?,?,?,?,?,?,?,?)''',
                (
                    self.vault.encrypt(private_key),
                    self.vault.encrypt(seed_phrase)
                    if seed_phrase else None,
                    self.vault.encrypt(api_key),
                    self.vault.encrypt(api_secret),
                    self.vault.encrypt(api_passphrase),
                    funder or signer,
                    signer,
                    int(signature_type)
                )
            )

        return {
            'signer': signer,
            'funder': funder or signer,
            'api_key': api_key,
            'signature_type': int(signature_type)
        }

    def generate_wallet(self):
        from mnemonic import Mnemonic
        from eth_account import Account

        mn = Mnemonic('english')
        phrase = mn.generate(strength=128)

        Account.enable_unaudited_hdwallet_features()

        acct = Account.from_mnemonic(phrase)

        return {
            'address': acct.address,
            'private_key': acct.key.hex(),
            'seed_phrase': phrase
        }

    def client(self):
        r = self.creds()

        if not r:
            return None

        from py_clob_client_v2 import ClobClient, ApiCreds

        creds = ApiCreds(
            api_key=self.vault.decrypt(r['api_key']),
            api_secret=self.vault.decrypt(r['api_secret']),
            api_passphrase=self.vault.decrypt(r['api_passphrase'])
        )

        return ClobClient(
            host=CLOB_URL,
            chain_id=CHAIN_ID,
            key=self.vault.decrypt(r['private_key']),
            creds=creds,
            signature_type=int(r['signature_type']),
            funder=r['funder']
        )

    def balance(self):
        from py_clob_client_v2 import BalanceAllowanceParams, AssetType

        client = self.client()

        if not client:
            return None

        return client.get_balance_allowance(
            BalanceAllowanceParams(
                asset_type=AssetType.COLLATERAL
            )
        )

    def markets(self):
        """
        Discover currently active BTC 15-minute Up/Down markets.

        Polymarket's generic /markets endpoint does not reliably expose the
        current short-duration BTC series in the first page of results.

        The BTC 15-minute markets use predictable event slugs:

            btc-updown-15m-<15-minute Unix timestamp>

        We therefore query Gamma's /events endpoint directly for a small
        rolling window around the current 15-minute interval.

        Previous slot:
            Helps during transitions.

        Current slot:
            The market currently trading.

        Next eight slots:
            Allows the scanner to see upcoming markets before they become
            the active trading window.
        """

        now = int(time.time())

        # Round down to the beginning of the current 15-minute period.
        base = now - (now % 900)

        # Use cached discovery results for 10 seconds.
        if (
            self._market_cache
            and time.time() - self._market_cache_at < 10
        ):
            return self._market_cache

        found = []
        seen = set()

        # Previous slot + current slot + next 8 slots.
        #
        # 900 seconds = 15 minutes.
        for offset in range(-1, 9):
            timestamp = base + (offset * 900)

            slug = f'btc-updown-15m-{timestamp}'

            try:
                response = requests.get(
                    GAMMA_URL + '/events',
                    params={
                        'slug': slug
                    },
                    timeout=10
                )

                response.raise_for_status()

                events = response.json()

                if not isinstance(events, list):
                    continue

            except Exception as exc:
                self.last_error = (
                    f'Gamma discovery {slug}: {exc}'
                )
                continue

            for event in events:
                if not isinstance(event, dict):
                    continue

                for market in event.get('markets') or []:
                    if not isinstance(market, dict):
                        continue

                    # Copy the market so we can normalize fields without
                    # modifying Gamma's original response object.
                    market = dict(market)

                    market_slug = str(
                        market.get('slug') or ''
                    )

                    if not market_slug:
                        continue

                    if market_slug in seen:
                        continue

                    # Confirm this is actually a BTC 15-minute market.
                    if not self.is_btc_15m(market):
                        continue

                    # CLOB order book must be enabled.
                    if market.get('enableOrderBook') is False:
                        continue

                    # Market must currently be active.
                    if market.get('active') is not True:
                        continue

                    # Do not scan closed markets.
                    if market.get('closed') is True:
                        continue

                    # Do not scan markets that have stopped accepting orders.
                    if market.get('acceptingOrders') is False:
                        continue

                    # Normalize CLOB token IDs.
                    tokens = market.get('clobTokenIds') or []

                    if isinstance(tokens, str):
                        try:
                            tokens = json.loads(tokens)
                        except Exception:
                            continue

                    if not isinstance(tokens, list):
                        continue

                    # BTC Up/Down requires at least YES and NO tokens.
                    if len(tokens) < 2:
                        continue

                    market['clobTokenIds'] = tokens

                    found.append(market)
                    seen.add(market_slug)

        # Only replace the cache after a successful discovery.
        #
        # This prevents a temporary Gamma/API failure from immediately
        # destroying the scanner's last known market list.
        if found:
            self._market_cache = found
            self._market_cache_at = time.time()

        return self._market_cache

    def orderbook(self, token):
        public = None

        client = self.client()

        if client:
            public = client.get_order_book(token)
        else:
            from py_clob_client_v2 import ClobClient

            public = ClobClient(
                host=CLOB_URL,
                chain_id=CHAIN_ID
            ).get_order_book(token)

        return public

    @staticmethod
    def _levels(book, side):
        values = (
            getattr(book, side, None)
            if not isinstance(book, dict)
            else book.get(side)
        )

        if values is None:
            return []

        out = []

        for item in values:
            if isinstance(item, dict):
                p = item.get('price')
                s = item.get('size')
            else:
                p = getattr(item, 'price', None)
                s = getattr(item, 'size', None)

            try:
                if (
                    p is not None
                    and s is not None
                    and float(s) > 0
                ):
                    out.append(
                        (
                            float(p),
                            float(s)
                        )
                    )

            except (TypeError, ValueError):
                continue

        out.sort(
            key=lambda x: x[0],
            reverse=(side == 'bids')
        )

        return out

    @staticmethod
    def quote_buy(levels, shares):
        remaining = float(shares)
        cost = 0.0
        filled = 0.0

        for price, size in levels:
            take = min(remaining, size)

            cost += take * price
            filled += take
            remaining -= take

            if remaining <= 1e-12:
                break

        if filled + 1e-9 < shares:
            return None

        return (
            cost,
            cost / filled if filled else None
        )

    @staticmethod
    def fee_for(shares, price, rate):
        # Protocol formula:
        #
        # C * feeRate * p * (1-p)
        #
        # Rounded to 5 decimals per fill.
        fee = (
            shares
            * rate
            * price
            * (1.0 - price)
        )

        return round(fee, 5)

    @staticmethod
    def market_fee_rate(market):
        # If Gamma exposes an explicit fee parameter, prefer it.
        for key in (
            'feeRate',
            'fee_rate',
            'takerFeeRate',
            'feeRateBps'
        ):
            value = market.get(key)

            if value is None:
                continue

            try:
                f = float(value)

                return (
                    f / 10000.0
                    if 'bps' in key.lower() or f > 1
                    else f
                )

            except (TypeError, ValueError):
                pass

        tags = str(
            market.get('category', '')
        ).lower()

        if 'sport' in tags:
            return DEFAULT_FEE_RATES['sports']

        if 'finance' in tags:
            return DEFAULT_FEE_RATES['finance']

        if 'politic' in tags:
            return DEFAULT_FEE_RATES['politics']

        if 'geopolit' in tags:
            return DEFAULT_FEE_RATES['geopolitics']

        return DEFAULT_FEE_RATES['crypto']

    @staticmethod
    def is_btc_15m(m):
        q = str(
            m.get('question', '')
        )

        slug = str(
            m.get('slug', '')
        ).lower()

        text = (
            q
            + ' '
            + slug
        ).lower()

        # Must contain Bitcoin or BTC.
        if (
            'bitcoin' not in text
            and re.search(r'\bbtc\b', text) is None
        ):
            return False

        # Must be an Up/Down market.
        if not any(
            x in text
            for x in (
                'up or down',
                'up/down',
                'updown'
            )
        ):
            return False

        # Must be 15-minute.
        if not any(
            x in text
            for x in (
                '15m',
                '15-min',
                '15 minute',
                '15-minute'
            )
        ):
            return False

        # Explicitly reject 5-minute markets.
        if any(
            x in text
            for x in (
                '5m',
                '5-min',
                '5 minute',
                '5-minute'
            )
        ):
            return False

        return True

    def scan(
        self,
        min_net_edge=0.01,
        trade_size=10.0,
        fee_mode='taker'
    ):
        self.last_error = ''

        markets = self.markets()
        out = []

        for m in markets:
            if (
                not self.is_btc_15m(m)
                or m.get('enableOrderBook') is False
            ):
                continue

            try:
                tokens = m.get(
                    'clobTokenIds',
                    []
                )

                if isinstance(tokens, str):
                    tokens = json.loads(tokens)

                if len(tokens) < 2:
                    continue

                yes_book = self.orderbook(tokens[0])
                no_book = self.orderbook(tokens[1])

                yes_asks = self._levels(
                    yes_book,
                    'asks'
                )

                no_asks = self._levels(
                    no_book,
                    'asks'
                )

                if not yes_asks or not no_asks:
                    continue

                yes_ask = yes_asks[0][0]
                no_ask = no_asks[0][0]

                pair = yes_ask + no_ask

                gross = 1.0 - pair

                fee_rate = self.market_fee_rate(m)

                # Estimate executable shares under the configured spend cap,
                # including a fee reserve.
                raw_target = max(
                    0.01,
                    trade_size
                    / max(
                        pair
                        + (
                            2
                            * fee_rate
                            * 0.25
                        ),
                        0.01
                    )
                )

                max_shares = min(
                    sum(
                        size
                        for _, size in yes_asks
                    ),
                    sum(
                        size
                        for _, size in no_asks
                    ),
                    raw_target
                )

                # Walk both books at the same share count.
                #
                # If the spend cap is exceeded, shrink the position.
                shares = max_shares

                for _ in range(8):
                    yes_quote = self.quote_buy(
                        yes_asks,
                        shares
                    )

                    no_quote = self.quote_buy(
                        no_asks,
                        shares
                    )

                    if not yes_quote or not no_quote:
                        shares *= 0.8
                        continue

                    spend = (
                        yes_quote[0]
                        + no_quote[0]
                    )

                    if spend <= trade_size * 1.000001:
                        break

                    shares *= (
                        trade_size / spend
                    ) * 0.98

                yes_quote = self.quote_buy(
                    yes_asks,
                    shares
                )

                no_quote = self.quote_buy(
                    no_asks,
                    shares
                )

                if not yes_quote or not no_quote:
                    continue

                yes_cost, yes_vwap = yes_quote
                no_cost, no_vwap = no_quote

                gross_dollars = (
                    shares
                    - (
                        yes_cost
                        + no_cost
                    )
                )

                if fee_mode == 'maker':
                    fee = 0.0
                else:
                    fee = (
                        self.fee_for(
                            shares,
                            yes_vwap,
                            fee_rate
                        )
                        + self.fee_for(
                            shares,
                            no_vwap,
                            fee_rate
                        )
                    )

                net_dollars = (
                    gross_dollars
                    - fee
                )

                net_edge = (
                    net_dollars / shares
                    if shares
                    else -1
                )

                qualified = (
                    net_edge >= min_net_edge
                    and shares > 0
                )

                minimum_order_size = 0.0

                minimum_order_value = str(
                    m.get(
                        'minimumOrderSize',
                        ''
                    )
                )

                if minimum_order_value.replace(
                    '.',
                    '',
                    1
                ).isdigit():
                    minimum_order_size = float(
                        minimum_order_value
                    )

                out.append({
                    'question': str(
                        m.get('question', '')
                    ),

                    'slug': str(
                        m.get('slug', '')
                    ),

                    'yes_ask': yes_ask,
                    'no_ask': no_ask,

                    'pair_cost': pair,

                    'gross_edge': gross,

                    'fee_estimate': (
                        fee / shares
                        if shares
                        else 0
                    ),

                    'net_edge': net_edge,

                    'net_profit': net_dollars,

                    'executable_shares': shares,

                    'executable_spend': (
                        yes_cost
                        + no_cost
                    ),

                    'liquidity_yes': sum(
                        size
                        for _, size in yes_asks
                    ),

                    'liquidity_no': sum(
                        size
                        for _, size in no_asks
                    ),

                    'condition_id': m.get(
                        'conditionId'
                    ),

                    'market_id': m.get(
                        'id'
                    ),

                    'tokens': tokens,

                    'fee_rate': fee_rate,

                    'min_order_size': (
                        minimum_order_size
                    ),

                    'qualified': qualified,

                    'neg_risk': bool(
                        m.get('negRisk')
                    ),
                })

            except Exception as e:
                self.last_error = str(e)

        out.sort(
            key=lambda x: x['net_edge'],
            reverse=True
        )

        return out
```
