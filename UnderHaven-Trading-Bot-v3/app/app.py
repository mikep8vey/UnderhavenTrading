from flask import Flask, render_template, request, redirect, url_for, session, jsonify, flash, Response
from werkzeug.security import generate_password_hash, check_password_hash
from . import db
from .bot import bot
from .config import HOST, PORT
import os, secrets, csv, io, time, calendar, logging, traceback

app = Flask(__name__, static_folder='../static', template_folder='../templates')
app.secret_key = os.environ.get('UNDERHAVEN_SESSION_SECRET') or secrets.token_hex(32)
logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(name)s: %(message)s')
log = logging.getLogger('underhaven')

def auth(): return bool(session.get('admin'))
def setup_done(): return bool(db.get_setting('admin_hash'))

@app.route('/login', methods=['GET','POST'])
def login():
    if request.method == 'POST':
        if request.form.get('username') == db.get_setting('admin_user','') and check_password_hash(db.get_setting('admin_hash',''), request.form.get('password','')):
            session['admin'] = True
            db.set_setting('prev_login', db.get_setting('last_login', ''))
            db.set_setting('last_login', time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()))
            return redirect(url_for('dashboard'))
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
        trades = db.recent_trades(100)
        opps = db.recent_opportunities(100)
        return render_template('dashboard.html', bot=bot, creds=creds, balance=bal, opps=opps, trades=trades, paper=paper)
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
    bot.min_net_edge=max(0.0,float(request.form.get('min_net_edge','0.01')))
    bot.paper_trade_size=max(0.1,float(request.form.get('paper_size','10')))
    bot.live_size=max(0.1,float(request.form.get('live_size','10')))
    bot.max_daily_loss=max(0.0,float(request.form.get('max_daily_loss','5')))
    bot.cooldown_seconds=max(1,int(request.form.get('paper_cooldown','30')))
    bot.auto_merge=request.form.get('auto_merge')=='1'
    for k,v in [('min_net_edge',bot.min_net_edge),('paper_size',bot.paper_trade_size),('live_size',bot.live_size),('max_daily_loss',bot.max_daily_loss),('paper_cooldown',bot.cooldown_seconds),('auto_merge','1' if bot.auto_merge else '0')]: db.set_setting(k,v)
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
        r=bot.pm.save_connection(pk,funder,sig); flash('Polymarket signer authenticated. CLOB API credentials were created/derived automatically.')
    except Exception as e: flash('Polymarket connection failed: '+str(e))
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
    w.writerow(['id','timestamp','market','slug','yes_ask','no_ask','pair_cost','gross_edge','fee_estimate','net_edge','shares','spend','yes_liquidity','no_liquidity','qualified'])
    with db.connect() as c:
        for r in c.execute('SELECT * FROM opportunities WHERE session_id=? ORDER BY id',(sid,)):
            w.writerow([r[k] for k in ['id','ts','market','slug','yes_ask','no_ask','pair_cost','gross_edge','fee_estimate','net_edge','executable_shares','executable_spend','yes_liquidity','no_liquidity','qualified']])
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
        d = db.report_data(since)
        t = d['total']
        # Coverage = share of minutes in the window that actually have scans. Measured from the
        # first recorded scan, so a bot installed 3 hours ago is not penalised for a 24h window.
        coverage = None
        if t['minutes']:
            start = max(calendar.timegm(time.strptime(since[:16] + 'Z', '%Y-%m-%dT%H:%MZ')),
                        calendar.timegm(time.strptime(t['first_minute'], '%Y-%m-%dT%H:%MZ')))
            expected = max(1, int((time.time() - start) // 60) + 1)
            coverage = min(100.0, t['minutes'] * 100.0 / expected)
        return render_template('report.html', d=d, key=key, since=since, note=note, coverage=coverage,
                               bot=bot, ranges=list(RANGES), has_prev=bool(db.get_setting('prev_login', '')))
    except Exception as e:
        request_id = secrets.token_hex(4)
        log.error('Report failure [%s]: %s\n%s', request_id, e, traceback.format_exc())
        return render_template('error.html', request_id=request_id, error=str(e)), 500

@app.route('/export/scans.csv')
def export_scans():
    if not auth(): return redirect(url_for('login'))
    key, since, _ = _window()
    out = io.StringIO(); w = csv.writer(out)
    cols = ['minute','scans','markets_seen','best_gross','best_net','min_pair','qualified_scans','errors','last_error']
    w.writerow(cols)
    for r in db.scan_rows(since): w.writerow([r[k] for k in cols])
    return Response(out.getvalue(), mimetype='text/csv', headers={'Content-Disposition': f'attachment; filename=underhaven-scans-{key}.csv'})

@app.route('/health')
def health(): return jsonify(ok=True,scanner=bot.running,live=bot.live,last_error=bot.last_error,session_id=bot.session_id,last_scan=bot.last_scan)

if __name__=='__main__': app.run(host=HOST, port=PORT)
