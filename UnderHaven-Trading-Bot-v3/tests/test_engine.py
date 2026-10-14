"""Strategies, risk, paper execution, settlement, persistence and the scanner service (fixture exchanges, never live data)."""
import os, sys, time, threading
from decimal import Decimal as D
from fake_btc import BASE, KAL_RULES, POLY_RULES, Clock, FakeKalshi, FakePm, make_feed
from app import db
from app.btc import store, strategies as st, settings as bs
from app.btc.books import Book, MarketObs
from app.btc.calc import Position
from app.btc.executor import Executor, OrderResult, PaperGateway
from app.btc.service import BtcService
from app.btc.settlement import Settler

SAME = ('Resolves Up if the Chainlink BTC/USD 60-second simple average at the end is greater than or equal to the 60-second simple average at the beginning.')


def reset():
    with db.connect() as c:
        for t in ('b_signals', 'b_orders', 'b_positions', 'b_accounts', 'b_events', 'b_health', 'b_ref', 'kalshi_credentials'): c.execute(f'DELETE FROM {t}')
        c.execute("DELETE FROM settings WHERE key LIKE 'b\\_%' ESCAPE '\\'")


def world(start_price=60000.0, now_price=60300.0, elapsed=200, **cfg):
    reset(); clock = Clock(BASE + elapsed); store.now = clock; pm, k = FakePm(), FakeKalshi()
    feed = make_feed(clock, BASE, start_price, now_price)
    svc = BtcService(pm, feed, kalshi=k, now=clock, paper_sleep=lambda s: None)
    if cfg: setting(svc, **cfg)
    return svc, pm, k, clock


def setting(svc, **kv):
    bs.save(db.set_setting, bs.validate(kv)); svc._S_at = -1e12


def rows(sql, *a):
    with db.connect() as c: return [dict(r) for r in c.execute(sql, a)]


def reasons(svc): return [r['reason'] for r in rows('SELECT reason FROM b_signals WHERE accepted=0')]


# ---------------------------------------------------------------------------------------------------------------- strategies & filters
def test_momentum_trades_only_when_the_market_lags_a_real_btc_move():
    svc, pm, k, clock = world(now_price=60300)                    # BTC +0.5% since the window opened
    pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    svc.cycle()
    pos = rows('SELECT * FROM b_positions'); o = rows('SELECT * FROM b_orders')
    assert len(pos) == 1 and pos[0]['strategy'] == 'momentum' and pos[0]['side'] == 'UP' and o[0]['status'] == 'FILLED' and o[0]['tif'] == 'fok'
    assert abs(o[0]['avg_price'] - 0.60) < 1e-9                  # filled at the real displayed ask, not a better price


def test_no_trade_when_the_market_is_already_fairly_priced():
    svc, pm, k, clock = world(now_price=60300)
    pm.add_window(900, BASE, up=(0.97, 200), down=(0.05, 200), up_bid=0.95, down_bid=0.03, rules=SAME)
    svc.cycle()
    assert not rows('SELECT * FROM b_orders') and any('EDGE_BELOW_MINIMUM' in r for r in reasons(svc))


def test_no_signal_without_a_real_window_start_price_or_with_a_stale_feed():
    svc, pm, k, clock = world(); pm.add_window(900, BASE, up=(0.5, 200), down=(0.5, 200), rules=SAME)
    svc.feed.history.clear(); svc.cycle()
    assert any('NO_FRESH_BTC_PRICE' in r for r in reasons(svc)) and not rows('SELECT * FROM b_orders')
    svc2, pm2, _, c2 = world(); pm2.add_window(900, BASE, up=(0.5, 200), down=(0.5, 200), rules=SAME)
    c2.advance(60); svc2.cycle()                                 # the feed stopped a minute ago
    assert any('NO_FRESH_BTC_PRICE' in r for r in reasons(svc2))


def test_expiration_aware_entry_window():
    for elapsed, frag in ((20, 'TOO_EARLY'), (880, 'TOO_LATE')):
        svc, pm, k, clock = world(elapsed=elapsed); pm.add_window(900, BASE, up=(0.6, 200), down=(0.4, 200), rules=SAME)
        svc.cycle(); assert any(frag in r for r in reasons(svc)), (elapsed, reasons(svc)); assert not rows('SELECT * FROM b_orders')


