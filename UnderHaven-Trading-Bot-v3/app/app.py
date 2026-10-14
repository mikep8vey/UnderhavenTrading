from flask import Flask, render_template, request, redirect, url_for, session, jsonify, flash, Response
from werkzeug.security import generate_password_hash, check_password_hash
from . import db
from .config import HOST, PORT
from .runtime import pm, svc, feed
from .btc import settings as bs, store, views
from .btc.calc import d
from .kalshi import KalshiClient, KalshiError
import os, secrets, csv, io, time, logging, traceback

app = Flask(__name__, static_folder='../static', template_folder='../templates')
app.secret_key = os.environ.get('UNDERHAVEN_SESSION_SECRET') or secrets.token_hex(32)
app.config.update(SESSION_COOKIE_HTTPONLY=True, SESSION_COOKIE_SAMESITE='Lax', MAX_CONTENT_LENGTH=64 * 1024)
logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(name)s %(message)s')
log = logging.getLogger('underhaven')

@app.before_request
def _csrf_guard():
    """Every POST must carry the per-session token, so another website cannot make your browser press
    'Enable LIVE', change settings or swap the wallet while you are logged in."""
    if 'csrf' not in session: session['csrf'] = secrets.token_urlsafe(24)
    if request.method == 'POST':
        sent = request.form.get('csrf', '') or request.headers.get('X-CSRF-Token', '')
        if not sent or not secrets.compare_digest(sent, session.get('csrf', '')):
            flash('That form was out of date (or your session expired). Please try again.')
            return redirect('/')

@app.context_processor
def _inject_csrf(): return {'csrf': session.get('csrf', '')}

@app.after_request
def _headers(resp):
    resp.headers.setdefault('X-Frame-Options', 'DENY')
    resp.headers.setdefault('X-Content-Type-Options', 'nosniff')
    resp.headers.setdefault('Referrer-Policy', 'no-referrer')
    if resp.mimetype == 'text/html': resp.headers['Cache-Control'] = 'no-store'
    return resp

_FAILS = {}
def _client_ip():
    return request.headers.get('X-Real-IP') or request.remote_addr or '?'   # set by nginx, not by the client

def _login_wait(ip):
    """Seconds this address must still wait. 5 failures in 10 min, then a growing delay (max 5 min): slows
    password guessing without letting a stranger lock you out."""
    now = time.time(); recent = [t for t in _FAILS.get(ip, []) if now - t < 600]; _FAILS[ip] = recent
    if len(recent) < 5: return 0
    return max(0, int(min(300, 15 * (len(recent) - 4)) - (now - recent[-1])))


@app.template_filter('short')
def short_addr(a):
    a = str(a or ''); return a if len(a) < 14 else a[:6] + '…' + a[-4:]

@app.template_filter('usd')
def usd(x, places=2):
    try: return ('-' if float(x) < 0 else '') + '$' + f'{abs(float(x)):,.{places}f}'
    except (TypeError, ValueError): return '–'

@app.template_filter('num')
def num(x, places=2):
    try: return f'{float(x):,.{places}f}'
    except (TypeError, ValueError): return '–'

@app.template_filter('pct')
def pct(x, places=2):
    try: return f'{float(x):+.{places}f}%'
    except (TypeError, ValueError): return '–'

@app.template_filter('utc')
def utc(ts):
    try: return time.strftime('%Y-%m-%d %H:%M:%S', time.gmtime(float(ts))) + 'Z'
    except (TypeError, ValueError): return '–'

@app.template_filter('hms')
def hms(ts):
    try: return time.strftime('%H:%M:%S', time.gmtime(float(ts))) + 'Z'
    except (TypeError, ValueError): return '–'

@app.template_filter('mmss')
def mmss(s):
    try: s = max(0, int(float(s))); return f'{s // 60}:{s % 60:02d}'
    except (TypeError, ValueError): return '–'

@app.template_filter('dur')
def dur(s):
    try: s = int(float(s))
    except (TypeError, ValueError): return '–'
    return f'{s // 86400}d {s % 86400 // 3600}h' if s >= 86400 else (f'{s // 3600}h {s % 3600 // 60}m' if s >= 3600 else f'{s // 60}m {s % 60}s')

def auth(): return bool(session.get('admin'))
def setup_done(): return bool(db.get_setting('admin_hash'))

