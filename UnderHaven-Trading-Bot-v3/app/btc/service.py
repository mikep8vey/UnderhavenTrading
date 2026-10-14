"""The persistent scanner. One background service (managed by systemd, independent of any browser) that discovers BTC Up/Down 5- and 15-minute
markets on both venues, reads real order books, runs the strategies, coordinates and executes (paper always; live only when explicitly enabled),
manages exits, settles from verified results and records everything."""
import fcntl, os, threading, time
from collections import Counter, deque
from concurrent.futures import ThreadPoolExecutor
from .. import db
from ..config import DATA_DIR, GAMMA_URL
from ..kalshi import KalshiClient, KalshiError
from . import settings as bs, store, strategies as st
from .books import kalshi_obs, poly_obs
from .discovery import discover_kalshi, discover_polymarket, normalize_polymarket
from .executor import Executor, PaperGateway
from .live import KalshiLiveGateway, PolyLiveGateway
from .matching import match_btc
from .models import PairInfo
from .risk import RiskManager
from .settlement import Settler

VENUES = ('polymarket', 'kalshi')


class BtcService:
    def __init__(self, pm, feed, kalshi=None, now=time.time, sleep=time.sleep, paper_sleep=time.sleep):
        self.pm, self.feed, self.now, self.sleep = pm, feed, now, sleep
        self.kalshi = kalshi or KalshiClient(env=db.get_setting('b_kalshi_env', 'prod'))
        self._S, self._S_at = bs.BtcSettings(), 0.0
        self.risk = RiskManager(self.settings, now)
        self.hist = st.MarketHistory()
        self.markets, self.obs, self.pairs = {}, {}, []
        self.kalshi_series, self._disc_at, self._kdisc_at, self._kgen_at, self._settle_at, self._ref_at = [], 0.0, 0.0, 0.0, 0.0, 0.0
        self.rejections, self.recent = Counter(), deque(maxlen=200)
        self._rej_log = {}
        self.errors = {'polymarket': '', 'kalshi': '', 'feed': ''}
        self.started, self.last_cycle, self.last_ok, self.cycles = now(), None, None, 0
        self.live = False                                         # NEVER persisted: every restart begins in paper mode
        self.live_exec = None
        self._pool = ThreadPoolExecutor(max_workers=4, thread_name_prefix='uh-obs')
        self._stop, self._thread, self._lock_fd = threading.Event(), None, None
        store.accounts_ensure('PAPER', {'polymarket': self.settings().balance_polymarket, 'kalshi': self.settings().balance_kalshi})
        gws = {v: PaperGateway(v, self.fetch_obs, self.settings, paper_sleep, now) for v in VENUES}
        self.paper_exec = Executor('PAPER', gws, self.risk, self.settings, lambda: False, now)
        self.paper_settler = Settler('PAPER', self.result_of, now)
        self.live_settler = Settler('LIVE', self.result_of, now)

    # ------------------------------------------------------------------ settings / lock
    def settings(self):
        if self.now() - self._S_at > 2.0: self._S, self._S_at = bs.load(db.get_setting), self.now()
        return self._S

    def acquire_lock(self):
        """Only one scanner process may run: a second systemd/worker start is refused instead of duplicating orders."""
        os.makedirs(DATA_DIR, exist_ok=True)
        fd = open(os.path.join(DATA_DIR, 'scanner.lock'), 'w')
        try: fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError: fd.close(); return False
        self._lock_fd = fd; return True

    # ------------------------------------------------------------------ market data
    def fetch_obs(self, m):
        """One FRESH order-book observation of a market, straight from the exchange. None when there is no book or the fetch fails."""
        try:
            if m.venue == 'polymarket':
                bk = self.pm.books([m.up_id, m.down_id])
                u, dn = bk.get(m.up_id), bk.get(m.down_id)
                if u is None or dn is None: return None
                self.errors['polymarket'] = ''
                return poly_obs(m, u, dn, self.pm._levels, self.now())
            parsed = self.kalshi.get_orderbook(m.market_id); self.errors['kalshi'] = ''
            return kalshi_obs(m, parsed, self.now())
        except Exception as exc:
            self.errors[m.venue] = f'{type(exc).__name__}: {str(exc)[:100]}'; return None

    def lookup(self, venue, market_id): return next((m for m in self.markets.values() if m.venue == venue and m.market_id == market_id), None)

    def result_of(self, venue, market_id):
        if venue == 'polymarket':
            r = self.pm._http.get(f'{GAMMA_URL}/markets/{market_id}', timeout=(3, 6)); r.raise_for_status()
            return normalize_polymarket(r.json(), 900, 0).result
        m = self.kalshi.get_market(market_id)
        res = str(m.get('result') or '').lower()
        return {'yes': 'up', 'no': 'down'}.get(res)

    def discover(self):
        S, t = self.settings(), self.now()
        durs = tuple(x for x, on in ((300, S.d5), (900, S.d15)) if on)
        if t - self._disc_at > 15:
            mk, errs = discover_polymarket(self.pm, t, 1, durs); self._disc_at = t
            self.errors['polymarket'] = errs[0] if errs and not mk else ''
            for m in mk: self.markets[m.key] = m
        if t - self._kdisc_at > 30:
            self._kdisc_at = t
            generic = (not self.kalshi_series) or (t - self._kgen_at > 1800)
            mk, series, errs = discover_kalshi(self.kalshi, None if generic else self.kalshi_series, t)
            if generic: self._kgen_at = t; self.kalshi_series = series or self.kalshi_series
            self.errors['kalshi'] = errs[0] if errs and not mk else ''
            for m in mk:
                if m.duration in durs: self.markets[m.key] = m
        for k in [k for k, m in self.markets.items() if m.end_ts < t - 7200]: del self.markets[k]
        pm_ = [m for m in self.markets.values() if m.venue == 'polymarket' and m.end_ts > t - 60]
        km_ = [m for m in self.markets.values() if m.venue == 'kalshi' and m.end_ts > t - 60]
        self.pairs = [PairInfo(p, k, match_btc(p, k)) for p in pm_ for k in km_ if p.duration == k.duration and abs(p.start_ts - k.start_ts) <= 60]

    def live_markets(self):
        t = self.now(); S = self.settings()
        return [m for m in self.markets.values() if m.tradable and m.start_ts <= t < m.end_ts and ((m.duration == 300 and S.d5) or (m.duration == 900 and S.d15))]

    def observe(self, ms):
        futs = {m.key: self._pool.submit(self.fetch_obs, m) for m in ms}
        self.obs = {}
        for k, f in futs.items():
            o = f.result(timeout=20)
            if o: self.obs[k] = o

    # ------------------------------------------------------------------ strategy evaluation + coordination
    def _reject(self, rj):
        key = (rj.strategy, rj.market.key if rj.market else '', rj.reason.split(' (')[0][:60], rj.side)
        self.rejections[(rj.strategy, rj.reason.split(' (')[0][:70])] += 1
        t = self.now()
        if t - self._rej_log.get(key, 0) >= 60:
            self._rej_log[key] = t
            store.signal(t, rj.strategy, rj.market, rj.side, False, rj.reason, rj.edge, None, None, None, rj.features, None, 'PAPER')

    def evaluate_all(self):
        S, now = self.settings(), self.now()
        ctx = type('Ctx', (), {'now': now, 'S': S, 'feed': self.feed, 'hist': self.hist})()
        sigs = []
        for m in self.live_markets():
            o = self.obs.get(m.key)
            if o and o.up.bids and o.up.asks: self.hist.push(m.key, now, (o.up.best_bid + o.up.best_ask) / 2)
        for m in self.live_markets():
            o = self.obs.get(m.key)
            ok, why, rem, el = st.time_ok(m, now, S)
            for name, on in (('momentum', S.s_momentum), ('meanrev', S.s_meanrev), ('bundle', S.s_bundle)):
                if not on: continue
                if o is None: self._reject(st.Rejection(name, m, 'NO_ORDER_BOOK')); continue
                if not ok: self._reject(st.Rejection(name, m, why)); continue
                for r in st.ENTRY[name](ctx, m, o):
                    (sigs.append(r) if isinstance(r, st.Signal) else self._reject(r))
        if S.s_arb:
            pairs = [p for p in self.pairs if p.poly.key in self.obs or p.kal.key in self.obs]
            if not pairs:
                have = {(m.venue, m.duration) for m in self.live_markets()}
                missing = [f'{v} {dur // 60}-minute' for dur in (300, 900) for v in VENUES if (v, dur) not in have]
                self._reject(st.Rejection('arb', None, 'NO_CROSS_PLATFORM_PAIR: no live window on ' + (', '.join(missing) or 'both venues with the same start time')))
            for p in pairs:
                op, ok_ = self.obs.get(p.poly.key), self.obs.get(p.kal.key)
                okt, why, _, _ = st.time_ok(p.poly, now, S)
                if not (op and ok_): self._reject(st.Rejection('arb', p.poly, 'NO_ORDER_BOOK on ' + ('kalshi' if op else 'polymarket'))); continue
                if not okt: self._reject(st.Rejection('arb', p.poly, why)); continue
                for r in st.arb(ctx, p, op, ok_): (sigs.append(r) if isinstance(r, st.Signal) else self._reject(r))
        return sigs

    def coordinate(self, sigs, max_exec=2):
        """F: rank by expected net $ x quality, drop duplicate exposure, then execute. Every outcome is recorded with its reason."""
        seen, results = set(), []
        for sg in sorted(sigs, key=lambda s: float(s.expected_net) * s.quality, reverse=True):
            ident = {(l.market.key, l.side) for l in sg.legs}
            if ident & seen:
                store.signal(self.now(), sg.strategy, sg.market, sg.legs[0].side, False, 'DUPLICATE_EXPOSURE_WITH_HIGHER_RANKED_SIGNAL', sg.edge); continue
            if len(results) >= max_exec:
                store.signal(self.now(), sg.strategy, sg.market, sg.legs[0].side, False, 'EXECUTION_LIMIT_PER_CYCLE', sg.edge); continue
            seen |= ident
            res = self.paper_exec.execute(sg)
            if not res['ok']: store.signal(self.now(), sg.strategy, sg.market, sg.legs[0].side, False, 'PAPER: ' + res['reason'][:250], sg.edge, sg.p_model, sg.legs[0].vwap, sg.qty, sg.features)
            results.append((sg, res))
            self.recent.appendleft({'ts': self.now(), 'strategy': sg.strategy, 'market': sg.market.market_id, 'side': sg.legs[0].side, 'edge': float(sg.edge), 'ok': res['ok'], 'note': res.get('reason', 'EXECUTED')[:120]})
            if self.live and self.live_exec and not sg.research and sg.strategy in self.settings().live_strategies and res.get('ok') is not None:
                lres = self.live_exec.execute(sg)
                store.signal(self.now(), sg.strategy, sg.market, sg.legs[0].side, lres['ok'], 'LIVE: ' + (lres.get('reason') or 'EXECUTED')[:240], sg.edge, sg.p_model, sg.legs[0].vwap, sg.qty, sg.features, lres.get('group_id'), 'LIVE')
        return results

    # ------------------------------------------------------------------ one cycle
    def cycle(self):
        t = self.now(); self.cycles += 1
        try:
            self.discover()
            self.observe(self.live_markets())
            sigs = self.evaluate_all()
            if self.risk.killed():
                if self.settings().kill_policy == 'unwind_open':
                    self.paper_exec.unwind_all(self.lookup)
                    if self.live_exec: self.live_exec.unwind_all(self.lookup)
            else:
                self.coordinate(sigs)
                self.paper_exec.manage_exits(self.lookup)
                if self.live_exec: self.live_exec.manage_exits(self.lookup)
            if t - self._settle_at > 10:
                self._settle_at = t; self.paper_settler.run(); self.live_settler.run()
            if t - self._ref_at >= 5 and self.feed.latest():
                self._ref_at = t; l = self.feed.latest(); store.sample_ref(l[0], l[1], l[2])
            self.errors['feed'] = self.feed.last_error
            self.last_ok = t; store.health_set('last_ok', t)
        except Exception as exc:
            store.event('ERROR', 'CYCLE_FAILED', f'{type(exc).__name__}: {exc}')
            self.errors['cycle'] = f'{type(exc).__name__}: {str(exc)[:120]}'
        self.last_cycle = t; store.health_set('last_cycle', t); store.health_set('started', self.started)

    # ------------------------------------------------------------------ lifecycle
    def start(self):
        if self._thread and self._thread.is_alive(): return True
        if not self.acquire_lock():
            store.event('WARNING', 'DUPLICATE_WORKER', 'Another scanner process already holds the lock; this one stays idle.'); return False
        self.feed.start(1.0); self._stop.clear()
        def loop():
            while not self._stop.is_set():
                t0 = time.time(); self.cycle()
                self._stop.wait(max(0.2, self.settings().scan_interval - (time.time() - t0)))
        self._thread = threading.Thread(target=loop, daemon=True, name='uh-btc-scanner'); self._thread.start(); return True

    def stop(self): self._stop.set(); self.feed.stop()

    # ------------------------------------------------------------------ live mode
    def kalshi_auth_client(self):
        with db.connect() as c: r = c.execute('SELECT key_id, private_key, env FROM kalshi_credentials WHERE id=1').fetchone()
        if not r: return None
        return KalshiClient(env=r['env'], key_id=r['key_id'], private_key_pem=self.pm.vault.decrypt(r['private_key']))

    def preflight(self):
        S, checks = self.settings(), []
        add = lambda name, ok, detail, hard=True: checks.append({'name': name, 'ok': bool(ok), 'detail': detail, 'hard': hard})
        add('Kill switch', not self.risk.killed(), 'off' if not self.risk.killed() else self.risk.kill_reason())
        pc = self.pm.creds(); add('Polymarket credentials', bool(pc), 'connected' if pc else 'not connected')
        if pc:
            gw = PolyLiveGateway(self.pm, self.fetch_obs); b = gw.balance()
            add('Polymarket balance', b is not None, f'${b:.2f} available' if b is not None else 'could not read the balance (authentication or allowance problem)')
        kc = None
        try: kc = self.kalshi_auth_client()
        except Exception as exc: add('Kalshi credentials', False, f'stored key unusable: {type(exc).__name__}')
        if kc is None and not any(c['name'] == 'Kalshi credentials' for c in checks): add('Kalshi credentials', False, 'not configured')
        if kc:
            try: b = kc.balance()['balance_usd']; add('Kalshi authentication', True, f'ok ({kc.env}); balance ${b:.2f}' if b is not None else f'ok ({kc.env})')
            except KalshiError as exc: add('Kalshi authentication', False, str(exc)[:120])
        lm = self.live_markets()
        for dur in (300, 900):
            for v in VENUES:
                n = sum(1 for m in lm if m.venue == v and m.duration == dur)
                add(f'{v.title()} {dur // 60}-minute market', n > 0, f'{n} live window(s)' if n else 'none live right now' + (' (Kalshi may not offer this duration)' if v == 'kalshi' and dur == 300 else ''), hard=False)
        add('Verified cross-platform matches', any(p.status == 'VERIFIED' for p in self.pairs),
            f'{sum(1 for p in self.pairs if p.status == "VERIFIED")} verified of {len(self.pairs)} candidate pairs' + ('' if any(p.status == "VERIFIED" for p in self.pairs) else ' (cross-platform arbitrage stays disabled)'), hard=False)
        add('Live strategies permitted', bool(S.live_strategies), ', '.join(S.live_strategies) or 'none selected')
        add('Per-account limits', True, f'order ${S.live_max_order_polymarket}/${S.live_max_order_kalshi}, exposure ${S.live_max_exposure_polymarket}/${S.live_max_exposure_kalshi}, daily loss ${S.live_daily_loss_polymarket}/${S.live_daily_loss_kalshi} (Polymarket/Kalshi)', hard=False)
        add('Fee handling', True, 'Polymarket: market feeSchedule when stated, else the documented 0.07 crypto rate; Kalshi: 0.07 x C x P x (1-P) rounded up per order. All fees are estimates until reconciled.', hard=False)
        add('Order submission', True, 'FOK first leg, IOC second leg, then the recovery policy "' + S.recovery + '"', hard=False)
        return {'ok': all(c['ok'] for c in checks if c['hard']), 'checks': checks, 'kalshi': kc}

    def enable_live(self, confirm):
        if confirm != 'ENABLE LIVE TRADING': return False, 'Type ENABLE LIVE TRADING exactly to confirm.'
        pf = self.preflight()
        if not pf['ok']: return False, 'Preflight failed: ' + '; '.join(c['name'] for c in pf['checks'] if c['hard'] and not c['ok'])
        gws = {'polymarket': PolyLiveGateway(self.pm, self.fetch_obs), 'kalshi': KalshiLiveGateway(pf['kalshi'], self.fetch_obs)}
        self.live_exec = Executor('LIVE', gws, self.risk, self.settings, lambda: self.live, self.now)
        self.live = True; store.event('WARNING', 'LIVE_ENABLED', 'Live trading was enabled by the operator.')
        return True, 'LIVE trading enabled. Only strategies ticked under "Live strategies" will place real orders.'

    def disable_live(self, reason='operator'):
        self.live = False; store.event('WARNING', 'LIVE_DISABLED', f'Live trading disabled ({reason}). Existing orders/positions are untouched.')
