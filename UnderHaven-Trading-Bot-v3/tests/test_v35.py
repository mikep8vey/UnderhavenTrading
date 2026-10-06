"""Offline tests for v3.5 (multi-coin scan, re-check, history, security). Run: python3 tests/run_tests.py"""
import os, sys, time, tempfile, json, types, re
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, os.path.dirname(HERE)); sys.path.insert(0, HERE)
os.environ['UNDERHAVEN_DATA_DIR'] = tempfile.mkdtemp(); os.environ['UNDERHAVEN_VAULT_KEY_FILE'] = os.path.join(os.environ['UNDERHAVEN_DATA_DIR'], 'vault.key')
import requests
from unittest import mock
from fake_exchange import FakeExchange, WINDOW

# Import the app with the network blocked, then stop the background thread so tests are deterministic.
with mock.patch.object(requests.Session, 'get', side_effect=RuntimeError('offline')), mock.patch.object(requests.Session, 'post', side_effect=RuntimeError('offline')):
    from app.app import app, bot, group_markets
    from app import db
    from app.assets import ASSETS
bot.running = False; time.sleep(2.3)
from werkzeug.security import generate_password_hash
db.set_setting('admin_user', 'admin'); db.set_setting('admin_hash', generate_password_hash('correct-horse-battery'))


def fresh(ex, **cfg):
    bot.pm._http = ex; bot.pm._batch_off_until = 0.0; bot.pm._market_cache = []; bot.pm._market_cache_key = None; bot.pm.notes = []
    bot.assets = cfg.get('assets', [a['code'] for a in ASSETS]); bot.slots_ahead = cfg.get('slots_ahead', 2)
    bot.confirm = cfg.get('confirm', True); bot.slip_ticks = cfg.get('slip', 0.0); bot.late_skip = cfg.get('late_skip', 0)
    bot.min_net_edge = 0.01; bot.confirm_delay = 0.0; bot._traded = {}; bot._last_paper = {}; bot.cooldown_seconds = 1
    bot.session_id = db.start_paper_session(); bot.scans = 0
    with db.connect() as c:
        for t in ('scan_stats', 'trades', 'opportunities'): c.execute(f'DELETE FROM {t}')


def test_discovers_all_seven_coins_and_skips_dead_windows():
    ex = FakeExchange(); ex.no_gamma.add('DOGE'); ex.nobook.add(('SOL', 1)); fresh(ex)
    bot.step()
    coins = sorted({m['asset'] for m in bot.markets})
    assert coins == ['BNB', 'BTC', 'ETH', 'HYPE', 'SOL', 'XRP'], coins          # DOGE has no events
    sol = [m for m in bot.markets if m['asset'] == 'SOL']
    assert len(sol) == 2, len(sol)                                              # prev window ended (dropped), live OK, +1 has no book yet (skipped), +2 OK
    assert ex.gamma_calls == 7 * 4, ex.gamma_calls                              # 7 coins x (prev + live + 2 ahead)
    assert bot.pm.books_via == 'batch' and ex.batch_calls == 1 and ex.single_calls == 0
    assert 'No orderbook' not in (bot.last_error or ''), 'a 404 "no book yet" must not be an error'


def test_batch_unsupported_falls_back_to_parallel_and_is_remembered():
    ex = FakeExchange(); ex.batch_supported = False; fresh(ex)
    bot.step()
    assert bot.pm.books_via == 'parallel' and ex.single_calls > 0
    n = ex.batch_calls; bot.pm._market_cache_at = 0; bot.step()
    assert ex.batch_calls == n, 'batch endpoint must not be hammered again for 5 minutes'
    assert any('batch' in x for x in bot.pm.notes)


def test_real_gap_is_confirmed_logged_once_per_window():
    ex = FakeExchange(); ex.gap[('SOL', 0)] = (0.44, 0.50); fresh(ex)
    for _ in range(3): bot.pm._market_cache_at = 0; bot.step()
    sol = [m for m in bot.markets if m['asset'] == 'SOL' and m['qualified']]
    assert len(sol) == 1 and sol[0]['confirmed'] and sol[0]['net_edge'] > 0.01, sol
    with db.connect() as c:
        rows = c.execute("SELECT asset,COUNT(*) FROM trades WHERE mode='PAPER' AND session_id=? GROUP BY asset", (bot.session_id,)).fetchall()
    assert [tuple(r) for r in rows] == [('SOL', 1)], [tuple(r) for r in rows]      # 3 scans, same window -> ONE paper trade


