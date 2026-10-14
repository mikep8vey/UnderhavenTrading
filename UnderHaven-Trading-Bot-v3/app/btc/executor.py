"""Order execution shared by paper and live mode. The ONLY difference between the two is the Gateway: PaperGateway simulates fills against REAL
order books; the live gateways talk to the exchanges. Cross-venue execution is NOT atomic, so the leg sequencing, partial-fill handling and
recovery below are explicit, and every attempt is persisted."""
import time, uuid
from dataclasses import dataclass
from decimal import Decimal
from . import store
from .books import MarketObs
from .calc import FeeModel, Position, ZERO, ONE, d, fee_for_fill, walk
from .strategies import fee_model, Signal, LegSpec


@dataclass
class OrderResult:
    status: str                    # FILLED | PARTIAL | UNFILLED | REJECTED | UNCERTAIN
    qty: Decimal = ZERO
    avg_price: Decimal = ZERO
    fees: Decimal = ZERO
    ref: str = ''
    estimated: bool = True
    reason: str = ''
    slippage: Decimal = ZERO


class PaperGateway:
    """Simulated execution against REAL, freshly fetched order books. No randomness: a fill is whatever the displayed depth (haircut by the
    configured fraction) supports at or better than the limit price, after the configured latency. Never touches an exchange."""
    mode = 'PAPER'

    def __init__(self, venue, fetcher, settings_fn, sleep=time.sleep, now=time.time):
        self.venue, self.fetch, self.S, self.sleep, self.now = venue, fetcher, settings_fn, sleep, now
        self._used = []          # (ts, market, side, action, qty): liquidity this simulator already consumed

    def balance(self): return store.cash('PAPER', self.venue)

    def place(self, market, side, action, qty, limit, tif, ref):
        S = self.S()
        self.sleep(S.latency + (market.seconds_delay or 0))         # time for the order to reach the book
        obs = self.fetch(market)
        if obs is None: return OrderResult('REJECTED', reason='NO_ORDER_BOOK')
        book = obs.book(side); fm = fee_model(market)
        lot = ONE if market.venue == 'kalshi' else None
        q = d(qty)
        if lot and q != q.to_integral_value(): return OrderResult('REJECTED', reason='KALSHI_WHOLE_CONTRACTS_ONLY')
        t = self.now(); self._used = [u for u in self._used if t - u[0] < 5.0]       # displayed depth does not replenish within 5 seconds
        used = sum((u[4] for u in self._used if u[1:4] == (market.market_id, side, action)), ZERO)
        def after(levels):
            skip, out = used, []
            for p, s in levels:
                take = min(s, skip); skip -= take
                if s - take > 0: out.append((p, s - take))
            return out
        if action == 'buy':
            fill = walk(after([(p, s * d(S.depth_haircut)) for p, s in book.asks]), q, side='buy', limit=limit, lot=lot)
            if market.venue == 'polymarket' and market.min_notional and q * d(limit) < market.min_notional: return OrderResult('REJECTED', reason='BELOW_MIN_ORDER')
        else:
            fill = walk(after([(p, s * d(S.depth_haircut)) for p, s in book.bids]), q, side='sell', limit=limit, lot=lot)
        if fill.qty <= 0: return OrderResult('UNFILLED', reason='NO_LIQUIDITY_AT_LIMIT')
        if tif == 'fok' and not fill.complete: return OrderResult('UNFILLED', reason=f'FOK: only {fill.qty} of {q} fillable at the limit')
        self._used.append((t, market.market_id, side, action, fill.qty))
        fee = fee_for_fill(fill, fm)
        best = fill.best
        slip = abs(fill.vwap - best) * fill.qty if best is not None else ZERO
        return OrderResult('FILLED' if fill.complete else 'PARTIAL', fill.qty, fill.vwap, fee, ref=f'paper-{ref}', estimated=True, slippage=slip)