def test_stale_thin_and_invalid_books_are_rejected():
    S = bs.BtcSettings(); now = 1000.0; m = type('M', (), {})()
    good = MarketObs(m, now, Book([(D('0.5'), D('50'))], [(D('0.48'), D('50'))]), Book([(D('0.5'), D('50'))], [(D('0.48'), D('50'))]))
    assert st.book_ok(good, 'UP', S, now) == (True, '')
    assert 'STALE_BOOK' in st.book_ok(good, 'UP', S, now + 60)[1]
    thin = MarketObs(m, now, Book([(D('0.5'), D('2'))], [(D('0.48'), D('2'))]), good.down); assert 'THIN_BOOK' in st.book_ok(thin, 'UP', S, now)[1]
    crossed = MarketObs(m, now, Book([(D('0.5'), D('50'))], [(D('0.52'), D('50'))]), good.down); assert 'INVALID_BOOK' in st.book_ok(crossed, 'UP', S, now)[1]
    wide = MarketObs(m, now, Book([(D('0.70'), D('50'))], [(D('0.40'), D('50'))]), good.down); assert 'SPREAD' in st.book_ok(wide, 'UP', S, now)[1]
    assert 'NO_ASKS' in st.book_ok(MarketObs(m, now, Book([], [(D('0.4'), D('5'))]), good.down), 'UP', S, now)[1]


def test_polymarket_minimum_order_is_a_usdc_notional():
    svc, pm, k, clock = world(now_price=60300, max_order_notional=10, max_capital_per_trade=10)
    pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME, orderMinSize=50)
    svc.cycle(); assert not [o for o in rows('SELECT * FROM b_orders') if o['venue'] == 'polymarket'] and any('BELOW_MIN_ORDER' in r for r in reasons(svc))


def test_mean_reversion_needs_a_jump_a_dislocation_and_no_model_confirmation():
    svc, pm, k, clock = world(now_price=60000)                   # BTC unchanged: the model says ~50/50
    mid = pm.add_window(900, BASE, up=(0.50, 200), down=(0.52, 200), up_bid=0.48, down_bid=0.50, rules=SAME)
    setting(svc, s_momentum=False, s_bundle=False, s_arb=False)
    svc.cycle(); assert any('NO_PRICE_HISTORY' in r for r in reasons(svc))
    clock.advance(1); pm.set_books(mid, (0.80, 200), (0.22, 200), 0.78, 0.20)          # the market jumps up with no BTC move
    svc.feed.history.append((clock(), 60001.0, 'fixture')); svc.cycle()
    pos = rows('SELECT * FROM b_positions')
    assert len(pos) == 1 and pos[0]['strategy'] == 'meanrev' and pos[0]['side'] == 'DOWN'      # fade the overreaction toward the model value


def test_bundle_needs_a_real_gap_after_fees():
    svc, pm, k, clock = world(now_price=60000)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.52, 200), rules=SAME); setting(svc, s_momentum=False, s_meanrev=False, s_arb=False)
    svc.cycle(); assert not rows('SELECT * FROM b_orders') and any('pair costs 1.020' in r for r in reasons(svc))
    pm.set_books('P900-%d' % BASE, (0.40, 200), (0.46, 200), 0.38, 0.44); clock.advance(3); svc.feed.history.append((clock(), 60000.0, 'f')); svc.cycle()
    assert len(rows("SELECT * FROM b_positions WHERE strategy='bundle'")) == 2


def test_cross_platform_arbitrage_requires_a_verified_pair():
    svc, pm, k, clock = world(now_price=60000)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.52, 200), down_bid=0.50, rules=POLY_RULES); k.add(900, BASE, yes_ask=0.60, no_ask=0.42, rules=KAL_RULES)     # a juicy-looking gap
    setting(svc, s_momentum=False, s_meanrev=False, s_bundle=False, max_unmatched=20); svc.cycle()
    assert not rows('SELECT * FROM b_orders') and any('PAIR_NOT_VERIFIED' in r for r in reasons(svc))
    assert svc.pairs and svc.pairs[0].status == 'REJECTED'


