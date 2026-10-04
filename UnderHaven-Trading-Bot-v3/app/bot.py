import threading, time, json
from . import db
from .polymarket import PolymarketService

class Bot:
    def __init__(self):
        self.pm = PolymarketService()
        self.running = False
        self.live = False
        self.thread = None
        self.markets = []
        self.last_error = ''
        self.last_scan = None
        self.opportunities = 0
        self.scans = 0
        self.session_id = None
        self.paper_trade_size = float(db.get_setting('paper_size','10'))
        self.live_size = float(db.get_setting('live_size','10'))
        self.min_net_edge = float(db.get_setting('min_net_edge','0.01'))
        self.max_daily_loss = float(db.get_setting('max_daily_loss','5'))
        self.cooldown_seconds = int(db.get_setting('paper_cooldown','30'))
        self.auto_merge = db.get_setting('auto_merge','1') == '1'
        self._last_paper = {}
        self._live_lock = threading.Lock()
        try: db.prune(30)
        except Exception: pass
        self.start()

    def start(self):
        if self.running: return
        self.running = True
        self.session_id = db.start_paper_session()
        self.thread = threading.Thread(target=self.loop, daemon=True, name='underhaven-bot')
        self.thread.start()

    def stop(self):
        self.running = False
        if self.session_id:
            db.update_paper_session(self.session_id, status='ENDED', ended_at=self._now())

    def set_live(self, enabled):
        self.live = bool(enabled)
        db.set_setting('live_enabled','1' if self.live else '0')

    @staticmethod
    def _now(): return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())

    def loop(self):
        while self.running:
            started = time.time()
            try:
                self.markets = self.pm.scan(self.min_net_edge, self.live_size if self.live else self.paper_trade_size, 'taker')
                self.scans += 1
                self.opportunities = sum(1 for x in self.markets if x.get('qualified'))
                self.last_scan = self._now()
                self.last_error = self.pm.last_error
                self._record(self.markets)
                db.record_scan(self.session_id, self.markets, self.last_error)
                self._paper_simulate(self.markets)
                if self.live and self.pm.creds():
                    self.execute_best()
            except Exception as e:
                self.last_error = str(e)
                try: db.record_scan(self.session_id, [], str(e))
                except Exception: pass
            elapsed = time.time() - started
            time.sleep(max(1.0, 2.0 - elapsed))

    def _record(self, markets):
        sid = self.session_id
        for x in markets:
            # Persist every positive-gross gap so the 24-hour study can distinguish
            # gross opportunities from opportunities that actually survive costs.
            if x.get('gross_edge', 0) <= 0: continue
            reason = '' if x.get('qualified') else 'NET_EDGE_BELOW_SAFETY_MARGIN_OR_COSTS'
            db.add_opportunity((
                sid,self._now(),x['question'],x['slug'],x['yes_ask'],x['no_ask'],x['pair_cost'],
                x['gross_edge'],x['fee_estimate'],x['net_edge'],x['executable_shares'],x['executable_spend'],
                x['liquidity_yes'],x['liquidity_no'],1 if x.get('qualified') else 0,reason))
        db.update_paper_session(sid, scans=self.scans, markets_seen=len(markets), opportunities=self.opportunities)

    def _paper_simulate(self, markets):
        now = time.time()
        candidates = [x for x in markets if x.get('qualified') and x.get('net_profit',0) > 0]
        if not candidates: return
        c = candidates[0]
        key = c['condition_id'] or c['slug'] or c['question']
        if now - self._last_paper.get(key, 0) < self.cooldown_seconds: return
        self._last_paper[key] = now
        pnl = float(c['net_profit'])
        # Paper trade assumes both legs are filled at the measured executable VWAP and
        # then redeemed/merged for $1 per matched share. This is a simulation, not a fill.
        db.add_trade((self.session_id,self._now(),'PAPER',c['question'],'SIM-YES','SIM-NO','SIMULATED_BOTH_LEGS',
                      c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],pnl,1,
                      'Hypothetical pair redemption/merge; not a real fill'))
        s = db.current_paper_session() or {}
        db.update_paper_session(self.session_id,
            paper_trades=int(s.get('paper_trades',0))+1,
            wins=int(s.get('wins',0))+1,
            simulated_spend=float(s.get('simulated_spend',0))+c['executable_spend'],
            simulated_pnl=float(s.get('simulated_pnl',0))+pnl,
            scans=self.scans, markets_seen=len(markets), opportunities=self.opportunities)

    def execute_best(self):
        if not self._live_lock.acquire(blocking=False): return
        try:
            candidates=[x for x in self.markets if x.get('qualified') and x.get('net_profit',0) > 0]
            if not candidates: return
            c=candidates[0]
            # Hard daily loss guard based on recorded LIVE trades.
            if self.live_daily_pnl() <= -abs(self.max_daily_loss):
                self.set_live(False)
                self.last_error = 'LIVE disabled by daily-loss kill switch.'
                return
            from py_clob_client_v2 import OrderType, PartialCreateOrderOptions, Side
            client=self.pm.client()
            if not client: return
            shares=float(c.get('executable_shares',0))
            if shares <= 0: return
            # The two legs must be the SAME number of shares. We therefore use FOK limit
            # orders sized in shares, with each limit set to the worst ask required to
            # fill the measured depth. This is safer for pair matching than spending the
            # same USDC amount on both legs.
            def worst_ask(token, wanted):
                book=client.get_order_book(token)
                levels=self.pm._levels(book,'asks')
                remaining=wanted; worst=None
                for price,size in levels:
                    take=min(remaining,size); worst=price; remaining-=take
                    if remaining <= 1e-9: return worst
                return None
            y_price=worst_ask(c['tokens'][0], shares); n_price=worst_ask(c['tokens'][1], shares)
            if y_price is None or n_price is None: return
            from py_clob_client_v2 import OrderArgs
            y=client.create_and_post_order(OrderArgs(token_id=c['tokens'][0],price=float(y_price),side=Side.BUY,size=shares),options=PartialCreateOrderOptions(tick_size='0.01'),order_type=OrderType.FOK)
            if not self._filled(y):
                db.add_trade((None,self._now(),'LIVE',c['question'],json.dumps(str(y)),'','FIRST_LEG_NOT_FILLED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],0,0,'First FOK leg not confirmed; live mode stopped.'))
                self.set_live(False); return
            n=client.create_and_post_order(OrderArgs(token_id=c['tokens'][1],price=float(n_price),side=Side.BUY,size=shares),options=PartialCreateOrderOptions(tick_size='0.01'),order_type=OrderType.FOK)
            if not self._filled(n):
                db.add_trade((None,self._now(),'LIVE',c['question'],str(y),str(n),'SECOND_LEG_NOT_FILLED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],0,0,'Second FOK leg not confirmed. Live mode stopped; unwind is required.'))
                self.set_live(False); return
            pnl = shares * c['net_edge']
            db.add_trade((None,self._now(),'LIVE',c['question'],str(y),str(n),'BOTH_LEGS_FILLED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],pnl,1,'Both equal-share FOK legs confirmed.'))
            if self.auto_merge and c.get('condition_id'):
                self.try_merge(c['condition_id'], shares)
        except Exception as e:
            self.last_error='LIVE EXECUTION: '+str(e)
            self.set_live(False)
        finally:
            self._live_lock.release()

    def try_merge(self, condition_id, shares):
        try:
            # Current official SDK can inspect/merge complementary positions and can
            # route wallet/relayer transactions. Keep this best-effort and never treat
            # a merge attempt as proof of profit.
            from polymarket import SecureClient
            pk=self.pm.vault.decrypt(self.pm.creds()['private_key'])
            wallet=self.pm.creds()['funder']
            with SecureClient.create(private_key=pk, wallet=wallet) as client:
                amount=max(1, int(float(shares) * 1_000_000))
                handle=client.merge_positions(condition_id=condition_id, amount=amount, metadata='UnderHaven auto-merge arbitrage pair')
                outcome=handle.wait()
            db.add_trade((None,self._now(),'LIVE',condition_id,'','', 'MERGE_SUBMITTED',0,0,0,0,0,1,'Auto-merge transaction result: '+str(outcome)))
        except Exception as e:
            self.last_error='AUTO-MERGE: '+str(e)
            db.add_trade((None,self._now(),'LIVE',condition_id,'','', 'MERGE_FAILED',0,0,0,0,0,0,'Auto-merge failed: '+str(e)))

    def live_daily_pnl(self):
        from datetime import datetime, timezone
        today=time.strftime('%Y-%m-%d', time.gmtime())
        with db.connect() as c:
            r=c.execute("SELECT COALESCE(SUM(pnl),0) AS p FROM trades WHERE mode='LIVE' AND substr(ts,1,10)=?",(today,)).fetchone()
            return float(r['p'])

    @staticmethod
    def _filled(resp):
        if isinstance(resp, dict):
            s=' '.join(str(resp.get(k,'')) for k in ('status','order_status','state','success','result')).lower()
            if any(x in s for x in ('matched','filled','success','confirmed')): return True
            for k in ('data','order'):
                if isinstance(resp.get(k),dict) and any(x in str(resp[k].get(a,'')).lower() for a in ('status','state') for x in ('matched','filled','success')):
                    return True
        return any(x in str(resp).lower() for x in ('matched','filled','success'))

bot=Bot()