def test_phantom_gap_that_vanishes_on_recheck_is_rejected():
    ex = FakeExchange(); ex.phantom[('ETH', 0)] = (0.40, 0.45); fresh(ex)
    bot.step()
    eth = [m for m in bot.markets if m['asset'] == 'ETH' and m['start_ts'] == ex.base()][0]
    assert eth['raw_qualified'] and not eth['qualified'] and eth['reason'] == 'gap gone on re-check', eth['reason']
    with db.connect() as c:
        n = c.execute("SELECT COUNT(*) FROM trades WHERE asset='ETH' AND session_id=?", (bot.session_id,)).fetchone()[0]
    assert n == 0


def test_without_recheck_the_phantom_would_have_been_traded():
    ex = FakeExchange(); ex.phantom[('ETH', 0)] = (0.40, 0.45); fresh(ex, confirm=False)
    bot.step()
    eth = [m for m in bot.markets if m['asset'] == 'ETH' and m['start_ts'] == ex.base()][0]
    assert eth['qualified'], 'sanity: the guard, not the data, is what rejects it'


def test_late_window_and_min_order_guards():
    ex = FakeExchange(); fresh(ex)
    m = {'asset': 'BTC', 'start_ts': ex.base(), 'question': 'q', 'slug': 's', 'clobTokenIds': ['a', 'b'], 'conditionId': 'c'}
    book = lambda p, ms='5': {'tick_size': '0.01', 'min_order_size': ms, 'asks': [{'price': str(p), 'size': '100'}], 'bids': []}
    now = ex.base() + WINDOW - 30
    o = bot.pm.evaluate(m, book(.44), book(.50), 0.01, 10.0, late_skip=60, now=now)
    assert o['raw_qualified'] and not o['qualified'] and o['reason'] == 'too late in window'
    o = bot.pm.evaluate(m, book(.44), book(.50), 0.01, 10.0, late_skip=60, now=ex.base() + 100)
    assert o['qualified']
    o = bot.pm.evaluate(m, book(.44, '50'), book(.50, '50'), 0.01, 10.0, now=ex.base() + 100)
    assert not o['qualified'] and o['reason'] == 'below minimum order size', o['reason']


def test_slippage_allowance_lowers_net_edge():
    ex = FakeExchange(); fresh(ex); m = {'asset': 'BTC', 'start_ts': ex.base(), 'question': 'q', 'slug': 's', 'clobTokenIds': ['a', 'b']}
    book = lambda p: {'tick_size': '0.01', 'asks': [{'price': str(p), 'size': '100'}], 'bids': []}
    a = bot.pm.evaluate(m, book(.46), book(.50), 0.0, 10.0, slip_ticks=0); b = bot.pm.evaluate(m, book(.46), book(.50), 0.0, 10.0, slip_ticks=1)
    assert abs((a['net_edge'] - b['net_edge']) - 0.02) < 1e-9, (a['net_edge'], b['net_edge'])   # 1 tick on each leg = 2c per share


def test_gamma_failure_keeps_last_good_result():
    ex = FakeExchange(); fresh(ex); bot.step(); before = {m['slug'] for m in bot.markets}
    victim = f"hype-updown-15m-{ex.base() + WINDOW}"; ex.gamma_fail.add(victim); bot.pm._market_cache_at = 0; bot.step()
    after = {m['slug'] for m in bot.markets}
    assert victim in after and after == before, (before ^ after)
    assert 'simulated Gamma outage' in bot.last_error


def test_no_file_descriptor_growth_over_many_scans():
    ex = FakeExchange(); fresh(ex); bot.step(); n0 = len(os.listdir('/proc/self/fd'))
    for _ in range(60): bot.pm._market_cache_at = 0; bot.step()
    n1 = len(os.listdir('/proc/self/fd'))
    assert n1 - n0 <= 3, (n0, n1)