@app.route('/login', methods=['GET','POST'])
def login():
    if request.method == 'POST':
        wait = _login_wait(_client_ip())
        if wait:
            flash(f'Too many failed attempts. Try again in {wait} seconds.')
            return render_template('login.html', setup=not setup_done()), 429
        if request.form.get('username') == db.get_setting('admin_user','') and check_password_hash(db.get_setting('admin_hash',''), request.form.get('password','')):
            _FAILS.pop(_client_ip(), None)
            csrf = session.get('csrf'); session.clear(); session['csrf'] = csrf or secrets.token_urlsafe(24)
            session['admin'] = True
            db.set_setting('prev_login', db.get_setting('last_login', ''))
            db.set_setting('last_login', time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()))
            return redirect('/')
        _FAILS.setdefault(_client_ip(), []).append(time.time())
        flash('Invalid administrator credentials.')
    return render_template('login.html', setup=not setup_done())

@app.route('/setup', methods=['GET','POST'])
def setup():
    if setup_done(): return redirect(url_for('login'))
    if request.method == 'POST':
        u=request.form.get('username','').strip(); p=request.form.get('password',''); p2=request.form.get('password_confirm','')
        if len(u)<3: flash('Use an administrator username of at least 3 characters.')
        elif len(p)<10: flash('Password must be at least 10 characters.')
        elif p != p2: flash('The two passwords do not match.')
        else:
            db.set_setting('admin_user',u); db.set_setting('admin_hash',generate_password_hash(p)); return redirect(url_for('login'))
    return render_template('setup.html')

@app.route('/logout')
def logout(): session.clear(); return redirect(url_for('login'))


def need(): return redirect(url_for('login'))
def back(): return redirect(request.referrer or '/')

def page(name, **ctx):
    """Full page, or just the body (?fragment=1) so the page can refresh itself without reloading, which would wipe a form being typed in."""
    ctx.update(nav=name, mode='LIVE' if svc.live else 'PAPER', killed=svc.risk.killed(), kill_reason=svc.risk.kill_reason(), svc=svc)
    if request.args.get('fragment'): return render_template(f'_{name}_body.html', **ctx)
    return render_template(f'{name}.html', **ctx)

@app.route('/')
def overview():
    if not auth(): return need()
    return page('overview', v=views.overview(svc))

@app.route('/markets')
def markets():
    if not auth(): return need()
    return page('markets', v=views.markets_view(svc))

@app.route('/strategies')
def strategies():
    if not auth(): return need()
    return page('strategies', v=views.strategies_view(svc), S=svc.settings())

@app.post('/strategies/save')
def strategies_save():
    if not auth(): return need()
    try:
        raw = {k: (k in request.form) for k in ('s_arb', 's_momentum', 's_meanrev', 's_bundle', 'd5', 'd15', 'research_xplat')}
        raw['live_strategies'] = request.form.getlist('live_strategies')
        clean = bs.validate(raw); bs.save(db.set_setting, clean); svc._S_at = 0
        flash('Strategy switches saved. Live permissions only matter once live mode has been enabled on the Live page.')
    except ValueError as e: flash('Not saved: ' + str(e))
    return back()

@app.route('/paper')
def paper():
    if not auth(): return need()
    return page('paper', v=views.paper_view(svc))

@app.post('/paper/balances')
def paper_balances():
    if not auth(): return need()
    try:
        with db.connect() as c: used = c.execute("SELECT COUNT(*) FROM b_orders WHERE mode='PAPER'").fetchone()[0]
        if used: raise ValueError('Paper balances cannot change after paper trading has started: that would rewrite recorded history.')
        clean = bs.validate({k: request.form.get(k) for k in ('balance_polymarket', 'balance_kalshi')})
        bs.save(db.set_setting, clean)
        for v in ('polymarket', 'kalshi'): store.account_set_start('PAPER', v, clean['balance_' + v])
        svc._S_at = 0; flash('Paper starting balances updated.')
    except ValueError as e: flash('Not saved: ' + str(e))
    return back()

@app.route('/settings')
def settings_page():
    if not auth(): return need()
    S = svc.settings(); return page('settings', S=S, SV=S.to_dict(), R=bs.RANGES, C=bs.CHOICES)

@app.post('/settings')
def settings_save():
    if not auth(): return need()
    try:
        raw = {k: v for k, v in request.form.items() if k in bs.RANGES or k in bs.CHOICES}
        if not raw: raise ValueError('nothing to save')
        clean = bs.validate(raw); bs.save(db.set_setting, clean); svc._S_at = 0
        store.event('INFO', 'SETTINGS_CHANGED', ', '.join(sorted(clean)))
        flash('Settings saved. Limits are enforced by the backend on the next scan.')
    except ValueError as e: flash('Not saved: ' + str(e))
    return back()

@app.post('/kill')
def kill():
    if not auth(): return need()
    svc.risk.kill('Operator emergency stop'); svc.disable_live('emergency stop')
    flash('EMERGENCY STOP: no new entries. Live mode is off. Existing positions follow the kill policy "' + svc.settings().kill_policy + '".')
    return back()

