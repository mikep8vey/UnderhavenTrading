"""Strategy signals. Each strategy turns real observations into a Signal (what to buy, how many, the worst acceptable price, the expected net
edge after fees/slippage/safety) or a Rejection with a reason. Strategies never place orders; the coordinator ranks, risk-checks and executes.

  arb       A  cross-platform complementary pair (Polymarket UP + Kalshi DOWN or the reverse), VERIFIED matches only
  momentum  B  BTC distance-from-start + short-term drift  ->  model probability vs executable ask
  meanrev   C  market jump with no matching BTC move  ->  fade toward the model value
  (book)    D  order-book quality / imbalance: a FILTER on every strategy and an optional small adjustment to the model probability
  (time)    E  entry window from contract duration, settlement and latency
  bundle    G  retained same-platform strategy: Polymarket UP + DOWN asks for less than $1 after fees
"""
import math, time
from collections import deque
from dataclasses import dataclass, field
from decimal import Decimal, ROUND_FLOOR
from typing import List, Optional
from .calc import FeeModel, ONE, ZERO, d, fee_for_fill, pair_edge, walk
from .books import MarketObs


@dataclass
class LegSpec:
    market: object
    side: str                 # 'UP' | 'DOWN'
    qty: Decimal
    limit: Decimal            # worst acceptable ask (a marketable limit)
    vwap: Decimal
    fee: Decimal
    fm: FeeModel


@dataclass
class Signal:
    strategy: str
    legs: List[LegSpec]
    edge: Decimal             # expected net edge per contract/pair after every cost
    expected_net: Decimal     # edge x qty, dollars
    p_model: Optional[float] = None
    features: dict = field(default_factory=dict)
    quality: float = 1.0
    research: bool = False    # unverified cross-platform pair, paper only

    @property
    def market(self): return self.legs[0].market
    @property
    def cost(self): return sum((l.qty * l.vwap + l.fee for l in self.legs), ZERO)
    @property
    def qty(self): return self.legs[0].qty
    @property
    def cross(self): return len({l.market.venue for l in self.legs}) > 1


@dataclass
class Rejection:
    strategy: str
    market: object
    reason: str
    side: str = ''
    edge: Optional[Decimal] = None
    features: dict = field(default_factory=dict)


class MarketHistory:
    """Short rolling history of each UP price (mid of best bid/ask) for jump detection."""
    def __init__(self, maxlen=400): self.h, self.maxlen = {}, maxlen
    def push(self, key, ts, mid): self.h.setdefault(key, deque(maxlen=self.maxlen)).append((ts, mid))
    def change(self, key, now, lookback):
        h = self.h.get(key)
        if not h or len(h) < 2: return None
        old = next((m for t, m in h if t >= now - lookback), None)
        return None if old is None else h[-1][1] - old


def fee_model(m) -> FeeModel:
    if m.venue == 'polymarket': return FeeModel('polymarket', rate=m.fee_rate, exponent=m.fee_exponent, estimated=not m.fee_known)
    return FeeModel('kalshi', multiplier=m.fee_multiplier, maker_fees=m.maker_fees, estimated=not m.fee_known)


def ncdf(x): return 0.5 * (1.0 + math.erf(x / math.sqrt(2.0)))


# ----------------------------------------------------------------------------- E: time policy
def time_ok(m, now, S):
    rem, el = m.end_ts - now, now - m.start_ts
    mr = S.min_remaining_5m if m.duration == 300 else S.min_remaining_15m
    me = S.min_elapsed_5m if m.duration == 300 else S.min_elapsed_15m
    need = mr + (m.seconds_delay or 0) + S.latency
    if rem <= 0: return False, 'WINDOW_ENDED', rem, el
    if rem < need: return False, f'TOO_LATE ({rem:.0f}s left, need {need:.0f}s for latency, delay and exit)', rem, el
    if el < me: return False, f'TOO_EARLY ({el:.0f}s elapsed, reference not established until {me:.0f}s)', rem, el
    return True, '', rem, el