def test_history_and_report_per_coin():
    ex = FakeExchange(); ex.gap[('SOL', 0)] = (0.44, 0.50); ex.phantom[('ETH', 0)] = (0.40, 0.45); fresh(ex)
    for _ in range(2): bot.pm._market_cache_at = 0; bot.step()
    d = db.report_data('2000-01-01T00:00:00Z'); by = {a['asset']: a for a in d['assets']}
    assert d['total']['scans'] == 2 and by['SOL']['confirmed'] == 2 and by['ETH']['raw'] == 1 and by['ETH']['confirmed'] == 0, by
    assert by['SOL']['trades'] == 1
    only = db.report_data('2000-01-01T00:00:00Z', 'SOL'); assert only['paper']['n'] == 1 and db.report_data('2000-01-01T00:00:00Z', 'BTC')['paper']['n'] == 0
    assert sum(t['n'] for t in d['timing']) >= 1
    assert by['ETH']['best_net'] < 0.0, 'a phantom that failed the re-check must not appear as the best edge'


def test_filled_requires_a_real_match_not_the_word_success():
    f = bot.__class__._filled
    assert not f({'success': False, 'errorMsg': 'not enough balance', 'status': ''})
    assert not f({'success': False, 'status': 'matched'})
    assert not f({'success': True, 'status': 'unmatched'}) and not f('success') and not f(None)
    assert f({'success': True, 'status': 'matched', 'orderID': '1'}) and f({'status': 'MATCHED'})


class _FakeClient:
    def __init__(self, replies): self.replies = list(replies); self.orders = []
    def get_order_book(self, token): return {'asks': [{'price': '0.44', 'size': '100'}], 'bids': [{'price': '0.42', 'size': '100'}]}
    def create_and_post_order(self, args, options=None, order_type=None):
        self.orders.append(args); return self.replies.pop(0)


def _live(replies, max_loss=5.0):
    sdk = types.ModuleType('py_clob_client_v2')
    class _E:  # simple enum stand-ins
        BUY = 'BUY'; SELL = 'SELL'; FOK = 'FOK'
    sdk.Side = _E; sdk.OrderType = _E
    sdk.PartialCreateOrderOptions = lambda tick_size: tick_size
    class OrderArgs:
        def __init__(self, token_id, price, side, size): self.token_id, self.price, self.side, self.size = token_id, price, side, size
    sdk.OrderArgs = OrderArgs
    ex = FakeExchange(); ex.gap[('SOL', 0)] = (0.44, 0.50); fresh(ex); bot.max_daily_loss = max_loss
    bot.step(); client = _FakeClient(replies)
    with mock.patch.dict(sys.modules, {'py_clob_client_v2': sdk}), mock.patch.object(bot.pm, 'client', lambda: client), mock.patch.object(bot.pm, 'creds', lambda: {'x': 1}):
        bot.live = True; bot._traded = {}; bot.execute_best()
    with db.connect() as c: t = [dict(r) for r in c.execute("SELECT status,pnl,asset FROM trades WHERE mode='LIVE' ORDER BY id DESC LIMIT 1")][0]
    return t, client


OK = {'success': True, 'status': 'matched'}; BAD = {'success': False, 'errorMsg': 'no match'}


def test_live_both_legs_filled():
    t, c = _live([OK, OK]); assert t['status'] == 'BOTH_LEGS_FILLED' and t['asset'] == 'SOL' and len(c.orders) == 2


def test_live_first_leg_rejected_is_not_mistaken_for_a_fill():
    t, c = _live([BAD]); assert t['status'] == 'FIRST_LEG_NOT_FILLED' and t['pnl'] == 0 and len(c.orders) == 1 and bot.live is False


def test_live_second_leg_fails_first_leg_is_sold_back_and_loss_is_booked():
    t, c = _live([OK, BAD, OK]); assert t['status'] == 'SECOND_LEG_FAILED_UNWOUND' and t['pnl'] < 0 and c.orders[-1].side == 'SELL' and bot.live is False


