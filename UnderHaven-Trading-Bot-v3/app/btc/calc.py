"""Shared financial engine: fees, executable prices from order-book depth, position accounting and P&L.

EVERY strategy and the paper/live brokers use this module; nothing else computes money.
All arithmetic is Decimal. Prices are dollars per contract (0..1); quantities are contracts/shares.

Fee schedules (verified against the official docs, see README):
  Polymarket  fee = C x rate x (p x (1-p)) ** exponent   taker only, makers pay 0, rounded to 5 decimals. rate/exponent come from the market's feeSchedule.
  Kalshi      fee = ceil_to_cent( M x 0.07 x C x P x (1-P) )   per ORDER, M = series multiplier (1 by default)
"""
from dataclasses import dataclass, field
from decimal import Decimal, ROUND_CEILING, ROUND_HALF_UP, ROUND_FLOOR
from typing import List, Optional, Sequence, Tuple

ZERO, ONE, CENT = Decimal('0'), Decimal('1'), Decimal('0.01')
Level = Tuple[Decimal, Decimal]


def d(x) -> Decimal:
    if isinstance(x, Decimal): return x
    if x is None: return ZERO
    return Decimal(str(x))


def money(x) -> Decimal:
    return d(x).quantize(CENT, rounding=ROUND_HALF_UP)


# ----------------------------------------------------------------------------- fees
def poly_fee(qty, price, rate, exponent=1, maker=False) -> Decimal:
    if maker or d(rate) == 0: return ZERO
    p = d(price)
    base = p * (ONE - p)
    if d(exponent) != 1: base = base ** d(exponent)
    return (d(qty) * d(rate) * base).quantize(Decimal('0.00001'), rounding=ROUND_HALF_UP)


def kalshi_fee(count, price, multiplier=1, maker=False, maker_fees=False) -> Decimal:
    """Per-order fee, rounded UP to the next cent. Makers pay only on series that have maker fees (rate 0.0175)."""
    if maker and not maker_fees: return ZERO
    rate = Decimal('0.0175') if maker else Decimal('0.07')
    p = d(price)
    raw = d(multiplier) * rate * d(count) * p * (ONE - p)
    return (raw / CENT).to_integral_value(rounding=ROUND_CEILING) * CENT


@dataclass(frozen=True)
class FeeModel:
    venue: str                       # 'polymarket' | 'kalshi'
    rate: Decimal = Decimal('0.07')  # polymarket feeSchedule.rate
    exponent: Decimal = ONE          # polymarket feeSchedule.exponent
    multiplier: Decimal = ONE        # kalshi series multiplier
    maker_fees: bool = False         # kalshi: does this series charge makers?
    estimated: bool = True           # True until the exchange reports the actual fee

    def fee(self, qty, price, maker=False) -> Decimal:
        if self.venue == 'polymarket': return poly_fee(qty, price, self.rate, self.exponent, maker)
        return kalshi_fee(qty, price, self.multiplier, maker, self.maker_fees)


# ----------------------------------------------------------------------------- executable prices
@dataclass(frozen=True)
class Fill:
    requested: Decimal
    qty: Decimal                     # what the displayed depth can actually fill
    cost: Decimal                    # sum(price x qty) -- before fees (a buy pays it, a sell receives it)
    vwap: Optional[Decimal]
    worst: Optional[Decimal]         # the least favourable price touched = the limit an order needs
    best: Optional[Decimal]
    levels: Tuple[Level, ...]
    complete: bool