# ----------------------------------------------------------------------------- D: book quality
def imbalance(book, top=3):
    b, a = book.depth('bids', top), book.depth('asks', top)
    return float((b - a) / (b + a)) if (b + a) > 0 else 0.0


def book_ok(obs: MarketObs, side, S, now):
    b = obs.book(side)
    if obs.age(now) > S.max_quote_age: return False, f'STALE_BOOK ({obs.age(now):.1f}s)'
    if not b.valid(): return False, 'INVALID_BOOK (crossed or unsorted)'
    if not b.asks: return False, 'NO_ASKS'
    if b.spread is not None and b.spread > d(S.max_spread): return False, f'SPREAD_TOO_WIDE ({b.spread})'
    if b.depth('asks', 3) < d(S.min_depth): return False, f'THIN_BOOK ({b.depth("asks", 3)} contracts in top 3 levels)'
    return True, ''


# ----------------------------------------------------------------------------- model probability (B, C)
def model_prob(ctx, m):
    feed = ctx.feed; S = ctx.S
    last = feed.latest()
    if not last or feed.age() is None or feed.age() > 5: return None, 'NO_FRESH_BTC_PRICE', {}
    s0 = feed.price_at(m.start_ts)
    if not s0: return None, 'NO_WINDOW_START_PRICE (feed was not running at the window start)', {}
    sig = feed.sigma(300)
    if not sig or sig <= 0: return None, 'INSUFFICIENT_VOLATILITY_DATA', {}
    tau = m.end_ts - ctx.now
    if tau <= 0: return None, 'WINDOW_ENDED', {}
    st = last[1]
    r30 = feed.ret(S.mom_lookback) or 0.0
    drift = S.mom_weight * r30 / S.mom_lookback            # per-second log drift, shrunk by mom_weight (0 = pure random walk)
    mean = math.log(st / s0) + drift * tau
    sd = sig * math.sqrt(tau)
    p = ncdf(mean / sd)
    feats = {'btc': round(st, 2), 'btc_start': round(s0, 2), 'dist_pct': round((st / s0 - 1) * 100, 4), 'sigma_bp_per_sqrt_s': round(sig * 1e4, 4), 'remaining_s': round(tau, 1),
             'ret_%ds' % S.mom_lookback: round(r30 * 100, 4), 'z': round(mean / sd, 3), 'feed': last[2]}
    return p, '', feats


# ----------------------------------------------------------------------------- shared single-leg pricing
def price_single(m, obs, side, p_win, S, now, strategy):
    """Expected value per contract of buying `side` at executable asks, after fee, slippage and safety. Returns (LegSpec, edge) or (None, reason)."""
    ok, why = book_ok(obs, side, S, now)
    if not ok: return None, why
    book = obs.book(side); fm = fee_model(m)
    best = book.best_ask
    lot = ONE if m.venue == 'kalshi' else None
    cap = min(d(S.max_order_notional), d(S.max_capital_per_trade))
    q = (cap / best).to_integral_value(rounding=ROUND_FLOOR)
    if q < 1: return None, f'ORDER_SIZE_LIMIT_BELOW_ONE_CONTRACT (cap ${cap}, ask {best})'
    depth_ok = None
    while q >= 1:                                   # the capital cap covers cost AND fee
        fill = walk(book.asks, q, lot=lot)
        if fill.complete and fill.cost + fee_for_fill(fill, fm) <= cap: depth_ok = fill; break
        q -= 1
    if not depth_ok: return None, ('NO_DEPTH' if not walk(book.asks, 1, lot=lot).complete else f'ORDER_SIZE_LIMIT_BELOW_ONE_CONTRACT (cap ${cap} incl. fees)')
    fill = depth_ok
    if m.venue == 'polymarket' and m.min_notional and fill.cost < m.min_notional: return None, f'BELOW_MIN_ORDER (${fill.cost:.2f} < ${m.min_notional})'
    fee = fee_for_fill(fill, fm)
    per = lambda x: x / fill.qty
    edge = d(p_win) - fill.vwap - per(fee) - d(S.slippage) - d(S.safety_margin)
    leg = LegSpec(m, side, fill.qty, fill.worst, fill.vwap, fee, fm)
    return (leg, edge), ''