def test_live_unwind_failure_books_full_cost_and_trips_the_loss_stop():
    t, c = _live([OK, BAD, BAD], max_loss=1.0)
    assert t['status'] == 'SECOND_LEG_FAILED_UNWIND_FAILED' and t['pnl'] < -4 and bot.live is False and 'MANUAL' in bot.last_error
    assert bot.live_daily_pnl() <= -1.0


def _login(client):
    html = client.get('/login').get_data(as_text=True); tok = re.search(r'name="csrf" value="([^"]+)"', html).group(1)
    return client.post('/login', data={'username': 'admin', 'password': 'correct-horse-battery', 'csrf': tok}), tok


def test_csrf_blocks_forged_posts_and_allows_real_ones():
    c = app.test_client(); r, tok = _login(c); assert r.status_code == 302
    before = bot.min_net_edge
    c.post('/settings', data={'min_net_edge': '0.5'})                                  # no token (a forged cross-site form)
    assert bot.min_net_edge == before
    c.post('/live', data={'enabled': '1', 'csrf': 'forged'}); assert bot.live is False
    c.post('/settings', data={'min_net_edge': '0.02', 'csrf': tok}); assert bot.min_net_edge == 0.02
    bot.min_net_edge = 0.01


def test_settings_page_controls_roundtrip():
    c = app.test_client(); _, tok = _login(c)
    c.post('/settings', data={'csrf': tok, 'settings_v': '2', 'min_net_edge': '0.01', 'paper_size': '10', 'live_size': '10', 'max_daily_loss': '5', 'paper_cooldown': '30',
                              'assets': ['BTC', 'ETH'], 'slots_ahead': '3', 'late_skip': '45', 'slip_ticks': '1.5', 'confirm': '1'})
    assert bot.assets == ['BTC', 'ETH'] and bot.slots_ahead == 3 and bot.late_skip == 45 and bot.slip_ticks == 1.5 and bot.confirm
    assert db.get_setting('assets') == 'BTC,ETH'
    c.post('/settings', data={'csrf': tok, 'settings_v': '2', 'min_net_edge': 'abc'})   # garbage must not 500 or corrupt anything
    assert bot.assets == ['BTC', 'ETH']
    c.post('/settings', data={'csrf': tok, 'settings_v': '2', 'min_net_edge': '0.01', 'paper_size': '10', 'live_size': '10', 'max_daily_loss': '5', 'paper_cooldown': '30', 'slots_ahead': '2', 'late_skip': '60', 'slip_ticks': '1'})
    assert bot.assets == ['BTC'] and bot.confirm is False                              # all boxes unticked -> BTC kept
    bot.assets = [a['code'] for a in ASSETS]; bot.confirm = True


def test_pages_render_and_health_hides_details_from_strangers():
    ex = FakeExchange(); ex.gap[('SOL', 0)] = (0.44, 0.50); fresh(ex); bot.step()
    c = app.test_client(); _login(c)
    assert c.get('/').status_code == 200
    for q in ('', '?range=6h', '?range=7d&asset=SOL', '?asset=NOPE'): assert c.get('/report' + q).status_code == 200, q
    csv = c.get('/export/scans.csv?range=24h&asset=SOL'); assert csv.status_code == 200 and 'SOL' in csv.get_data(as_text=True)
    assert c.get('/export/paper.csv').status_code == 200
    anon = app.test_client().get('/health').json
    assert set(anon) == {'ok', 'scanner', 'last_scan'}, anon
    assert 'open_files' in c.get('/health').json


def test_login_throttle_after_repeated_failures():
    c = app.test_client(); html = c.get('/login').get_data(as_text=True); tok = re.search(r'name="csrf" value="([^"]+)"', html).group(1)
    from app import app as A
    codes = [c.post('/login', data={'username': 'admin', 'password': 'nope', 'csrf': tok}, headers={'X-Real-IP': '9.9.9.9'}).status_code for _ in range(7)]
    assert codes[:5] == [200] * 5 and codes[5] == 429, codes
    assert c.post('/login', data={'username': 'admin', 'password': 'correct-horse-battery', 'csrf': tok}, headers={'X-Real-IP': '1.2.3.4'}).status_code == 302   # other address unaffected