def walk(levels: Sequence, qty, *, side='buy', limit=None, lot=None, haircut=ONE) -> Fill:
    """Consume price levels (best first) for `qty`. BUY walks asks upward, SELL walks bids downward.
    `limit` stops at that price; `lot` floors to whole contracts; `haircut` (0..1) assumes only that fraction of each level is fillable."""
    want = d(qty)
    got, cost, used = ZERO, ZERO, []
    for price, size in levels:
        p, s = d(price), d(size) * d(haircut)
        if lot: s = (s / d(lot)).to_integral_value(rounding=ROUND_FLOOR) * d(lot)
        if s <= 0: continue
        if limit is not None and ((side == 'buy' and p > d(limit)) or (side == 'sell' and p < d(limit))): break
        take = min(s, want - got)
        if take <= 0: break
        got += take; cost += take * p; used.append((p, take))
        if got >= want: break
    if lot and got > 0:
        whole = (got / d(lot)).to_integral_value(rounding=ROUND_FLOOR) * d(lot)
        if whole < got:   # trim the tail so the quantity is a whole number of lots
            trim, new_used = got - whole, []
            for p, t in reversed(used):
                cut = min(t, trim); trim -= cut
                if t - cut > 0: new_used.append((p, t - cut))
            used = list(reversed(new_used)); got = whole; cost = sum((p * t for p, t in used), ZERO)
    return Fill(want, got, cost, (cost / got) if got > 0 else None, used[-1][0] if used else None, used[0][0] if used else None, tuple(used), got >= want and got > 0)


def buy_all_in(fill: Fill, fm: FeeModel, maker=False) -> Decimal:
    """Total cash needed to buy this fill: cost + fee (fee computed per level for Polymarket, per order for Kalshi)."""
    return fill.cost + fee_for_fill(fill, fm, maker)


def fee_for_fill(fill: Fill, fm: FeeModel, maker=False) -> Decimal:
    if fm.venue == 'kalshi':   # per ORDER, at the order's average price (documented rounding is per order)
        return fm.fee(fill.qty, fill.vwap, maker) if fill.qty > 0 else ZERO
    return sum((fm.fee(t, p, maker) for p, t in fill.levels), ZERO)


def sell_net(fill: Fill, fm: FeeModel, maker=False) -> Decimal:
    """Cash received for selling this fill, after the exit fee."""
    return fill.cost - fee_for_fill(fill, fm, maker)


# ----------------------------------------------------------------------------- positions (average-cost accounting)
@dataclass
class Position:
    qty: Decimal = ZERO
    paid: Decimal = ZERO             # cost basis of the REMAINING quantity, entry fees included
    entry_fees: Decimal = ZERO       # entry fees still attached to the remaining quantity
    realized: Decimal = ZERO
    bought: Decimal = ZERO
    sold: Decimal = ZERO
    exit_proceeds: Decimal = ZERO
    exit_fees: Decimal = ZERO
    fees_total: Decimal = ZERO
    buy_cost: Decimal = ZERO         # sum(qty x price) of every buy, fees excluded

    def buy(self, qty, price, fee=ZERO):
        q, p, f = d(qty), d(price), d(fee)
        self.qty += q; self.paid += q * p + f; self.entry_fees += f; self.bought += q; self.fees_total += f; self.buy_cost += q * p

    @property
    def avg_cost(self) -> Decimal:       # all-in average (fees included)
        return self.paid / self.qty if self.qty > 0 else ZERO

    @property
    def avg_price(self) -> Decimal:      # price only
        return (self.paid - self.entry_fees) / self.qty if self.qty > 0 else ZERO

    def sell(self, qty, price, fee=ZERO) -> Decimal:
        q, p, f = d(qty), d(price), d(fee)
        if q > self.qty + Decimal('1e-12'): raise ValueError(f'cannot sell {q}: only {self.qty} held')
        frac = q / self.qty if self.qty > 0 else ZERO
        removed, ef = self.paid * frac, self.entry_fees * frac
        proceeds = q * p
        pnl = proceeds - f - removed
        self.qty -= q; self.paid -= removed; self.entry_fees -= ef
        self.realized += pnl; self.sold += q; self.exit_proceeds += proceeds; self.exit_fees += f; self.fees_total += f
        return pnl

    def settle(self, value_per_unit) -> Decimal:
        """Close the remaining quantity at its settlement value (1 or 0). No settlement fee on either venue's general schedule."""
        payout = self.qty * d(value_per_unit)
        pnl = payout - self.paid
        self.realized += pnl; self.exit_proceeds += payout
        self.qty = ZERO; self.paid = ZERO; self.entry_fees = ZERO
        return pnl

    def unrealized(self, exit_fill: Fill, fm: FeeModel) -> Optional[Decimal]:
        """Mark to EXECUTABLE exit value (bids, depth, exit fee). None when the book cannot absorb the position."""
        if self.qty <= 0: return ZERO
        if exit_fill.qty < self.qty: return None
        return sell_net(exit_fill, fm) - self.paid


