"""Strict cross-platform contract matching for BTC Up/Down windows. A pair is VERIFIED only if every settlement-relevant fact is PROVEN
equal. Titles are never compared for similarity. Anything unknown is REVIEW (not tradable); anything proven different is REJECT.
Two BTC "up or down" contracts can look identical and still pay out differently, for example when one settles on a single Chainlink price
and the other on a 60-second average of a different index, so the oracle, averaging method and tie rule must all match."""
from .models import Check, MatchResult


def match_btc(poly, kal, start_tol=15.0, end_tol=15.0) -> MatchResult:
    c = []
    add = lambda n, v, t='': c.append(Check(n, v, t))
    add('Underlying asset', 'PASS', 'BTC on both')
    add('Duration', 'PASS' if poly.duration == kal.duration else 'REJECT', f'{poly.duration // 60}-minute vs {kal.duration // 60}-minute')
    ds, de = abs(poly.start_ts - kal.start_ts), abs(poly.end_ts - kal.end_ts)
    add('Window start', 'PASS' if ds <= start_tol else 'REJECT', f'{ds:.0f}s apart')
    add('Expiration', 'PASS' if de <= end_tol else 'REJECT', f'{de:.0f}s apart')
    add('Market status', 'PASS' if poly.tradable and kal.tradable else 'REJECT', f'polymarket={poly.status}, kalshi={kal.status}')
    add('UP/DOWN meaning', 'PASS' if poly.outcome_ok and kal.outcome_ok else 'REVIEW', '; '.join(poly.flags + kal.flags) or 'YES/Up = price rises')

    def rule(name, a, b):
        if 'UNKNOWN' in (a, b): add(name, 'REVIEW', f'not stated on one side (polymarket={a}, kalshi={b})')
        elif a == b: add(name, 'PASS', a)
        else: add(name, 'REJECT', f'polymarket={a} vs kalshi={b}')
    rule('Settlement source', poly.oracle, kal.oracle)
    rule('Price method', poly.averaging, kal.averaging)
    rule('Tie rule', poly.tie_rule, kal.tie_rule)
    v = {x.verdict for x in c}
    return MatchResult('REJECTED' if 'REJECT' in v else ('NEEDS_REVIEW' if 'REVIEW' in v else 'VERIFIED'), c)