def test_wallet_update_validates_funder():
    from app.polymarket import PolymarketService as P
    for bad, sig in (('0x123', 3), ('', 3)):
        try: P.save_connection(bot.pm, 'k', bad, sig)
        except ValueError as e: assert 'Funder' in str(e)
        except Exception as e: raise AssertionError(f'expected ValueError, got {e!r}')
        else: raise AssertionError('accepted a bad funder')


def test_old_database_is_migrated_in_place():
    """A database written by the previous release (scan_stats keyed by minute only) must upgrade without losing history."""
    import subprocess
    code = r'''
import sqlite3, sys, os
d = os.environ["UNDERHAVEN_DATA_DIR"]
c = sqlite3.connect(d + "/underhaven.db")
c.executescript("""
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE paper_sessions (id INTEGER PRIMARY KEY AUTOINCREMENT, started_at TEXT NOT NULL, ended_at TEXT, status TEXT NOT NULL DEFAULT 'RUNNING', scans INTEGER NOT NULL DEFAULT 0, markets_seen INTEGER NOT NULL DEFAULT 0, opportunities INTEGER NOT NULL DEFAULT 0, paper_trades INTEGER NOT NULL DEFAULT 0, wins INTEGER NOT NULL DEFAULT 0, losses INTEGER NOT NULL DEFAULT 0, simulated_spend REAL NOT NULL DEFAULT 0, simulated_pnl REAL NOT NULL DEFAULT 0, notes TEXT);
CREATE TABLE opportunities (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id INTEGER, ts TEXT, market TEXT, slug TEXT, yes_ask REAL, no_ask REAL, pair_cost REAL, gross_edge REAL, fee_estimate REAL, net_edge REAL, executable_shares REAL, executable_spend REAL, yes_liquidity REAL, no_liquidity REAL, qualified INTEGER DEFAULT 0, rejection_reason TEXT);
CREATE TABLE trades (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id INTEGER, ts TEXT, mode TEXT, market TEXT, yes_order TEXT, no_order TEXT, status TEXT, spend REAL, gross_edge REAL, fee_estimate REAL, net_edge REAL, pnl REAL, win INTEGER DEFAULT 0, note TEXT);
CREATE TABLE credentials (id INTEGER PRIMARY KEY CHECK(id=1), private_key TEXT, seed_phrase TEXT, api_key TEXT, api_secret TEXT, api_passphrase TEXT, funder TEXT, signer TEXT, signature_type INTEGER DEFAULT 0);
CREATE TABLE scan_stats (minute TEXT PRIMARY KEY, session_id INTEGER, scans INTEGER NOT NULL DEFAULT 0, markets_seen INTEGER NOT NULL DEFAULT 0, best_gross REAL, best_net REAL, min_pair REAL, qualified_scans INTEGER NOT NULL DEFAULT 0, errors INTEGER NOT NULL DEFAULT 0, last_error TEXT DEFAULT '');
""")
c.execute("INSERT INTO trades(ts,mode,market,status,pnl) VALUES('2026-10-04T01:10:00Z','PAPER','m','S',2.02)")
for m, q in (('2026-10-04T01:10Z', 1), ('2026-10-04T01:11Z', 0)): c.execute("INSERT INTO scan_stats VALUES(?,1,14,9,.04,.01,.96,?,0,'')", (m, q))
c.commit(); c.close()
sys.path.insert(0, os.environ["REPO"])
from app import db
with db.connect() as c:
    assert [r[1] for r in c.execute("PRAGMA table_info(scan_stats)")][:2] == ["minute", "asset"]
    assert c.execute("SELECT COUNT(*) FROM scan_stats WHERE asset='*'").fetchone()[0] == 2
    assert c.execute("SELECT COUNT(*) FROM scan_stats WHERE asset='BTC'").fetchone()[0] == 2
    assert c.execute("SELECT asset FROM trades").fetchone()[0] == "BTC"
db._ready = False
with db.connect() as c: pass   # a second start must be a no-op
print("MIGRATED_OK")
'''
    d = tempfile.mkdtemp()
    r = subprocess.run([sys.executable, '-c', code], env=dict(os.environ, UNDERHAVEN_DATA_DIR=d, REPO=os.path.dirname(HERE)), capture_output=True, text=True)
    assert 'MIGRATED_OK' in r.stdout, r.stdout + r.stderr