def test_verified_pair_executes_both_legs_and_settles_one_winner():
    svc, pm, k, clock = world(now_price=60000)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.52, 200), down_bid=0.50, rules=SAME); k.add(900, BASE, yes_ask=0.60, no_ask=0.42, rules=SAME)
    setting(svc, s_momentum=False, s_meanrev=False, s_bundle=False, max_unmatched=20); svc.cycle()
    pos = rows('SELECT * FROM b_positions ORDER BY id'); assert len(pos) == 2 and {p['venue'] for p in pos} == {'polymarket', 'kalshi'} and {p['side'] for p in pos} == {'UP', 'DOWN'}
    assert pos[0]['qty'] == pos[1]['qty'] and pos[0]['group_id'] == pos[1]['group_id']
    clock.advance(800); pm.results['P900-%d' % BASE] = {'id': 'x', 'outcomes': '["Up","Down"]', 'closed': True, 'outcomePrices': '["1","0"]'}
    k.markets[next(iter(k.markets))]['result'] = 'yes'
    svc.paper_settler.run()
    done = rows('SELECT * FROM b_positions'); assert all(p['status'] == 'SETTLED' for p in done)
    t = store.trades('PAPER')[0]; assert t['done'] and t['pnl'] > 0                        # exactly one leg won: the pair paid $1
    assert not rows("SELECT * FROM b_events WHERE kind='SETTLEMENT_MISMATCH'")


def test_settlement_mismatch_between_venues_is_recorded_not_hidden():
    svc, pm, k, clock = world(now_price=60000)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.52, 200), down_bid=0.50, rules=SAME); k.add(900, BASE, yes_ask=0.60, no_ask=0.42, rules=SAME)
    setting(svc, s_momentum=False, s_meanrev=False, s_bundle=False, max_unmatched=20); svc.cycle()
    clock.advance(800)
    pm.results['P900-%d' % BASE] = {'id': 'x', 'outcomes': '["Up","Down"]', 'closed': True, 'outcomePrices': '["1","0"]'}     # Polymarket: Up won
    k.markets[next(iter(k.markets))]['result'] = 'no'                                                                          # Kalshi: Down won => the UP/DOWN pair has TWO winners
    svc.paper_settler.run()
    wins = sum(1 for p in rows('SELECT * FROM b_positions') if p['settle_value'] == 1)
    assert wins != 1 and rows("SELECT * FROM b_events WHERE kind='SETTLEMENT_MISMATCH'")


def test_open_positions_are_never_counted_as_wins_before_a_verified_result():
    svc, pm, k, clock = world(now_price=60300); pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME); svc.cycle()
    assert store.performance('PAPER')['trades'] == 0 and store.performance('PAPER')['wins'] == 0
    clock.advance(800); svc.paper_settler.run()                                           # window over, but the exchange has not reported a result
    assert rows('SELECT status FROM b_positions')[0]['status'] == 'AWAITING_SETTLEMENT' and store.performance('PAPER')['trades'] == 0
    pm.results['P900-%d' % BASE] = {'id': 'x', 'outcomes': '["Up","Down"]', 'closed': True, 'outcomePrices': '["1","0"]'}; svc.paper_settler.run()
    perf = store.performance('PAPER'); assert perf['trades'] == 1 and perf['wins'] == 1 and perf['net_pnl'] > 0
    assert store.cash('PAPER', 'polymarket') > store.start_balance('PAPER', 'polymarket')


# ---------------------------------------------------------------------------------------------------------------- execution
def _sig(svc, qty=10, price='0.60', venue='polymarket', side='UP', strategy='momentum'):
    from app.btc.discovery import normalize_polymarket
    raw = svc.pm.events[f'btc-updown-15m-{BASE}'][0]['markets'][0]; m = normalize_polymarket(raw, 900, BASE) if venue == 'polymarket' else None
    fm = st.fee_model(m); leg = st.LegSpec(m, side, D(qty), D(price), D(price), D('0.2'), fm)
    return st.Signal(strategy, [leg], D('0.2'), D('2'), 0.9, {}), m


