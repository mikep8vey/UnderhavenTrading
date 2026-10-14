"""BTC reference price feed (public spot tickers). This is the bot's OWN reference for momentum / volatility; it is NOT the settlement
source of either venue (Polymarket and Kalshi each settle on their own index), so a signal near the strike always carries basis risk."""
import math, threading, time
from collections import deque
import requests

SOURCES = [
    ('coinbase', 'https://api.exchange.coinbase.com/products/BTC-USD/ticker', lambda j: float(j['price'])),
    ('kraken', 'https://api.kraken.com/0/public/Ticker?pair=XBTUSD', lambda j: float(next(iter(j['result'].values()))['c'][0])),
    ('binance', 'https://api.binance.com/api/v3/ticker/price?symbol=BTCUSDT', lambda j: float(j['price'])),
]


class PriceFeed:
    def __init__(self, sources=None, session=None, now=time.time, maxlen=7200):
        self.sources = sources or SOURCES
        self.http = session or requests.Session()
        self.now = now
        self.history = deque(maxlen=maxlen)       # (ts, price, source)
        self.last_error = ''
        self.errors = 0
        self._stop = threading.Event(); self._thread = None

    def poll_once(self):
        for name, url, parse in self.sources:
            try:
                r = self.http.get(url, timeout=(2, 4)); r.raise_for_status()
                px = parse(r.json())
                if not (px > 1000): raise ValueError(f'implausible price {px}')
                t = self.now(); self.history.append((t, px, name)); self.last_error = ''
                return px, name, t
            except Exception as exc:
                self.last_error = f'{name}: {type(exc).__name__}'; self.errors += 1
        return None

    def latest(self):
        return self.history[-1] if self.history else None

    def age(self):
        l = self.latest(); return (self.now() - l[0]) if l else None

    def price_at(self, ts, tol=20.0):
        """Last sample at or before ts (within `tol` seconds), else None. Never extrapolates or invents a start price."""
        best = None
        for t, px, _ in self.history:
            if t <= ts: best = (t, px)
            else: break
        return best[1] if best and ts - best[0] <= tol else None

    def sigma(self, lookback=300.0, min_samples=20):
        """Per-sqrt(second) volatility of log returns from the recorded samples (irregular spacing handled)."""
        cut = self.now() - lookback
        pts = [(t, px) for t, px, _ in self.history if t >= cut]
        if len(pts) < min_samples: return None
        num = den = 0.0
        for (t0, p0), (t1, p1) in zip(pts, pts[1:]):
            dt = t1 - t0
            if dt <= 0: continue
            r = math.log(p1 / p0); num += r * r; den += dt
        return math.sqrt(num / den) if den > 0 else None

    def ret(self, seconds):
        l = self.latest()
        if not l: return None
        p0 = self.price_at(l[0] - seconds, tol=max(10.0, seconds * 0.5))
        return math.log(l[1] / p0) if p0 else None

    def start(self, interval=1.0):
        if self._thread and self._thread.is_alive(): return
        self._stop.clear()
        def loop():
            while not self._stop.is_set():
                t0 = time.time(); self.poll_once()
                self._stop.wait(max(0.2, interval - (time.time() - t0)))
        self._thread = threading.Thread(target=loop, daemon=True, name='uh-btcfeed'); self._thread.start()

    def stop(self): self._stop.set()