@app.post('/kill/clear')
def kill_clear():
    if not auth(): return need()
    svc.risk.clear_kill(); flash('Kill switch cleared. Live mode stays OFF until you enable it again.'); return back()

@app.route('/live')
def live_page():
    if not auth(): return need()
    with db.connect() as c: kr = c.execute('SELECT key_id, env, updated_ts FROM kalshi_credentials WHERE id=1').fetchone()
    live_positions = views.position_rows(svc, 'LIVE', ('OPEN', 'AWAITING_SETTLEMENT', 'SETTLED', 'CLOSED'))[:50]
    with db.connect() as c: live_orders = [dict(r) for r in c.execute("SELECT * FROM b_orders WHERE mode='LIVE' ORDER BY id DESC LIMIT 50")]
    return page('live', S=svc.settings(), kalshi=dict(kr) if kr else None, pcreds=pm.creds(), live_positions=live_positions, live_orders=live_orders,
                perf=store.performance('LIVE'), pf=None, alerts=[e for e in views_events() if e['level'] in ('CRITICAL', 'WARNING')][:10])

def views_events():
    with db.connect() as c: return [dict(r) for r in c.execute('SELECT * FROM b_events ORDER BY id DESC LIMIT 60')]

@app.post('/live/preflight')
def live_preflight():
    if not auth(): return need()
    pf = svc.preflight(); pf.pop('kalshi', None)
    return jsonify(pf)

@app.post('/live/enable')
def live_enable():
    if not auth(): return need()
    ok, msg = svc.enable_live(request.form.get('confirm', '').strip()); flash(msg); return back()

@app.post('/live/disable')
def live_disable():
    if not auth(): return need()
    svc.disable_live('operator'); flash('Live trading disabled. Open orders/positions were not touched.'); return back()

@app.post('/live/kalshi')
def kalshi_save():
    if not auth(): return need()
    try:
        key_id, pem, env = request.form.get('key_id', '').strip(), request.form.get('private_key', '').strip(), request.form.get('env', 'prod')
        if env not in ('prod', 'demo'): raise ValueError('environment must be prod or demo')
        if not key_id or not pem: raise ValueError('key id and private key are both required')
        KalshiClient(env=env, key_id=key_id, private_key_pem=pem)        # validates the PEM before anything is stored
        with db.connect() as c:
            c.execute('INSERT INTO kalshi_credentials(id,key_id,private_key,env,updated_ts) VALUES(1,?,?,?,?) ON CONFLICT(id) DO UPDATE SET key_id=excluded.key_id, '
                      'private_key=excluded.private_key, env=excluded.env, updated_ts=excluded.updated_ts', (key_id, pm.vault.encrypt(pem), env, time.time()))
        svc.disable_live('credentials changed'); flash('Kalshi credentials saved (encrypted). Run the connection test before enabling live trading.')
    except Exception as e: flash('Kalshi credentials not saved: ' + str(e)[:160])
    return back()

@app.post('/live/kalshi/test')
def kalshi_test():
    if not auth(): return need()
    try:
        kc = svc.kalshi_auth_client()
        if not kc: raise KalshiError('No Kalshi credentials are stored.')
        b = kc.balance()['balance_usd']; flash(f'Kalshi authentication OK on {kc.env}.' + (f' Available balance ${b:,.2f}.' if b is not None else ''))
    except Exception as e: flash('Kalshi connection test failed: ' + str(e)[:160])
    return back()

@app.post('/live/kalshi/delete')
def kalshi_delete():
    if not auth(): return need()
    with db.connect() as c: c.execute('DELETE FROM kalshi_credentials WHERE id=1')
    svc.disable_live('credentials removed'); flash('Kalshi credentials deleted from this server.'); return back()

@app.post('/live/polymarket/test')
def poly_test():
    if not auth(): return need()
    try:
        if not pm.creds(): raise ValueError('No Polymarket wallet is connected.')
        pm._bal_sync_at = 0.0; b = pm.balance()
        raw = b.get('balance') if isinstance(b, dict) else getattr(b, 'balance', None)
        flash(f'Polymarket authentication OK. Available collateral ${float(raw) / 1e6:,.2f}.' if raw is not None else 'Polymarket answered, but no balance was returned.')
    except Exception as e: flash('Polymarket connection test failed: ' + str(e)[:160])
    return back()

@app.post('/wallet/generate')
def wallet_generate():
    if not auth(): return need()
    try:
        w = pm.generate_wallet(); pm.save_connection(w['private_key'], funder=w['address'], signature_type=0, seed_phrase=w['seed_phrase'])
        flash('Testing wallet generated and encrypted on the VPS. Write the seed phrase down offline before funding.')
    except Exception as e: flash('Wallet generation failed: ' + str(e))
    return back()