def test_paper_fill_consumes_real_depth_and_partial_fills_are_recorded():
    svc, pm, k, clock = world(); pm.add_window(900, BASE, up=[(0.60, 4), (0.65, 4)], down=(0.42, 200), rules=SAME)
    sig, m = _sig(svc, qty=12, price='0.70'); gw = svc.paper_exec.gw['polymarket']
    fok = gw.place(m, 'UP', 'buy', D(12), D('0.70'), 'fok', 'r1'); assert fok.status == 'UNFILLED'      # 8 shown, haircut 0.7 -> 5 fillable < 12
    ioc = gw.place(m, 'UP', 'buy', D(12), D('0.70'), 'ioc', 'r2'); assert ioc.status == 'PARTIAL' and 0 < ioc.qty < 12 and ioc.fees > 0
    capped = gw.place(m, 'UP', 'buy', D(12), D('0.60'), 'ioc', 'r3'); assert capped.qty <= 4        # the price limit is respected


def test_duplicate_requests_never_double_up_exposure():
    svc, pm, k, clock = world(now_price=60300); pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    setting(svc, s_meanrev=False, s_bundle=False, s_arb=False); svc.cycle(); n1 = len(rows('SELECT * FROM b_orders'))
    clock.advance(5); svc.feed.history.append((clock(), 60300.0, 'f')); svc.cycle()
    assert len(rows('SELECT * FROM b_orders')) == n1 and len(rows('SELECT * FROM b_positions')) == 1
    sig, m = _sig(svc); r = svc.paper_exec.execute(sig); assert not r['ok'] and 'DUPLICATE' in r['reason']


def test_timeout_is_recorded_as_uncertain_and_no_position_is_assumed():
    svc, pm, k, clock = world(); pm.add_window(900, BASE, up=(0.6, 200), down=(0.4, 200), rules=SAME)
    class Boom:
        mode = 'PAPER'
        def fetch(self, m): return None
        def balance(self): return D('100')
        def place(self, *a, **k): raise TimeoutError('read timed out')
    sig, m = _sig(svc); ex = Executor('PAPER', {'polymarket': Boom(), 'kalshi': Boom()}, svc.risk, svc.settings)
    r = ex.execute(sig); assert not r['ok'] and 'UNCERTAIN' in r['reason'] and not rows('SELECT * FROM b_positions')
    assert rows("SELECT * FROM b_events WHERE kind='ORDER_OUTCOME_UNKNOWN'")


def _two_leg(svc, pm, k, qty=10):
    from app.btc.discovery import normalize_polymarket, normalize_kalshi
    praw = pm.events[f'btc-updown-15m-{BASE}'][0]['markets'][0]; P = normalize_polymarket(praw, 900, BASE); K = normalize_kalshi(next(iter(k.markets.values())))
    a = st.LegSpec(P, 'UP', D(qty), D('0.55'), D('0.50'), D('0.1'), st.fee_model(P)); b = st.LegSpec(K, 'DOWN', D(qty), D('0.45'), D('0.42'), D('0.1'), st.fee_model(K))
    return st.Signal('arb', [a, b], D('0.1'), D('1'), None, {}), P, K


def test_one_leg_fills_and_the_other_fails_the_excess_is_unwound_or_the_kill_switch_trips():
    svc, pm, k, clock = world(leg_order='polymarket_first', retry_count=1, max_unmatched=20)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.50, 200), up_bid=0.48, rules=SAME); k.add(900, BASE, yes_ask=0.60, no_ask=0.42, depth=3)      # Kalshi shows only 3 contracts
    sig, P, K = _two_leg(svc, pm, k, qty=10); r = svc.paper_exec.execute(sig)
    held = {p['venue']: p for p in rows('SELECT * FROM b_positions')}
    assert r['ok'] and held['kalshi']['qty'] <= 3 and abs((held['polymarket']['bought'] - held['polymarket']['sold']) - held['kalshi']['bought']) < 1e-6       # unmatched part was sold back
    assert rows("SELECT * FROM b_orders WHERE action='sell' AND venue='polymarket'") and rows("SELECT * FROM b_events WHERE kind='UNMATCHED_LEG'")


