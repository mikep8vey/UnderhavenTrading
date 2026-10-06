from flask import Flask, render_template, request, redirect, url_for, session, jsonify, flash, Response
from werkzeug.security import generate_password_hash, check_password_hash
from . import db
from .bot import bot
from .config import HOST, PORT
from .assets import ASSETS
import os, re, secrets, csv, io, time, calendar, logging, traceback

app = Flask(__name__, static_folder='../static', template_folder='../templates')
app.secret_key = os.environ.get('UNDERHAVEN_SESSION_SECRET') or secrets.token_hex(32)
app.config.update(SESSION_COOKIE_HTTPONLY=True, SESSION_COOKIE_SAMESITE='Lax')
KNOWN_COINS = [a['code'] for a in ASSETS]
logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(name)s: %(message)s')
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

SIG_LABELS = {0: '0 · standalone wallet', 1: '1 · proxy wallet (older email/Magic)', 2: '2 · Gnosis Safe', 3: '3 · deposit wallet (new accounts)'}

@app.template_filter('short')
def short_addr(a):
    a = str(a or '')
    return a if len(a) < 14 else a[:6] + '…' + a[-4:]

def balance_view(bal):
    """Turn the raw get_balance_allowance() payload into something readable. The CLOB reports
    collateral in 6-decimal base units, so 1000000 = $1.00. The raw value stays visible too."""
    if bal is None: return None
    get = (lambda k, d=None: bal.get(k, d)) if isinstance(bal, dict) else (lambda k, d=None: getattr(bal, k, d))
    raw = get('balance', None); allow = get('allowances', {}) or {}
    usd = None
    try:
        r = str(raw).strip()
        usd = int(r) / 1_000_000 if r.lstrip('-').isdigit() else float(r)
    except Exception: pass
    items = [(str(a), str(v)) for a, v in (allow.items() if isinstance(allow, dict) else [])]
    approved = sum(1 for _, v in items if v.strip() not in ('', '0', '0.0'))
    return {'usd': usd, 'raw': str(raw) if raw is not None else str(bal)[:200], 'allowances': items, 'approved': approved}

def slot_ts(x):
    if x.get('start_ts'): return int(x['start_ts'])
    try: return int(str(x.get('slug', '')).rsplit('-', 1)[-1])
    except Exception: return 0

_WIN = re.compile(r'(\d{1,2}:\d{2})\s*([AP]M)?\s*-\s*(\d{1,2}:\d{2})\s*([AP]M)', re.I)

@app.template_global('window_label')
def window_label(x):
    """'Bitcoin Up or Down - October 3, 8:00PM-8:15PM ET' -> '8:00-8:15 PM'."""
    m = _WIN.search(str(x.get('question', '')))
    if not m:
        st = slot_ts(x)   # fall back to the window's start time (UTC) when the title has no clock range
        return (time.strftime('%H:%M', time.gmtime(st)) + '-' + time.strftime('%H:%M', time.gmtime(st + 900)) + ' UTC') if st else str(x.get('question', ''))[-22:]
    a, ap1, b, ap2 = m.groups(); ap2 = ap2.upper()
    return f'{a}-{b} {ap2}' if (ap1 or ap2).upper() == ap2 else f'{a} {ap1.upper()}-{b} {ap2}'

@app.template_global('slot_state')
def slot_state(x, now=None):
    """('LIVE', '4:12 left') for the running window, ('NEXT', 'in 8m') for later ones."""
    start = slot_ts(x); now = now or time.time()
    if not start: return ('', '')
    if start <= now < start + 900:
        left = int(start + 900 - now); return ('LIVE', f'{left // 60}:{left % 60:02d}')
    if now < start: return ('NEXT', f'in {int((start - now) // 60)}m')
    return ('', '')

def group_markets(markets, per_asset=5, enabled=None):
    """One block per coin, soonest window first. Coins with no data yet still get a card."""
    out = []
    for a in ASSETS:
        rows = sorted([m for m in markets if m.get('asset', 'BTC') == a['code']], key=slot_ts)
        best = max([m['net_edge'] for m in rows], default=None)
        out.append(dict(a, rows=rows[:per_asset], hidden=max(0, len(rows) - per_asset), on=(enabled is None or a['code'] in enabled),
                        qualified=sum(1 for m in rows if m.get('qualified')), best_net=best))
    return out

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
            return redirect(url_for('dashboard'))
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