@app.post('/wallet/import')
def wallet_import():
    if not auth(): return need()
    try:
        pm.save_connection(request.form.get('private_key', '').strip(), request.form.get('funder', '').strip() or None, int(request.form.get('signature_type', '0')))
        svc.disable_live('wallet changed'); flash('Polymarket signer authenticated and API credentials derived. Live trading stays OFF.')
    except Exception as e: flash('Polymarket connection failed: ' + str(e))
    return back()

@app.post('/wallet/update')
def wallet_update():
    if not auth(): return need()
    try:
        pm.update_connection(request.form.get('funder', '').strip() or None, int(request.form.get('signature_type', '0'))); pm._bal_sync_at = 0.0
        svc.disable_live('wallet changed'); flash('Funder / signature type updated. Live trading was switched off.')
    except Exception as e: flash('Update failed: ' + str(e))
    return back()

@app.post('/wallet/delete')
def wallet_delete():
    if not auth(): return need()
    with db.connect() as c: c.execute('DELETE FROM credentials')
    svc.disable_live('credentials removed'); pm._client = None; flash('Polymarket credentials deleted from this server.'); return back()

def _csv(rows, cols, name):
    out = io.StringIO(); w = csv.writer(out); w.writerow(cols)
    for r in rows: w.writerow([r.get(c) for c in cols])
    return Response(out.getvalue(), mimetype='text/csv', headers={'Content-Disposition': f'attachment; filename={name}'})

@app.route('/export/trades.csv')
def export_trades():
    if not auth(): return need()
    mode = 'LIVE' if request.args.get('mode') == 'LIVE' else 'PAPER'
    rows = [dict(t, venues='+'.join(t['venues']), opened=utc(t['opened']), closed=utc(t['closed']) if t['closed'] else '') for t in store.trades(mode, 100000)]
    return _csv(rows, ['group_id', 'strategy', 'duration', 'venues', 'done', 'cost', 'fees', 'pnl', 'opened', 'closed'], f'underhaven-{mode.lower()}-trades.csv')

@app.route('/export/orders.csv')
def export_orders():
    if not auth(): return need()
    mode = 'LIVE' if request.args.get('mode') == 'LIVE' else 'PAPER'
    with db.connect() as c: rows = [dict(r) for r in c.execute('SELECT * FROM b_orders WHERE mode=? ORDER BY id', (mode,))]
    return _csv(rows, ['id', 'ts', 'mode', 'group_id', 'strategy', 'venue', 'market_id', 'duration', 'side', 'action', 'tif', 'qty_req', 'limit_price', 'status', 'qty_filled', 'avg_price', 'fees', 'slippage', 'reason', 'estimated'], f'underhaven-{mode.lower()}-orders.csv')

@app.route('/export/signals.csv')
def export_signals():
    if not auth(): return need()
    with db.connect() as c: rows = [dict(r) for r in c.execute('SELECT * FROM b_signals ORDER BY id DESC LIMIT 50000')]
    return _csv(rows, ['id', 'ts', 'strategy', 'duration', 'venue', 'market_id', 'side', 'accepted', 'reason', 'edge', 'p_model', 'price', 'qty', 'mode'], 'underhaven-signals.csv')

@app.get('/api/state')
def api_state():
    if not auth(): return jsonify(error='login required'), 401
    o = views.overview(svc)
    return jsonify(mode=o['mode'], killed=o['killed'], uptime=o['uptime'], last_ok_age=o['last_ok_age'], live_markets=o['live_markets'], verified=o['verified'], detected=o['detected'],
                   rejected=o['rejected'], paper_net=o['paper']['perf']['net_pnl'], errors=o['errors'])

@app.route('/health')
def health():
    out = dict(ok=True, scanner=bool(svc._thread and svc._thread.is_alive()))
    if auth():
        a = svc.now() - svc.last_ok if svc.last_ok else None
        out.update(mode='LIVE' if svc.live else 'PAPER', last_ok_age=a, cycles=svc.cycles, killed=svc.risk.killed(), errors={k: v for k, v in svc.errors.items() if v},
                   open_files=len(os.listdir('/proc/self/fd')) if os.path.isdir('/proc/self/fd') else None)
    return jsonify(out)

@app.route('/ready')
def ready():
    ok = bool(svc.last_ok and svc.now() - svc.last_ok < 60)
    return jsonify(ready=ok), (200 if ok else 503)

@app.errorhandler(500)
def err500(e):
    rid = secrets.token_hex(4); log.error('Unhandled error [%s]: %s\n%s', rid, e, traceback.format_exc())
    return render_template('error.html', request_id=rid, error='An internal error occurred. See the service log for the diagnostic id.'), 500

if __name__ == '__main__':
    app.run(host=HOST, port=PORT)