def test_unwind_failure_raises_a_manual_action_alert_and_trips_the_kill_switch():
    svc, pm, k, clock = world(leg_order='polymarket_first', retry_count=0, max_unmatched=20)
    pm.add_window(900, BASE, up=(0.50, 200), down=(0.50, 200), rules=SAME); k.add(900, BASE, yes_ask=0.60, no_ask=0.42, depth=0 + 1)
    pm.books_by_token[f'P900-{BASE}-U']['bids'] = []                                         # nobody is bidding: the excess cannot be sold back
    sig, P, K = _two_leg(svc, pm, k, qty=10); svc.paper_exec.execute(sig)
    assert rows("SELECT * FROM b_events WHERE kind='MANUAL_ACTION_REQUIRED'") and svc.risk.killed()
    assert svc.paper_exec.execute(_sig(svc)[0])['ok'] is False                                # no new entries while the switch is on


# ---------------------------------------------------------------------------------------------------------------- risk
def test_backend_risk_limits_block_oversize_overexposed_and_unaffordable_trades():
    svc, pm, k, clock = world(now_price=60300, max_total_exposure=5, max_capital_per_trade=10)
    pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME); svc.cycle()
    assert not rows('SELECT * FROM b_orders') and any('TOTAL_EXPOSURE_LIMIT' in r for r in reasons(svc))
    svc2, pm2, _, _ = world(now_price=60300, balance_polymarket=1); pm2.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    store.account_set_start('PAPER', 'polymarket', 1); svc2._S_at = -1e12; svc2.cycle()
    assert any('INSUFFICIENT_PAPER_BALANCE' in r for r in reasons(svc2))


def test_daily_loss_and_drawdown_trip_a_persistent_kill_switch():
    svc, pm, k, clock = world(now_price=60300, max_daily_loss=1)
    pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    from app.btc.discovery import normalize_polymarket
    m = normalize_polymarket(pm.events[f'btc-updown-15m-{BASE}'][0]['markets'][0], 900, BASE)
    p = Position(); p.buy(10, '0.9'); pid = store.position_create('PAPER', 'old', 'momentum', m, 'UP', p); p.settle(0); store.position_update(pid, p, 'SETTLED', 0)     # a -$9 loss
    svc.cycle(); assert svc.risk.killed() and any('DAILY_LOSS_LIMIT' in r or 'KILL_SWITCH' in r for r in reasons(svc)) and not rows("SELECT * FROM b_orders")
    svc2 = BtcService(pm, svc.feed, kalshi=k, now=clock, paper_sleep=lambda s: None); assert svc2.risk.killed()      # survives a restart
    svc2.risk.clear_kill(); assert not svc2.risk.killed()


def test_paper_mode_can_never_reach_a_live_gateway_and_live_is_off_by_default():
    svc, pm, k, clock = world(now_price=60300)
    class Live:
        mode = 'LIVE'
        def fetch(self, m): raise AssertionError('live gateway touched')
        def place(self, *a, **k): raise AssertionError('REAL ORDER ATTEMPTED FROM PAPER MODE')
    assert svc.live is False and svc.live_exec is None
    pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME); svc.live_exec = Executor('LIVE', {'polymarket': Live(), 'kalshi': Live()}, svc.risk, svc.settings)
    svc.cycle(); assert rows("SELECT * FROM b_orders WHERE mode='PAPER'") and not rows("SELECT * FROM b_orders WHERE mode='LIVE'")
    sig, _ = _sig(svc); ok, why = svc.risk.check_entry(sig, 'LIVE', live_enabled=False); assert not ok and any('LIVE_MODE_NOT_ENABLED' in w for w in why) and any('NOT_PERMITTED' in w for w in why)