def _quality(obs, side, edge):
    b = obs.book(side); sp = float(b.spread or 0.1)
    return max(0.1, min(1.0, 1.0 - sp * 4)) * (1.0 + min(float(edge), 0.2))


# ----------------------------------------------------------------------------- B momentum / C mean reversion
def _model_signals(ctx, m, obs, strategy):
    S, now = ctx.S, ctx.now
    p, why, feats = model_prob(ctx, m)
    if p is None: return [Rejection(strategy, m, why)]
    imb = imbalance(obs.up)
    p_adj = min(0.99, max(0.01, p + S.imb_weight * imb))     # D: optional small adjustment, 0 by default
    feats.update({'p_model': round(p, 4), 'imbalance': round(imb, 3), 'p_used': round(p_adj, 4)})
    if strategy == 'meanrev':
        key = m.key
        jump = ctx.hist.change(key, now, S.mr_lookback)
        mid = (obs.up.best_bid + obs.up.best_ask) / 2 if obs.up.bids and obs.up.asks else None
        if jump is None or mid is None: return [Rejection(strategy, m, 'NO_PRICE_HISTORY', features=feats)]
        feats.update({'jump': round(float(jump), 4), 'mid': round(float(mid), 4)})
        if abs(jump) < S.mr_jump: return [Rejection(strategy, m, f'NO_JUMP ({float(jump):+.3f} < {S.mr_jump})', features=feats)]
        dev = float(mid) - p_adj
        if abs(dev) < S.mr_dev: return [Rejection(strategy, m, f'NO_DISLOCATION ({dev:+.3f} from model)', features=feats)]
        if (jump > 0) != (dev > 0): return [Rejection(strategy, m, 'MOVE_CONFIRMED_BY_MODEL (a real trend, not an overreaction)', features=feats)]
    cands, rej = [], []
    for side, pw in (('UP', p_adj), ('DOWN', 1 - p_adj)):
        res, why = price_single(m, obs, side, pw, S, now, strategy)
        if res is None: rej.append(Rejection(strategy, m, why, side, features=feats)); continue
        leg, edge = res
        if edge < d(S.min_net_edge): rej.append(Rejection(strategy, m, f'EDGE_BELOW_MINIMUM ({float(edge):+.4f} < {S.min_net_edge})', side, edge, feats)); continue
        cands.append(Signal(strategy, [leg], edge, edge * leg.qty, p_adj, dict(feats, side=side), _quality(obs, side, edge)))
    return [max(cands, key=lambda s: s.expected_net)] if cands else rej


def momentum(ctx, m, obs): return _model_signals(ctx, m, obs, 'momentum')
def meanrev(ctx, m, obs): return _model_signals(ctx, m, obs, 'meanrev')