@app.route('/')
def dashboard():
    if not auth(): return redirect(url_for('login'))
    try:
        creds = bot.pm.creds()
        bal = None
        if creds:
            try:
                bal = bot.pm.balance()
            except Exception as e:
                bot.last_error = 'BALANCE: ' + str(e)
                log.exception('Polymarket balance lookup failed')
        paper = db.latest_paper_session()
        coin = request.args.get('coin') if request.args.get('coin') in KNOWN_COINS else None
        trades = db.recent_trades(100, coin)
        opps = db.recent_opportunities(100)
        return render_template('dashboard.html', bot=bot, creds=creds, balance=bal, bal_view=balance_view(bal), sig_labels=SIG_LABELS, assets=group_markets(list(bot.markets), per_asset=bot.slots_ahead + 2, enabled=bot.assets), preview=False, notes=bot.pm.notes, coin=coin,
                               opps=opps, trades=trades, paper=paper)
    except Exception as e:
        # Never hide the actual dashboard failure behind a generic 500.
        # The full traceback remains in journalctl for the VPS administrator.
        request_id = secrets.token_hex(4)
        log.error('Dashboard failure [%s]: %s\n%s', request_id, e, traceback.format_exc())
        return render_template('error.html', request_id=request_id, error=str(e)), 500

@app.post('/scanner/start')
def start():
    if not auth(): return redirect(url_for('login'))
    bot.start(); return redirect('/')
@app.post('/scanner/stop')
def stop():
    if not auth(): return redirect(url_for('login'))
    bot.stop(); return redirect('/')
@app.post('/live')
def live():
    if not auth(): return redirect(url_for('login'))
    enabled=request.form.get('enabled')=='1'
    if enabled and not bot.pm.creds(): flash('Connect a Polymarket signer first. Live trading was not enabled.')
    else: bot.set_live(enabled)
    return redirect('/')

@app.post('/settings')
def settings():
    if not auth(): return redirect(url_for('login'))
    try:
        num = lambda k, cur: float(request.form.get(k, cur))
        min_net = max(0.0, num('min_net_edge', bot.min_net_edge)); paper = max(0.1, num('paper_size', bot.paper_trade_size))
        live = max(0.1, num('live_size', bot.live_size)); loss = max(0.0, num('max_daily_loss', bot.max_daily_loss))
        cool = max(1, int(num('paper_cooldown', bot.cooldown_seconds)))
        bot.min_net_edge, bot.paper_trade_size, bot.live_size, bot.max_daily_loss, bot.cooldown_seconds = min_net, paper, live, loss, cool
        bot.auto_merge = request.form.get('auto_merge') == '1'
        pairs = [('min_net_edge', min_net), ('paper_size', paper), ('live_size', live), ('max_daily_loss', loss),
                 ('paper_cooldown', cool), ('auto_merge', '1' if bot.auto_merge else '0')]
        if request.form.get('settings_v') == '2':   # v3.5 scanner controls (absent on a stale cached page)
            codes = [c for c in request.form.getlist('assets') if c in KNOWN_COINS]
            if not codes: codes = ['BTC']; flash('At least one coin must be scanned; BTC was kept.')
            bot.assets = codes
            bot.slots_ahead = min(4, max(0, int(num('slots_ahead', bot.slots_ahead))))
            bot.late_skip = min(600, max(0, int(num('late_skip', bot.late_skip))))
            bot.confirm = request.form.get('confirm') == '1'
            bot.slip_ticks = min(5.0, max(0.0, num('slip_ticks', bot.slip_ticks)))
            pairs += [('assets', ','.join(codes)), ('slots_ahead', bot.slots_ahead), ('late_skip', bot.late_skip),
                      ('confirm', '1' if bot.confirm else '0'), ('slip_ticks', bot.slip_ticks)]
        for k, v in pairs: db.set_setting(k, v)
        flash('Settings saved.')
    except (ValueError, TypeError) as e:
        flash('Settings were not saved: ' + str(e))
    return redirect('/')

@app.post('/wallet/generate')
def wallet_generate():
    if not auth(): return redirect(url_for('login'))
    try:
        w=bot.pm.generate_wallet(); bot.pm.save_connection(w['private_key'], funder=w['address'], signature_type=0, seed_phrase=w['seed_phrase'])
        flash('Testing wallet generated and encrypted on the VPS. Write the seed phrase down offline before funding. Never reuse a key that has been exposed publicly.')
    except Exception as e: flash('Wallet generation failed: '+str(e))
    return redirect('/')

@app.post('/wallet/import')
def wallet_import():
    if not auth(): return redirect(url_for('login'))
    try:
        pk=request.form.get('private_key','').strip(); funder=request.form.get('funder','').strip() or None; sig=int(request.form.get('signature_type','0'))
        r=bot.pm.save_connection(pk,funder,sig); bot.set_live(False)
        flash('Polymarket signer authenticated. CLOB API credentials were created/derived automatically. LIVE stays off until you switch it on.')
    except Exception as e: flash('Polymarket connection failed: '+str(e))
    return redirect('/')

