"""Exchange clients and live gateways against scripted HTTP / SDK responses (no network, no real orders)."""
import base64, json, types, sys
from decimal import Decimal as D
import requests
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import padding, rsa
from fake_btc import BASE, FakePm, FakeKalshi
from app.kalshi import KalshiAuthError, KalshiClient, KalshiError, KalshiUncertain, fill_summary, parse_orderbook
from app.btc.discovery import normalize_kalshi, normalize_polymarket
from app.btc.live import KalshiLiveGateway, PolyLiveGateway, _matched

_KEY = rsa.generate_private_key(public_exponent=65537, key_size=2048)
PEM = _KEY.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()).decode()


class Resp:
    def __init__(self, status=200, js=None, headers=None): self.status_code, self._js, self.headers, self.content = status, js or {}, headers or {}, b'x'; self.text = json.dumps(self._js)
    def json(self): return self._js


class Session:
    def __init__(self, script): self.script, self.calls = list(script), []
    def request(self, method, url, params=None, json=None, headers=None, timeout=None):
        self.calls.append((method, url, params, json, headers)); r = self.script.pop(0)
        if isinstance(r, Exception): raise r
        return r


def client(script, **kw): return KalshiClient(env='demo', key_id='kid', private_key_pem=PEM, session=Session(script), rate_per_sec=1000, sleep=lambda s: None, **kw)


def test_kalshi_requests_are_signed_with_rsa_pss_over_timestamp_method_and_path():
    c = client([Resp(200, {'balance': 2500})]); assert c.balance()['balance_usd'] == 25.0
    m, url, params, body, h = c.http.calls[0]
    msg = (h['KALSHI-ACCESS-TIMESTAMP'] + 'GET' + '/trade-api/v2/portfolio/balance').encode()
    _KEY.public_key().verify(base64.b64decode(h['KALSHI-ACCESS-SIGNATURE']), msg, padding.PSS(mgf=padding.MGF1(hashes.SHA256()), salt_length=padding.PSS.DIGEST_LENGTH), hashes.SHA256())
    assert h['KALSHI-ACCESS-KEY'] == 'kid' and 'demo-api.kalshi.co' in url


def test_public_market_data_needs_no_credentials_and_sends_no_auth_headers():
    c = KalshiClient(session=Session([Resp(200, {'orderbook_fp': {'yes_dollars': [['0.40', '10']], 'no_dollars': [['0.55', '5']]}})]), sleep=lambda s: None)
    ob = c.get_orderbook('KX-1'); assert ob['yes_asks'][0] == (0.45, 5.0) and 'KALSHI-ACCESS-KEY' not in c.http.calls[0][4]
    try: c.balance(); raise AssertionError('private call without credentials')
    except KalshiAuthError: pass


def test_rate_limit_backoff_server_errors_and_outages_do_not_crash_reads():
    sleeps = []
    c = KalshiClient(session=Session([Resp(429, headers={'Retry-After': '2'}), Resp(503), requests.ConnectionError('x'), Resp(200, {'markets': []})]), sleep=sleeps.append, rate_per_sec=1000)
    assert c.get_markets_by_tickers(['A']) == [] and len(sleeps) >= 3 and sleeps[0] == 2.0
    dead = KalshiClient(session=Session([requests.ConnectionError('x')] * 5), sleep=lambda s: None, rate_per_sec=1000)
    try: dead.get_market('A'); raise AssertionError('should raise')
    except KalshiError: pass


def test_authentication_failure_is_reported_clearly():
    c = client([Resp(401, {'error': 'bad signature'})])
    try: c.balance(); raise AssertionError('accepted')
    except KalshiAuthError as e: assert 'rejected the credentials' in str(e)


def test_order_timeout_is_uncertain_and_is_never_retried_blindly():
    c = client([requests.Timeout('slow')])
    try: c.create_order('KX-1', 'yes', 'buy', 5, 0.55, 'fill_or_kill', 'uh-abc'); raise AssertionError('should be uncertain')
    except KalshiUncertain: pass
    assert len(c.http.calls) == 1                                           # exactly one POST: no automatic resend


def test_kalshi_order_payload_uses_whole_contracts_cents_and_client_order_id():
    c = client([Resp(200, {'order': {'order_id': 'o1', 'status': 'executed', 'fill_count': 5}})])
    c.create_order('KX-1', 'no', 'buy', 5, 0.42, 'immediate_or_cancel', 'uh-1')
    body = c.http.calls[0][3]; assert body['no_price'] == 42 and 'yes_price' not in body and body['count'] == 5 and body['client_order_id'] == 'uh-1' and body['time_in_force'] == 'immediate_or_cancel'
    for bad in (dict(count=0), dict(price=1.5), dict(price=0.0)):
        try: client([]).create_order('KX-1', 'yes', 'buy', bad.get('count', 1), bad.get('price', 0.5)); raise AssertionError(bad)
        except KalshiError: pass


