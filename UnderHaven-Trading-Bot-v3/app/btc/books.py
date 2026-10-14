"""Order-book observations with timestamps. Prices/sizes are Decimal. Observations older than the configured maximum are rejected downstream."""
import time
from dataclasses import dataclass, field
from decimal import Decimal
from typing import List, Tuple
from .calc import d

Lv = List[Tuple[Decimal, Decimal]]


@dataclass
class Book:
    asks: Lv = field(default_factory=list)     # best (lowest) first
    bids: Lv = field(default_factory=list)     # best (highest) first

    @property
    def best_ask(self): return self.asks[0][0] if self.asks else None
    @property
    def best_bid(self): return self.bids[0][0] if self.bids else None
    @property
    def spread(self): return (self.best_ask - self.best_bid) if self.asks and self.bids else None

    def valid(self):
        """Reject crossed or out-of-range books and non-monotonic ladders."""
        if self.asks and self.bids and self.best_bid >= self.best_ask: return False
        for lv, rev in ((self.asks, False), (self.bids, True)):
            ps = [p for p, _ in lv]
            if any(p < 0 or p > 1 for p in ps): return False
            if ps != sorted(ps, reverse=rev): return False
        return True

    def depth(self, side='asks', top=3): return sum((s for _, s in (self.asks if side == 'asks' else self.bids)[:top]), Decimal(0))


@dataclass
class MarketObs:
    market: object
    ts: float
    up: Book
    down: Book
    source: str = ''

    def book(self, side): return self.up if side == 'UP' else self.down
    def age(self, now=None): return (now or time.time()) - self.ts


def _lv(levels): return [(d(p), d(s)) for p, s in levels if d(s) > 0]


def poly_obs(market, up_raw, down_raw, levels, ts=None):
    """levels(raw_book, 'asks'|'bids') -> [(price, size)]  (PolymarketService._levels)."""
    return MarketObs(market, ts or time.time(), Book(_lv(levels(up_raw, 'asks')), _lv(levels(up_raw, 'bids'))),
                     Book(_lv(levels(down_raw, 'asks')), _lv(levels(down_raw, 'bids'))), 'polymarket-clob')


def kalshi_obs(market, parsed, ts=None):
    return MarketObs(market, ts or time.time(), Book(_lv(parsed['yes_asks']), _lv(parsed['yes_bids'])), Book(_lv(parsed['no_asks']), _lv(parsed['no_bids'])), 'kalshi-orderbook')