# ----------------------------------------------------------------------------- G bundle (same platform)
def bundle(ctx, m, obs):
    S, now = ctx.S, ctx.now
    if m.venue != 'polymarket': return []
    for side in ('UP', 'DOWN'):
        ok, why = book_ok(obs, side, S, now)
        if not ok: return [Rejection('bundle', m, why, side)]
    fm = fee_model(m)
    best = None
    cap = min(d(S.max_order_notional), d(S.max_capital_per_trade))
    for q in range(1, int(cap / max(obs.up.best_ask + obs.down.best_ask, Decimal('0.01'))) + 1):
        pe = pair_edge(obs.up.asks, obs.down.asks, q, fm, fm, slippage_per_unit=d(S.slippage), safety_per_unit=d(S.safety_margin))
        if pe.complete and pe.capital <= cap and (best is None or pe.net > best.net): best = pe
    if not best: return [Rejection('bundle', m, 'NO_DEPTH')]
    edge = best.net / best.qty
    if edge < d(S.min_net_edge): return [Rejection('bundle', m, f'EDGE_BELOW_MINIMUM ({float(edge):+.4f}; pair costs {float(obs.up.best_ask + obs.down.best_ask):.3f})', edge=edge)]
    if m.min_notional and (best.fill_a.cost < m.min_notional or best.fill_b.cost < m.min_notional): return [Rejection('bundle', m, 'BELOW_MIN_ORDER', edge=edge)]
    legs = [LegSpec(m, 'UP', best.qty, best.fill_a.worst, best.fill_a.vwap, fee_for_fill(best.fill_a, fm), fm), LegSpec(m, 'DOWN', best.qty, best.fill_b.worst, best.fill_b.vwap, fee_for_fill(best.fill_b, fm), fm)]
    return [Signal('bundle', legs, edge, best.net, None, {'pair_cost': float(best.fill_a.vwap + best.fill_b.vwap)})]


# ----------------------------------------------------------------------------- A cross-platform arbitrage
def arb(ctx, pair, obs_p, obs_k):
    """Buy UP on one venue and DOWN on the other for a VERIFIED pair: exactly one wins, so the pair pays $1 whatever BTC does."""
    S, now = ctx.S, ctx.now
    p, k = pair.poly, pair.kal
    research = pair.status != 'VERIFIED'
    if research and not S.research_xplat: return [Rejection('arb', p, 'PAIR_NOT_VERIFIED: ' + '; '.join(pair.reasons[:2]))]
    out = []
    for (mp, sp, mk_, sk, name) in ((p, 'UP', k, 'DOWN', 'POLY_UP+KALSHI_DOWN'), (p, 'DOWN', k, 'UP', 'POLY_DOWN+KALSHI_UP')):
        for mkt, ob, sd in ((mp, obs_p, sp), (mk_, obs_k, sk)):
            ok, why = book_ok(ob, sd, S, now)
            if not ok: out.append(Rejection('arb', mp, f'{name}: {mkt.venue} {why}', sd)); break
        else:
            fma, fmb = fee_model(mp), fee_model(mk_)
            A, B = obs_p.book(sp).asks, obs_k.book(sk).asks
            cap = min(d(S.max_order_notional), d(S.max_capital_per_trade))
            qmax = int(cap / max(A[0][0] + B[0][0], Decimal('0.01')))
            best = None
            for q in range(1, qmax + 1):
                pe = pair_edge(A, B, q, fma, fmb, slippage_per_unit=d(S.slippage), safety_per_unit=d(S.safety_margin), lot=ONE)
                if not pe.complete: break
                if pe.capital > cap: break
                if best is None or pe.net > best.net: best = pe
            if not best: out.append(Rejection('arb', mp, f'{name}: NO_DEPTH', sp)); continue
            edge = best.net / best.qty
            if edge < d(S.min_net_edge):
                out.append(Rejection('arb', mp, f'{name}: EDGE_BELOW_MINIMUM ({float(edge):+.4f}; pair costs {float(best.fill_a.vwap + best.fill_b.vwap):.3f})', sp, edge)); continue
            if mp.min_notional and best.fill_a.cost < mp.min_notional: out.append(Rejection('arb', mp, f'{name}: BELOW_MIN_ORDER', sp, edge)); continue
            legs = [LegSpec(mp, sp, best.qty, best.fill_a.worst, best.fill_a.vwap, fee_for_fill(best.fill_a, fma), fma),
                    LegSpec(mk_, sk, best.qty, best.fill_b.worst, best.fill_b.vwap, fee_for_fill(best.fill_b, fmb), fmb)]
            out.append(Signal('arb', legs, edge, best.net, None, {'direction': name, 'pair_cost': float(best.fill_a.vwap + best.fill_b.vwap), 'match': pair.status}, 1.0, research))
    return out


ENTRY = {'momentum': momentum, 'meanrev': meanrev, 'bundle': bundle}