def _kmarket(): k = FakeKalshi(); return normalize_kalshi(k.markets[k.add(900, BASE)])


def test_kalshi_gateway_reconciles_full_partial_unfilled_and_uncertain_orders():
    mk = _kmarket()
    def gw(script): return KalshiLiveGateway(client(script), lambda m: None)
    full = gw([Resp(200, {'order': {'order_id': 'o1', 'status': 'executed', 'fill_count': 5}}), Resp(200, {'fills': [{'count': 5, 'no_price': 42, 'side': 'no', 'fee_cost': '0.09'}]})]).place(mk, 'DOWN', 'buy', D(5), D('0.42'), 'fok', 'r1')
    assert full.status == 'FILLED' and full.qty == 5 and full.avg_price == D('0.42') and full.fees == D('0.09') and not full.estimated
    part = gw([Resp(200, {'order': {'order_id': 'o2', 'status': 'canceled', 'fill_count': 2}}), Resp(200, {'fills': [{'count': 2, 'yes_price': 55, 'side': 'yes'}]})]).place(mk, 'UP', 'buy', D(5), D('0.55'), 'ioc', 'r2')
    assert part.status == 'PARTIAL' and part.qty == 2
    none = gw([Resp(200, {'order': {'order_id': 'o3', 'status': 'canceled', 'fill_count': 0}}), Resp(200, {'fills': []})]).place(mk, 'UP', 'buy', D(5), D('0.55'), 'fok', 'r3'); assert none.status == 'UNFILLED'
    unc = gw([requests.Timeout('t'), Resp(200, {'orders': []})]).place(mk, 'UP', 'buy', D(5), D('0.55'), 'fok', 'r4'); assert unc.status == 'UNCERTAIN'
    found = gw([requests.Timeout('t'), Resp(200, {'orders': [{'client_order_id': 'uh-r5', 'order_id': 'o5', 'status': 'executed', 'fill_count': 5}]}), Resp(200, {'fills': []})]).place(mk, 'UP', 'buy', D(5), D('0.55'), 'fok', 'r5')
    assert found.status == 'FILLED'                                         # the timed-out order WAS processed: found by client id, not resent
    assert gw([]).place(mk, 'UP', 'buy', D('2.5'), D('0.5'), 'fok', 'r6').status == 'REJECTED'


def test_polymarket_success_flag_alone_is_never_a_fill():
    assert not _matched({'success': False, 'errorMsg': 'x', 'status': 'matched'}) and not _matched({'success': True, 'status': 'unmatched'}) and not _matched('success') and not _matched(None)
    assert _matched({'success': True, 'status': 'matched'})


class _Client:
    def __init__(self, reply): self.reply, self.orders = reply, []
    def create_and_post_order(self, args, options=None, order_type=None): self.orders.append((args, options, order_type)); return self.reply


def _sdk():
    m = types.ModuleType('py_clob_client_v2')
    class E: BUY, SELL, FOK, FAK = 'BUY', 'SELL', 'FOK', 'FAK'
    m.Side = E; m.OrderType = E; m.PartialCreateOrderOptions = lambda tick_size: tick_size
    class A:
        def __init__(self, token_id, price, side, size): self.token_id, self.price, self.side, self.size = token_id, price, side, size
    m.OrderArgs = A; return m


def test_polymarket_gateway_signs_via_the_sdk_with_real_token_ids_and_reads_actual_amounts():
    pm = FakePm(); pm.add_window(900, BASE); P = normalize_polymarket(pm.events[f'btc-updown-15m-{BASE}'][0]['markets'][0], 900, BASE)
    cl = _Client({'success': True, 'status': 'matched', 'orderID': 'abc', 'makingAmount': '6.00', 'takingAmount': '10'}); pm.client = lambda: cl
    sys.modules['py_clob_client_v2'] = _sdk()
    r = PolyLiveGateway(pm, lambda m: None).place(P, 'UP', 'buy', D(10), D('0.62'), 'fok', 'r1')
    a, opts, ot = cl.orders[0]; assert a.token_id == P.up_id and a.token_id and a.price == 0.62 and a.size == 10.0 and ot == 'FOK'
    assert r.status == 'FILLED' and r.avg_price == D('0.6') and r.qty == 10 and r.ref == 'abc'
    pm.client = lambda: _Client({'success': False, 'errorMsg': 'not enough balance'}); assert PolyLiveGateway(pm, None).place(P, 'UP', 'buy', D(10), D('0.62'), 'fok', 'r2').status == 'UNFILLED'
    pm.client = lambda: None; assert PolyLiveGateway(pm, None).place(P, 'UP', 'buy', D(10), D('0.62'), 'fok', 'r3').status == 'REJECTED'
    sys.modules.pop('py_clob_client_v2', None)