# ----------------------------------------------------------------------------- complementary pair (one winner pays $1)
@dataclass(frozen=True)
class PairEdge:
    qty: Decimal
    fill_a: Fill
    fill_b: Fill
    gross: Decimal
    fees: Decimal
    slippage: Decimal
    safety: Decimal
    net: Decimal
    capital: Decimal
    ret: Decimal
    complete: bool


def pair_edge(asks_a, asks_b, qty, fm_a: FeeModel, fm_b: FeeModel, *, payout=ONE, slippage_per_unit=ZERO, safety_per_unit=ZERO,
              lot=None, haircut=ONE) -> PairEdge:
    """Buy `qty` on leg A and leg B (executable asks). net = qty*payout - cost - fees - slippage - safety.
    Only valid when the two contracts are VERIFIED to settle as exact complements."""
    fa, fb = walk(asks_a, qty, lot=lot, haircut=haircut), walk(asks_b, qty, lot=lot, haircut=haircut)
    q = min(fa.qty, fb.qty)
    if q != fa.qty: fa = walk(asks_a, q, lot=lot, haircut=haircut)
    if q != fb.qty: fb = walk(asks_b, q, lot=lot, haircut=haircut)
    fees = fee_for_fill(fa, fm_a) + fee_for_fill(fb, fm_b)
    slip, safety = q * d(slippage_per_unit) * 2, q * d(safety_per_unit)
    gross = q * d(payout) - fa.cost - fb.cost
    net = gross - fees - slip - safety
    cap = fa.cost + fb.cost + fees
    return PairEdge(q, fa, fb, gross, fees, slip, safety, net, cap, (net / cap) if cap > 0 else ZERO, fa.complete and fb.complete)


# ----------------------------------------------------------------------------- exit card reconciliation
def exit_card(qty, legs, *, label=''):
    """Build and CROSS-CHECK the cross-platform exit card.
    legs: list of dicts {venue, side, entry_avg, entry_fee, exit_price, exit_fee}  (prices in dollars, fees in dollars for the whole leg).
    Returns the displayed numbers plus `issues`, a list of any internal contradictions."""
    q = d(qty)
    cost_basis = sum((q * d(l['entry_avg']) for l in legs), ZERO)
    entry_fees = sum((d(l.get('entry_fee', 0)) for l in legs), ZERO)
    sell_price = sum((d(l['exit_price']) for l in legs), ZERO)
    proceeds = q * sell_price
    exit_fees = sum((d(l.get('exit_fee', 0)) for l in legs), ZERO)
    paid = cost_basis + entry_fees
    net = proceeds - exit_fees - paid
    out = {'qty': q, 'combined_entry': sum((d(l['entry_avg']) for l in legs), ZERO), 'combined_sell': sell_price, 'cost_basis': cost_basis,
           'entry_fees': entry_fees, 'paid': paid, 'proceeds': proceeds, 'exit_fees': exit_fees, 'net_pnl': net,
           'return_pct': (net / paid * 100) if paid > 0 else ZERO, 'issues': []}
    return out


def check_displayed_card(card, *, displayed_cost, displayed_paid, displayed_pnl, tol=Decimal('0.011')):
    """Compare what a UI showed against the reconciled numbers; returns human-readable contradictions."""
    issues = []
    if abs(card['paid'] - d(displayed_paid)) > tol:
        issues.append(f"paid amount {money(displayed_paid)} != cost basis + entry fees {money(card['paid'])}")
    if abs(card['cost_basis'] - d(displayed_cost)) > tol:
        issues.append(f"displayed cost {money(displayed_cost)} != qty x entry prices {money(card['cost_basis'])}")
    if abs(card['net_pnl'] - d(displayed_pnl)) > tol:
        issues.append(f"displayed P&L {money(displayed_pnl)} != proceeds - exit fees - all-in paid = {money(card['net_pnl'])}")
    return issues