@app.post('/wallet/update')
def wallet_update():
    if not auth(): return redirect(url_for('login'))
    try:
        bot.pm.update_connection(request.form.get('funder','').strip() or None, int(request.form.get('signature_type','0')))
        bot.pm._bal_sync_at = 0.0; bot.set_live(False)
        flash('Funder / signature type updated using the stored key. LIVE trading was switched off; re-enable it deliberately after checking the balance.')
    except Exception as e: flash('Update failed: '+str(e))
    return redirect('/')

@app.post('/wallet/lock')
def lock():
    if not auth(): return redirect(url_for('login'))
    bot.set_live(False); flash('Live execution disabled.')
    return redirect('/')

@app.route('/export/paper.csv')
def export_paper():
    if not auth(): return redirect(url_for('login'))
    sid=request.args.get('session_id', type=int)
    if not sid and bot.session_id: sid=bot.session_id
    out=io.StringIO(); w=csv.writer(out)
    w.writerow(['id','timestamp','market','slug','yes_ask','no_ask','pair_cost','gross_edge','fee_estimate','net_edge','shares','spend','yes_liquidity','no_liquidity','qualified','coin','reason'])
    with db.connect() as c:
        for r in c.execute('SELECT * FROM opportunities WHERE session_id=? ORDER BY id',(sid,)):
            w.writerow([r[k] for k in ['id','ts','market','slug','yes_ask','no_ask','pair_cost','gross_edge','fee_estimate','net_edge','executable_shares','executable_spend','yes_liquidity','no_liquidity','qualified','asset','rejection_reason']])
    return Response(out.getvalue(), mimetype='text/csv', headers={'Content-Disposition':f'attachment; filename=underhaven-paper-{sid or "latest"}.csv'})

RANGES = {'6h': 6, '12h': 12, '24h': 24, '48h': 48, '7d': 168}

def _window():
    """Resolve ?range= into (key, since_iso, note). 'login' = since the previous login."""
    key = request.args.get('range', 'login')
    now = time.time(); note = ''
    if key == 'login':
        prev = db.get_setting('prev_login', '')
        if prev: return key, prev, note
        key, note = '24h', 'No earlier login on record yet, so showing the last 24 hours.'
    if key not in RANGES: key = '24h'
    return key, time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(now - RANGES[key] * 3600)), note

@app.route('/report')
def report():
    if not auth(): return redirect(url_for('login'))
    try:
        key, since, note = _window()
        asset = request.args.get('asset') if request.args.get('asset') in KNOWN_COINS else None
        d = db.report_data(since, asset)
        t = d['total']
        # Coverage = share of minutes in the window that actually have scans. Measured from the
        # first recorded scan, so a bot installed 3 hours ago is not penalised for a 24h window.
        coverage = None
        if t['minutes']:
            start = max(calendar.timegm(time.strptime(since[:16] + 'Z', '%Y-%m-%dT%H:%MZ')),
                        calendar.timegm(time.strptime(t['first_minute'], '%Y-%m-%dT%H:%MZ')))
            expected = max(1, int((time.time() - start) // 60) + 1)
            coverage = min(100.0, t['minutes'] * 100.0 / expected)
        return render_template('report.html', d=d, key=key, since=since, note=note, coverage=coverage, asset=asset, coins=KNOWN_COINS,
                               bot=bot, ranges=list(RANGES), has_prev=bool(db.get_setting('prev_login', '')))
    except Exception as e:
        request_id = secrets.token_hex(4)
        log.error('Report failure [%s]: %s\n%s', request_id, e, traceback.format_exc())
        return render_template('error.html', request_id=request_id, error=str(e)), 500

@app.route('/export/scans.csv')
def export_scans():
    if not auth(): return redirect(url_for('login'))
    key, since, _ = _window()
    asset = request.args.get('asset') if request.args.get('asset') in KNOWN_COINS else None
    out = io.StringIO(); w = csv.writer(out)
    cols = ['minute','asset','scans','markets_seen','best_gross','best_net','min_pair','raw_scans','qualified_scans','qual_secs','errors','last_error']
    w.writerow(cols)
    for r in db.scan_rows(since, asset): w.writerow([r[k] for k in cols])
    return Response(out.getvalue(), mimetype='text/csv', headers={'Content-Disposition': f'attachment; filename=underhaven-scans-{asset or "all"}-{key}.csv'})

@app.route('/health')
def health():
    out = dict(ok=True, scanner=bot.running, last_scan=bot.last_scan)
    if auth():   # details only for the logged-in administrator
        fds = len(os.listdir('/proc/self/fd')) if os.path.isdir('/proc/self/fd') else None
        out.update(live=bot.live, last_error=bot.last_error, session_id=bot.session_id, coins=bot.assets,
                   scan_secs=round(bot.last_scan_secs, 2), books_via=bot.pm.books_via, open_files=fds, notes=bot.pm.notes)
    return jsonify(out)

if __name__=='__main__': app.run(host=HOST, port=PORT)
