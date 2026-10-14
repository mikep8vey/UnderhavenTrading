"""Browser layout check (optional, needs playwright + chromium): python3 tests/layout_check.py
Serves the real app with a populated fixture state and verifies that no page overflows horizontally at common screen widths."""
import os, re, sys, tempfile, threading
HERE = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, HERE); sys.path.insert(0, os.path.dirname(HERE))
os.environ['UNDERHAVEN_DATA_DIR'] = tempfile.mkdtemp(); os.environ['UNDERHAVEN_VAULT_KEY_FILE'] = os.environ['UNDERHAVEN_DATA_DIR'] + '/v.key'
import app.app as A
from app import db
from werkzeug.security import generate_password_hash
from werkzeug.serving import make_server
from test_engine import world, setting, SAME, BASE, rows
from fake_btc import KAL_RULES

svc, pm, k, clock = world(now_price=60300)
pm.add_window(900, BASE, up=(0.60, 200), down=(0.42, 200), up_bid=0.58, down_bid=0.40, rules=SAME); pm.add_window(300, BASE, up=(0.55, 200), down=(0.47, 200), up_bid=0.53, down_bid=0.45, rules=SAME)
k.add(900, BASE, yes_ask=0.62, no_ask=0.42, rules=KAL_RULES)
svc.cycle(); clock.advance(30); svc.feed.history.append((clock(), 60300.0, 'fixture')); svc.cycle()
A.svc = svc
db.set_setting('admin_user', 'a'); db.set_setting('admin_hash', generate_password_hash('x' * 10))
srv = make_server('127.0.0.1', 5057, A.app, threaded=True); threading.Thread(target=srv.serve_forever, daemon=True).start()
from playwright.sync_api import sync_playwright
widths = [320, 360, 390, 414, 600, 768, 1024, 1280, 1366, 1440, 1920, 2560]; pages = ['/', '/markets', '/strategies', '/paper', '/live', '/settings']; bad = 0
os.makedirs('/tmp/shots', exist_ok=True)
with sync_playwright() as p:
    b = p.chromium.launch(); pg = b.new_page(viewport={'width': 1366, 'height': 900}); errs = []
    pg.on('pageerror', lambda e: errs.append(str(e)))
    pg.goto('http://127.0.0.1:5057/login'); pg.fill('input[name=username]', 'a'); pg.fill('input[name=password]', 'x' * 10); pg.click('form button.btn'); pg.wait_for_url('http://127.0.0.1:5057/')
    for path in pages:
        pg.goto('http://127.0.0.1:5057' + path)
        for w in widths:
            pg.set_viewport_size({'width': w, 'height': 900}); pg.wait_for_timeout(120)
            pg.evaluate("document.querySelectorAll('details').forEach(d=>d.open=true)")
            r = pg.evaluate('''()=>{const cw=document.documentElement.clientWidth;const over=[];document.querySelectorAll('body *').forEach(e=>{if(e.closest('.table-wrap')||e.closest('svg'))return;const r=e.getBoundingClientRect();if(r.width&&r.right>cw+1)over.push(e.tagName+'.'+(e.className||'').toString().slice(0,18))});return {sw:document.documentElement.scrollWidth,cw:cw,over:[...new Set(over)].slice(0,4)}}''')
            if r['sw'] > r['cw'] + 1 or r['over']: bad += 1; print('OVERFLOW', path, w, r)
            if w in (390, 1366) and path in ('/', '/paper', '/markets'): pg.screenshot(path=f'/tmp/shots/btc2_{path.strip("/") or "overview"}_{w}.png', full_page=True)
    print('JS errors:', errs[:3]); print(f'checked {len(pages) * len(widths)} page/width combinations; overflow problems: {bad}')
    b.close()
srv.shutdown()