class Executor:
    def __init__(self, mode, gateways, risk, settings_fn, live_flag=lambda: False, now=time.time):
        self.mode, self.gw, self.risk, self.S, self.live_flag, self.now = mode, gateways, risk, settings_fn, live_flag, now
        self._inflight = set()

    # ------------------------------------------------------------------ helpers
    def _record(self, gid, strategy, leg_market, side, action, tif, qty, limit, res: OrderResult):
        store.order(ts=self.now(), mode=self.mode, group_id=gid, strategy=strategy, venue=leg_market.venue, market_id=leg_market.market_id, duration=leg_market.duration,
                    window_start=leg_market.start_ts, side=side, action=action, tif=tif, qty_req=qty, limit_price=limit, status=res.status, qty_filled=res.qty,
                    avg_price=res.avg_price if res.qty else None, fees=res.fees, slippage=res.slippage, reason=res.reason, order_ref=res.ref, estimated=1 if res.estimated else 0)

    def _place(self, gid, strategy, market, side, action, qty, limit, tif):
        ref = f'{gid[:8]}-{uuid.uuid4().hex[:6]}'
        try: res = self.gw[market.venue].place(market, side, action, qty, limit, tif, ref)
        except Exception as exc: res = OrderResult('UNCERTAIN', reason=f'{type(exc).__name__}: {str(exc)[:120]}')
        self._record(gid, strategy, market, side, action, tif, qty, limit, res)
        if res.status == 'UNCERTAIN': store.event('CRITICAL', 'ORDER_OUTCOME_UNKNOWN', f'{market.venue} {side} {action} {market.market_id}: {res.reason}', {'ref': ref})
        return res

    def _available(self, leg):
        try:
            obs = self.gw[leg.market.venue].fetch(leg.market)
            return walk(obs.book(leg.side).asks, leg.qty, side='buy', limit=leg.limit).qty if obs else ZERO
        except Exception: return ZERO

    def _order_legs(self, sig):
        legs = list(sig.legs)
        if len(legs) < 2: return legs
        pol = self.S().leg_order
        if pol == 'polymarket_first': return sorted(legs, key=lambda l: l.market.venue != 'polymarket')
        if pol == 'kalshi_first': return sorted(legs, key=lambda l: l.market.venue != 'kalshi')
        return sorted(legs, key=self._available)                      # thinnest liquidity first

    def _apply(self, positions, gid, sig, leg, res, side_pos=None):
        key = (leg.market.venue, leg.market.market_id, leg.side)
        if key not in positions:
            positions[key] = [Position(), None]
        pos, pid = positions[key]
        pos.buy(res.qty, res.avg_price, res.fees)
        if pid is None: positions[key][1] = store.position_create(self.mode, gid, sig.strategy, leg.market, leg.side, pos)
        else: store.position_update(pid, pos)

    def _sell(self, positions, gid, strategy, market, side, qty, why):
        """Sell `qty` of a held leg into the real bids. Used for unwinding and exits. Returns the quantity actually sold."""
        key = (market.venue, market.market_id, side); sold = ZERO
        pos, pid = positions[key]
        for attempt in range(int(self.S().retry_count) + 2):
            want = qty - sold
            if want <= 0: break
            obs = self.gw[market.venue].fetch(market)
            if obs is None: break
            lot = ONE if market.venue == 'kalshi' else None
            probe = walk(obs.book(side).bids, want, side='sell', lot=lot)
            if probe.qty <= 0: break
            res = self._place(gid, strategy, market, side, 'sell', probe.qty, probe.worst, 'ioc')
            if res.qty > 0:
                pos.sell(res.qty, res.avg_price, res.fees); sold += res.qty
                store.position_update(pid, pos, 'CLOSED' if pos.qty <= 0 else None, note=why)
        return sold

    # ------------------------------------------------------------------ the entry workflow
    def execute(self, sig: Signal):
        S = self.S(); ts = self.now()
        m0 = sig.market; ident = (m0.market_id, sig.legs[0].side, m0.start_ts, sig.strategy)
        if ident in self._inflight: return {'ok': False, 'reason': 'DUPLICATE_REQUEST_IN_FLIGHT'}
        self._inflight.add(ident)
        try:
            open_keys = {(p['venue'], p['market_id'], p['side'], p['window_start']) for p in store.open_positions(self.mode)}
            for l in sig.legs:
                if (l.market.venue, l.market.market_id, l.side, l.market.start_ts) in open_keys:
                    return {'ok': False, 'reason': 'DUPLICATE_EXPOSURE (already holding this contract side in this window)'}
            ok, reasons = self.risk.check_entry(sig, self.mode, self.live_flag())
            if not ok: return {'ok': False, 'reason': '; '.join(reasons)}
            gid = uuid.uuid4().hex; positions = {}
            legs = self._order_legs(sig)
            first = legs[0]
            r1 = self._place(gid, sig.strategy, first.market, first.side, 'buy', first.qty, first.limit, 'fok')
            if r1.status != 'FILLED':
                return {'ok': False, 'group_id': gid, 'reason': f'FIRST_LEG_{r1.status}: {r1.reason}', 'held': False}
            self._apply(positions, gid, sig, first, r1)
            results = [(first, r1)]
            matched = r1.qty
            if len(legs) > 1:
                second = legs[1]
                r2 = self._place(gid, sig.strategy, second.market, second.side, 'buy', r1.qty, second.limit, 'ioc')
                if r2.qty > 0: self._apply(positions, gid, sig, second, r2)
                results.append((second, r2))
                matched = min(r1.qty, r2.qty) if r2.qty > 0 else ZERO
                remaining = r1.qty - r2.qty
                deadline = ts + S.max_exec_seconds
                # recovery policy for the unmatched part of leg 1
                if remaining > 0:
                    store.event('WARNING', 'UNMATCHED_LEG', f'{sig.strategy}: leg 1 filled {r1.qty}, leg 2 filled {r2.qty}; recovery policy {S.recovery}', {'group': gid})
                    if S.recovery == 'complete_then_unwind':
                        for _ in range(int(S.retry_count)):
                            if remaining <= 0 or self.now() > deadline: break
                            rr = self._place(gid, sig.strategy, second.market, second.side, 'buy', remaining, second.limit, 'ioc')
                            if rr.qty > 0: self._apply(positions, gid, sig, second, rr); remaining -= rr.qty
                    if remaining > 0 and S.recovery in ('complete_then_unwind', 'unwind_only'):
                        sold = self._sell(positions, gid, sig.strategy, first.market, first.side, remaining, 'unwound after the second leg failed')
                        remaining -= sold
                    if remaining > 0:
                        store.event('CRITICAL', 'MANUAL_ACTION_REQUIRED', f'Unmatched {remaining} contracts of {first.market.venue} {first.side} {first.market.market_id}', {'group': gid})
                        self.risk.kill('Unmatched cross-platform exposure could not be neutralised')
                    held = {k[0]: sum((pp[0].bought - pp[0].sold for kk, pp in positions.items() if kk[0] == k[0]), ZERO) for k in positions}
                    matched = min(held.get(first.market.venue, ZERO), held.get(second.market.venue, ZERO)) if first.market.venue != second.market.venue else matched
            store.signal(ts, sig.strategy, m0, sig.legs[0].side, True, 'EXECUTED', sig.edge, sig.p_model, sig.legs[0].vwap, sig.qty, sig.features, gid, self.mode)
            return {'ok': True, 'group_id': gid, 'matched': matched, 'legs': [(l.market.venue, l.side, str(r.qty), str(r.avg_price), r.status) for l, r in results]}
        finally:
            self._inflight.discard(ident)

    # ------------------------------------------------------------------ exits (single-leg directional trades only; hedged pairs hold to settlement)
    def manage_exits(self, lookup=None):
        S = self.S(); done = []
        groups = {}
        for p in store.open_positions(self.mode):
            if p['status'] == 'OPEN' and p['qty'] > 1e-9: groups.setdefault(p['group_id'], []).append(p)
        for gid, legs in groups.items():
            if len(legs) != 1: continue                      # hedged: do not break the hedge
            p = legs[0]; now = self.now()
            if p['window_end'] <= now: continue
            mk = (lookup(p['venue'], p['market_id']) if lookup else None) or _market_from_row(p)
            try: obs = self.gw[p['venue']].fetch(mk)
            except Exception: obs = None
            if obs is None: continue
            pos = store.pos_from_row(p)
            fill = walk(obs.book(p['side']).bids, pos.qty, side='sell', lot=ONE if p['venue'] == 'kalshi' else None)
            if fill.qty < pos.qty: continue                  # the book cannot absorb the whole position: do not pretend
            value = fill.cost - fee_for_fill(fill, fee_model(mk))
            paid = pos.paid
            why = None
            if S.take_profit and paid > 0 and value >= paid * (1 + d(S.take_profit)): why = 'take-profit'
            elif S.stop_loss and paid > 0 and value <= paid * (1 - d(S.stop_loss)): why = 'stop-loss'
            elif S.max_hold and now - p['opened_ts'] >= S.max_hold: why = 'max holding time'
            elif S.exit_before_expiry and p['window_end'] - now <= S.exit_before_expiry: why = 'pre-expiry exit'
            if why:
                positions = {(p['venue'], p['market_id'], p['side']): [pos, p['id']]}
                sold = self._sell(positions, gid, p['strategy'], mk, p['side'], pos.qty, why)
                done.append((gid, why, str(sold)))
        return done


    def unwind_all(self, lookup=None):
        """Kill-switch policy 'unwind_open': sell every open single position and every leg of an unmatched group into the real bids."""
        out = []
        for p in store.open_positions(self.mode):
            if p['status'] != 'OPEN' or p['qty'] <= 1e-9 or p['window_end'] <= self.now(): continue
            mk = (lookup(p['venue'], p['market_id']) if lookup else None) or _market_from_row(p)
            pos = store.pos_from_row(p)
            sold = self._sell({(p['venue'], p['market_id'], p['side']): [pos, p['id']]}, p['group_id'], p['strategy'], mk, p['side'], pos.qty, 'kill-switch unwind')
            out.append((p['id'], str(sold)))
        return out


def _market_from_row(p):
    from .models import BtcMarket
    return BtcMarket(p['venue'], p['market_id'], int(p['duration']), p['window_start'], p['window_end'], tradable=True)
