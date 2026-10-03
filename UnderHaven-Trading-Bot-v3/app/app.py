from flask import Flask, render_template, request, redirect, url_for, session, jsonify, flash, Response
from werkzeug.security import generate_password_hash, check_password_hash
from . import db
from .bot import bot
from .config import HOST, PORT
import os, secrets, csv, io, time

app = Flask(__name__, static_folder='../static', template_folder='../templates')
app.secret_key = os.environ.get('UNDERHAVEN_SESSION_SECRET') or secrets.token_hex(32)

def auth(): return bool(session.get('admin'))
def setup_done(): return bool(db.get_setting('admin_hash'))

@app.route('/login', methods=['GET','POST'])
def login():
    if request.method == 'POST':
        if request.form.get('username') == db.get_setting('admin_user','') and check_password_hash(db.get_setting('admin_hash',''), request.form.get('password','')):
            session['admin'] = True
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
    creds=bot.pm.creds(); bal=None
    if creds:
        try: bal=bot.pm.balance()
        except Exception as e: bot.last_error='BALANCE: '+str(e)
    paper=db.latest_paper_session()
    trades=db.recent_trades(100)
    opps=db.recent_opportunities(100)
    return render_template('dashboard.html', bot=bot, creds=creds, balance=bal, opps=opps, trades=trades, paper=paper)

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

@app.route('/health')
def health(): return jsonify(ok=True,scanner=bot.running,live=bot.live,last_error=bot.last_error,session_id=bot.session_id,last_scan=bot.last_scan)

if __name__=='__main__': app.run(host=HOST, port=PORT)
