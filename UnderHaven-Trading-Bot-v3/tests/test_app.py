"""Web layer: authentication, CSRF, secret handling, empty states and live-mode gating."""
import io, logging, re
from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from werkzeug.security import generate_password_hash
from app.app import app
from app import db
from app.runtime import svc

_k = rsa.generate_private_key(public_exponent=65537, key_size=2048)
PEM = _k.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()).decode()
db.set_setting('admin_user', 'admin'); db.set_setting('admin_hash', generate_password_hash('correct-horse-battery'))


def login():
    c = app.test_client(); h = c.get('/login').get_data(as_text=True); tok = re.search(r'name="csrf" value="([^"]+)"', h).group(1)
    assert c.post('/login', data={'username': 'admin', 'password': 'correct-horse-battery', 'csrf': tok}).status_code == 302
    with c.session_transaction() as s: return c, s['csrf']


def test_every_page_and_action_requires_a_login():
    c = app.test_client()
    for path in ('/', '/markets', '/strategies', '/paper', '/live', '/settings', '/api/state', '/export/trades.csv', '/export/orders.csv', '/export/signals.csv'):
        r = c.get(path); assert r.status_code in (302, 401), path
    h = c.get('/login').get_data(as_text=True); tok = re.search(r'name="csrf" value="([^"]+)"', h).group(1)
    for path in ('/live/enable', '/live/disable', '/live/kalshi', '/live/kalshi/delete', '/kill', '/kill/clear', '/settings', '/strategies/save', '/paper/balances', '/wallet/import', '/live/preflight'):
        r = c.post(path, data={'csrf': tok, 'confirm': 'ENABLE LIVE TRADING'}); assert r.status_code == 302 and '/login' in r.headers['Location'], path
    assert svc.live is False


def test_csrf_token_is_required_for_state_changes():
    c, tok = login()
    c.post('/kill', data={}); assert not svc.risk.killed()                      # forged cross-site form: ignored
    c.post('/kill', data={'csrf': tok}); assert svc.risk.killed()
    c.post('/kill/clear', data={'csrf': tok}); assert not svc.risk.killed()


def test_health_endpoints_reveal_no_secrets_to_strangers():
    anon = app.test_client().get('/health').get_json(); assert set(anon) == {'ok', 'scanner'}
    assert app.test_client().get('/ready').status_code in (200, 503)


def test_empty_states_say_so_instead_of_inventing_activity():
    with db.connect() as cn:
        for tb in ('b_signals', 'b_orders', 'b_positions', 'b_events'): cn.execute(f'DELETE FROM {tb}')
    c, _ = login()
    ov = c.get('/').get_data(as_text=True); assert 'No open positions' in ov and 'No completed trades yet' in ov
    pp = c.get('/paper').get_data(as_text=True); assert 'no curve to draw' in pp and 'No simulated orders yet' in pp and 'No trades yet' in pp
    lv = c.get('/live').get_data(as_text=True); assert 'No real order has ever been sent' in lv and 'DISABLED' in lv


def test_kalshi_credentials_are_stored_encrypted_never_echoed_and_deletable():
    c, tok = login(); logs = io.StringIO(); h = logging.StreamHandler(logs); logging.getLogger().addHandler(h)
    bad = c.post('/live/kalshi', data={'csrf': tok, 'key_id': 'abc', 'private_key': 'not a key', 'env': 'demo'}, follow_redirects=True).get_data(as_text=True)
    assert 'not saved' in bad.lower()
    page = c.post('/live/kalshi', data={'csrf': tok, 'key_id': 'KEYID-1234567890', 'private_key': PEM, 'env': 'demo'}, follow_redirects=True).get_data(as_text=True)
    assert 'saved (encrypted)' in page
    with db.connect() as cn: stored = cn.execute('SELECT private_key, key_id, env FROM kalshi_credentials').fetchone()
    assert stored['private_key'] != PEM and 'BEGIN' not in stored['private_key'] and stored['env'] == 'demo'
    body = ''.join(c.get(p).get_data(as_text=True) for p in ('/live', '/settings', '/', '/health', '/api/state'))
    assert 'BEGIN PRIVATE KEY' not in body and PEM.split('\n')[1] not in body and PEM.split('\n')[1] not in logs.getvalue()
    assert svc.kalshi_auth_client().can_trade                      # decrypts for use on the server only
    c.post('/live/kalshi/delete', data={'csrf': tok}); logging.getLogger().removeHandler(h)
    with db.connect() as cn: assert cn.execute('SELECT COUNT(*) FROM kalshi_credentials').fetchone()[0] == 0


def test_live_cannot_be_enabled_without_credentials_or_confirmation():
    c, tok = login()
    with db.connect() as cn: cn.execute('DELETE FROM kalshi_credentials'); cn.execute('DELETE FROM credentials')
    c.post('/live/enable', data={'csrf': tok, 'confirm': 'ENABLE LIVE TRADING'}); assert svc.live is False
    c.post('/live/enable', data={'csrf': tok, 'confirm': 'yes'}); assert svc.live is False
    pf = c.post('/live/preflight', data={'csrf': tok}).get_json(); assert pf['ok'] is False and any(x['name'] == 'Kalshi credentials' for x in pf['checks']) and 'kalshi' not in pf


def test_settings_are_validated_server_side_and_persist():
    c, tok = login()
    c.post('/settings', data={'csrf': tok, 'min_net_edge': '-5'}); assert svc.settings().min_net_edge >= 0
    c.post('/settings', data={'csrf': tok, 'min_net_edge': '0.03', 'max_daily_loss': '7'}); svc._S_at = -1e12
    assert svc.settings().min_net_edge == 0.03 and svc.settings().max_daily_loss == 7
    c.post('/settings', data={'csrf': tok, 'min_net_edge': '0.02', 'max_daily_loss': '5'}); svc._S_at = -1e12
    c.post('/strategies/save', data={'csrf': tok, 's_arb': 'on', 'live_strategies': ['arb']}); svc._S_at = -1e12
    assert svc.settings().live_strategies == ('arb',) and svc.settings().s_arb and not svc.settings().s_momentum
    c.post('/strategies/save', data={'csrf': tok, 's_arb': 'on', 's_momentum': 'on', 's_meanrev': 'on', 's_bundle': 'on', 'd5': 'on', 'd15': 'on'}); svc._S_at = -1e12
    assert svc.settings().live_strategies == () and svc.live is False


def test_csv_exports_work_for_a_logged_in_user():
    c, _ = login()
    for p in ('/export/trades.csv', '/export/orders.csv', '/export/signals.csv'):
        r = c.get(p); assert r.status_code == 200 and r.mimetype == 'text/csv'