def test_live_enable_requires_credentials_confirmation_and_a_passing_preflight():
    svc, pm, k, clock = world(); pm.add_window(900, BASE, up=(0.5, 200), down=(0.5, 200), rules=SAME); svc.cycle()
    ok, msg = svc.enable_live('yes please'); assert not ok and 'ENABLE LIVE TRADING' in msg and svc.live is False
    ok, msg = svc.enable_live('ENABLE LIVE TRADING'); assert not ok and 'Preflight failed' in msg and svc.live is False
    pf = svc.preflight(); assert not pf['ok'] and any(c['name'] == 'Polymarket credentials' and not c['ok'] for c in pf['checks'])
    assert any(c['name'] == 'Kalshi credentials' and not c['ok'] for c in pf['checks'])


def test_exits_take_profit_sells_into_real_bids_and_hedged_pairs_are_left_alone():
    svc, pm, k, clock = world(now_price=60300, take_profit=0.10)
    mid = pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME); setting(svc, s_meanrev=False, s_bundle=False, s_arb=False)
    svc.cycle(); assert rows('SELECT * FROM b_positions')[0]['status'] == 'OPEN'
    pm.set_books(mid, (0.80, 200), (0.22, 200), 0.78, 0.20); clock.advance(30); svc.feed.history.append((clock(), 60300.0, 'f')); svc.cycle()
    p = rows('SELECT * FROM b_positions')[0]; assert p['status'] == 'CLOSED' and p['realized'] > 0 and abs(p['exit_proceeds'] - p['sold'] * 0.78) < 1e-6


# ---------------------------------------------------------------------------------------------------------------- reliability
def test_unavailable_market_data_is_shown_as_an_error_and_nothing_is_traded():
    svc, pm, k, clock = world(now_price=60300); pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), rules=SAME); pm.fail_books = True
    svc.cycle(); assert not rows('SELECT * FROM b_orders') and 'polymarket' in svc.errors and svc.errors['polymarket'] and any('NO_ORDER_BOOK' in r for r in reasons(svc))


def test_restart_with_open_positions_keeps_state_and_blocks_duplicates():
    svc, pm, k, clock = world(now_price=60300); pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    setting(svc, s_meanrev=False, s_bundle=False, s_arb=False); svc.cycle(); cash1 = store.cash('PAPER', 'polymarket'); n = len(rows('SELECT * FROM b_orders'))
    svc2 = BtcService(pm, svc.feed, kalshi=k, now=clock, paper_sleep=lambda s: None)            # simulates a service restart
    assert store.cash('PAPER', 'polymarket') == cash1 and len(store.open_positions('PAPER')) == 1 and svc2.live is False
    clock.advance(5); svc2.feed.history.append((clock(), 60300.0, 'f')); svc2.cycle(); assert len(rows('SELECT * FROM b_orders')) == n


def test_scanner_runs_in_a_background_thread_without_any_browser_and_a_second_worker_is_refused():
    svc, pm, k, clock = world(now_price=60300); pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME)
    svc.feed.start = lambda interval=1.0: None; setting(svc, scan_interval=1)
    assert svc.start() and svc._thread.is_alive()
    t0 = time.time()
    while time.time() - t0 < 5 and svc.cycles < 2: time.sleep(0.05)
    assert svc.cycles >= 2 and svc.last_ok is not None
    other = BtcService(pm, svc.feed, kalshi=k, now=clock, paper_sleep=lambda s: None); other.feed.start = lambda interval=1.0: None
    assert other.start() is False                                    # the file lock prevents duplicate workers
    svc.stop(); svc._thread.join(3)


def test_settings_are_validated_and_a_corrupt_stored_value_cannot_crash_the_scanner():
    for bad in ({'min_net_edge': '-1'}, {'max_spread': 'abc'}, {'recovery': 'yolo'}, {'live_strategies': 'martingale'}, {'max_order_notional': '1e12'}):
        try: bs.validate(bad); raise AssertionError(f'accepted {bad}')
        except ValueError: pass
    db.set_setting('b_min_net_edge', 'garbage'); db.set_setting('b_max_spread', '0.2')
    S = bs.load(db.get_setting); assert S.min_net_edge == bs.BtcSettings().min_net_edge and S.max_spread == 0.2
    db.set_setting('b_min_net_edge', ''); db.set_setting('b_max_spread', '')
