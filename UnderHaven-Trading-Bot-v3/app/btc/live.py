"""LIVE gateways. They submit REAL orders and are only ever constructed when the operator has passed the preflight and enabled live mode.
Nothing here runs in paper mode. Outcomes that cannot be confirmed are reported as UNCERTAIN and never retried blindly."""
import time
from decimal import Decimal
from ..kalshi import KalshiError, KalshiUncertain, fill_summary
from .calc import FeeModel, ONE, ZERO, d, kalshi_fee
from .executor import OrderResult
from .strategies import fee_model


def _matched(resp):
    """True only when the exchange clearly says the order matched. {'success': False, ...} is never a fill."""
    r = resp if isinstance(resp, dict) else getattr(resp, '__dict__', None)
    if not isinstance(r, dict): return False
    if r.get('success') is False or str(r.get('success')).strip().lower() == 'false': return False
    if r.get('errorMsg') or r.get('error') or r.get('error_message'): return False
    return str(r.get('status') or r.get('order_status') or '').strip().lower() in ('matched', 'filled', 'confirmed')


class PolyLiveGateway:
    mode, venue = 'LIVE', 'polymarket'

    def __init__(self, pm, fetcher):
        self.pm, self.fetch = pm, fetcher

    def balance(self):
        try:
            b = self.pm.balance()
            raw = b.get('balance') if isinstance(b, dict) else getattr(b, 'balance', None)
            return d(raw) / Decimal(1_000_000) if raw is not None else None
        except Exception: return None

    def place(self, market, side, action, qty, limit, tif, ref):
        client = self.pm.client()
        if not client: return OrderResult('REJECTED', reason='NO_POLYMARKET_CREDENTIALS')
        from py_clob_client_v2 import OrderType, PartialCreateOrderOptions, Side, OrderArgs
        token = market.up_id if side == 'UP' else market.down_id
        if not token: return OrderResult('REJECTED', reason='MISSING_TOKEN_ID')
        tick = '%g' % float(market.tick)
        if tick not in ('0.1', '0.01', '0.001', '0.0001'): return OrderResult('REJECTED', reason=f'UNSUPPORTED_TICK {tick}')
        ot = OrderType.FOK if tif == 'fok' else getattr(OrderType, 'FAK', OrderType.FOK)
        opts = PartialCreateOrderOptions(tick_size=tick)
        resp = client.create_and_post_order(OrderArgs(token_id=token, price=float(limit), side=Side.BUY if action == 'buy' else Side.SELL, size=float(qty)), options=opts, order_type=ot)
        if not _matched(resp):
            return OrderResult('UNFILLED', reason=str((resp or {}).get('errorMsg') or (resp or {}).get('status') or 'not matched')[:120], ref=str((resp or {}).get('orderID', '')))
        r = resp if isinstance(resp, dict) else resp.__dict__
        mk_, tk_ = r.get('makingAmount'), r.get('takingAmount')
        if mk_ is not None and tk_ is not None and d(mk_) > 0 and d(tk_) > 0:
            shares, cash = (d(tk_), d(mk_)) if action == 'buy' else (d(mk_), d(tk_))
            avg, est = cash / shares, False
        else: shares, avg, est = d(qty), d(limit), True       # matched but amounts missing: assume the full size at the worst price (conservative) and flag it
        fee = fee_model(market).fee(shares, avg)
        return OrderResult('FILLED' if shares >= d(qty) else 'PARTIAL', shares, avg, fee, ref=str(r.get('orderID', '')), estimated=True)


class KalshiLiveGateway:
    mode, venue = 'LIVE', 'kalshi'

    def __init__(self, client, fetcher):
        self.client, self.fetch = client, fetcher

    def balance(self):
        try:
            b = self.client.balance()['balance_usd']; return d(b) if b is not None else None
        except Exception: return None

    def _result(self, market, order, count, action):
        oid = order.get('order_id')
        fills = []
        try: fills = self.client.fills(order_id=oid) if oid else []
        except KalshiError: pass
        s = fill_summary(order, fills)
        filled = d(s['filled'])
        if filled <= 0: return OrderResult('UNFILLED', ref=str(oid or ''), reason=str(s.get('status') or ''))
        avg = d(s['avg_price']).quantize(Decimal('0.00001')) if s['avg_price'] is not None else ZERO
        fee = d(s['fees']) if s['fees'] else kalshi_fee(filled, avg, market.fee_multiplier)
        return OrderResult('FILLED' if filled >= count else 'PARTIAL', filled, avg, fee, ref=str(oid or ''), estimated=not bool(fills))

    def place(self, market, side, action, qty, limit, tif, ref):
        if d(qty) != d(qty).to_integral_value(): return OrderResult('REJECTED', reason='KALSHI_WHOLE_CONTRACTS_ONLY')
        count = int(qty); coid = f'uh-{ref}'[:60]
        try:
            order = self.client.create_order(market.market_id, 'yes' if side == 'UP' else 'no', action, count, float(limit),
                                             'fill_or_kill' if tif == 'fok' else 'immediate_or_cancel', coid)
        except KalshiUncertain:
            order = None
            try: order = self.client.find_order_by_client_id(market.market_id, coid)       # reconcile before anything else; never resend blindly
            except KalshiError: pass
            if order is None: return OrderResult('UNCERTAIN', reason='order outcome could not be confirmed; check the Kalshi account', ref=coid)
        except KalshiError as exc:
            return OrderResult('REJECTED', reason=str(exc)[:150])
        return self._result(market, order, count, action)