def test_profit_lock_rejects_a_gap_that_only_exists_at_the_best_price():
    ex = FakeExchange(); fresh(ex); m = {'asset': 'BTC', 'start_ts': ex.base(), 'question': 'q', 'slug': 's', 'clobTokenIds': ['a', 'b']}
    # Up has only 2 shares at 0.44, then a deep level at 0.62: the average looks fine, the worst price loses money.
    up = {'tick_size': '0.01', 'asks': [{'price': '0.44', 'size': '2'}, {'price': '0.62', 'size': '500'}], 'bids': []}
    dn = {'tick_size': '0.01', 'asks': [{'price': '0.50', 'size': '500'}], 'bids': []}
    o = bot.pm.evaluate(m, up, dn, 0.0, 10.0, now=ex.base() + 10)
    assert o['pair_cost'] < 1 and not o['qualified'] and o['reason'] in ('not profitable at worst fill price', ''), o['reason']
    solid = {'tick_size': '0.01', 'asks': [{'price': '0.44', 'size': '500'}], 'bids': []}
    assert bot.pm.evaluate(m, solid, dn, 0.01, 10.0, now=ex.base() + 10)['qualified']


def test_every_coin_that_qualifies_is_traded_in_paper_mode():
    ex = FakeExchange()
    for coin in ('BTC', 'ETH', 'SOL'): ex.gap[(coin, 0)] = (0.44, 0.50)
    fresh(ex); bot.step()
    with db.connect() as c: got = sorted(r[0] for r in c.execute("SELECT asset FROM trades WHERE mode='PAPER'"))
    assert got == ['BTC', 'ETH', 'SOL'], got


def test_live_trades_every_coin_that_qualifies_and_only_once_each():
    sdk = types.ModuleType('py_clob_client_v2')
    class _E: BUY = 'BUY'; SELL = 'SELL'; FOK = 'FOK'
    sdk.Side = _E; sdk.OrderType = _E; sdk.PartialCreateOrderOptions = lambda tick_size: tick_size
    class OrderArgs:
        def __init__(self, token_id, price, side, size): self.token_id, self.price, self.side, self.size = token_id, price, side, size
    sdk.OrderArgs = OrderArgs
    ex = FakeExchange()
    for coin in ('BTC', 'ETH', 'SOL'): ex.gap[(coin, 0)] = (0.44, 0.50)
    fresh(ex); bot.max_daily_loss = 5.0; bot.step()
    client = _FakeClient([OK] * 6)
    with mock.patch.dict(sys.modules, {'py_clob_client_v2': sdk}), mock.patch.object(bot.pm, 'client', lambda: client), mock.patch.object(bot.pm, 'creds', lambda: {'x': 1}):
        bot.live = True; bot._traded = {}; bot.execute_best(); bot.execute_best()   # second call must not repeat any window
    with db.connect() as c: got = sorted(r[0] for r in c.execute("SELECT asset FROM trades WHERE mode='LIVE' AND status='BOTH_LEGS_FILLED'"))
    assert got == ['BTC', 'ETH', 'SOL'] and len(client.orders) == 6, (got, len(client.orders))


def test_trade_log_can_be_filtered_by_coin():
    ex = FakeExchange()
    for coin in ('BTC', 'SOL'): ex.gap[(coin, 0)] = (0.44, 0.50)
    fresh(ex); bot.step(); c = app.test_client(); _login(c)
    all_html = c.get('/').get_data(as_text=True); sol = c.get('/?coin=SOL').get_data(as_text=True)
    assert '<b>BTC</b></td><td>' in all_html and '<b>SOL</b></td><td>' in all_html
    log = sol[sol.index('id="log"'):]
    assert '<b>SOL</b></td><td>' in log and '<b>BTC</b></td><td>' not in log
