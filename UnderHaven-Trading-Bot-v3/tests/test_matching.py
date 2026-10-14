"""Discovery normalisation and STRICT contract matching (synthetic fixtures; not market data)."""
import json
from fake_btc import BASE, KAL_RULES, POLY_RULES, FakeKalshi, FakePm
from app.btc.discovery import normalize_kalshi, normalize_polymarket, kalshi_duration
from app.btc.matching import match_btc

SAME = ('Resolves Up if the Chainlink BTC/USD 60-second simple average at the end is greater than or equal to the 60-second simple average at the beginning.')


def mk_pair(poly_rules=SAME, kal_rules=SAME, pdur=900, kdur=900, kstart=BASE, **kw):
    pm = FakePm(); mid = pm.add_window(pdur, BASE, rules=poly_rules)
    raw = pm.events[f'btc-updown-{"5m" if pdur == 300 else "15m"}-{BASE}'][0]['markets'][0]
    P = normalize_polymarket(raw, pdur, BASE)
    k = FakeKalshi(); t = k.add(kdur, kstart, rules=kal_rules)
    K = normalize_kalshi(k.markets[t])
    for a, b in kw.items(): setattr(K, a, b)
    return P, K


def verdicts(r): return {c.name: c.verdict for c in r.checks}


def test_identical_settlement_rules_verify():
    P, K = mk_pair(); r = match_btc(P, K)
    assert r.status == 'VERIFIED', r.reasons


def test_real_world_style_rules_are_rejected_because_sources_and_averaging_differ():
    P, K = mk_pair(POLY_RULES, KAL_RULES); r = match_btc(P, K)
    v = verdicts(r)
    assert r.status == 'REJECTED' and v['Settlement source'] == 'REJECT' and v['Price method'] == 'REJECT'


def test_different_tie_rule_is_rejected():
    P, K = mk_pair(kal_rules=SAME.replace('greater than or equal to', 'strictly greater than'))
    assert match_btc(P, K).status == 'REJECTED' and verdicts(match_btc(P, K))['Tie rule'] == 'REJECT'


def test_five_minute_and_fifteen_minute_contracts_are_never_interchangeable():
    P, K = mk_pair(pdur=300, kdur=900)
    r = match_btc(P, K); assert r.status == 'REJECTED' and verdicts(r)['Duration'] == 'REJECT'


def test_window_start_and_expiration_must_line_up():
    P, K = mk_pair(kstart=BASE + 900)             # the next window
    v = verdicts(match_btc(P, K)); assert v['Window start'] == 'REJECT' and v['Expiration'] == 'REJECT'


def test_unknown_rules_need_review_and_are_not_tradable():
    P, K = mk_pair(poly_rules='Bitcoin up or down.', kal_rules='BTC up or down.')
    r = match_btc(P, K); assert r.status == 'NEEDS_REVIEW'


def test_closed_or_untradable_market_is_rejected():
    P, K = mk_pair(); K.tradable = False; K.status = 'closed'
    assert verdicts(match_btc(P, K))['Market status'] == 'REJECT'


def test_wrong_outcome_labels_are_flagged_not_assumed():
    pm = FakePm(); pm.add_window(900, BASE, rules=SAME, outcomes='["Yes", "No"]')
    raw = pm.events[f'btc-updown-15m-{BASE}'][0]['markets'][0]; raw['outcomes'] = json.dumps(['Yes', 'No'])
    P = normalize_polymarket(raw, 900, BASE); k = FakeKalshi(); K = normalize_kalshi(k.markets[k.add(900, BASE, rules=SAME)])
    assert not P.outcome_ok and verdicts(match_btc(P, K))['UP/DOWN meaning'] == 'REVIEW'


def test_polymarket_v2_position_ids_min_notional_and_fee_schedule():
    raw = {'id': '7', 'question': 'q', 'description': SAME, 'version': 'v2', 'positionIds': json.dumps(['pu', 'pd']), 'clobTokenIds': None, 'outcomes': '["Up","Down"]',
           'active': True, 'closed': False, 'acceptingOrders': True, 'enableOrderBook': True, 'orderMinSize': 5, 'orderPriceMinTickSize': 0.001, 'secondsDelay': 3,
           'feesEnabled': True, 'feeSchedule': {'rate': 0.05, 'exponent': 2}}
    m = normalize_polymarket(raw, 900, BASE)
    assert (m.up_id, m.down_id) == ('pu', 'pd') and m.min_notional == 5 and m.seconds_delay == 3 and str(m.tick) == '0.001'
    assert str(m.fee_rate) == '0.05' and str(m.fee_exponent) == '2' and m.fee_known and m.tradable
    nofee = normalize_polymarket(dict(raw, feesEnabled=False, feeSchedule=None), 900, BASE); assert nofee.fee_rate == 0 and nofee.fee_known
    est = normalize_polymarket(dict(raw, feesEnabled=None, feeSchedule=None), 900, BASE); assert not est.fee_known      # estimate is labelled as such


def test_kalshi_duration_detection_ignores_other_markets():
    assert kalshi_duration({'series_ticker': 'KXBTC15M'}) == 900 and kalshi_duration({'series_ticker': 'KXBTC5M'}) == 300
    assert kalshi_duration({'series_ticker': 'KXBTCD', 'title': 'Bitcoin above $60,000 today?'}) is None
    assert normalize_kalshi({'ticker': 'KXETH15M-1', 'series_ticker': 'KXETH15M', 'title': 'ETH price up in next 15 mins?', 'expiration_time': '2027-01-15T08:00:00Z', 'status': 'active'}) is None
