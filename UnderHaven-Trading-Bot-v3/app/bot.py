import threading, time, json
from . import db
from .polymarket import PolymarketService
from .assets import ASSETS

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
        # v3.5 scanner controls
        known = [a['code'] for a in ASSETS]
        saved = [c for c in (db.get_setting('assets', ','.join(known)) or '').split(',') if c in known]
        self.assets = saved or known                                   # coins to scan
        self.slots_ahead = int(db.get_setting('slots_ahead', '2'))     # live window + N upcoming windows
        self.late_skip = int(float(db.get_setting('late_skip', '60'))) # ignore gaps in the last N seconds of a window
        self.confirm = db.get_setting('confirm', '1') == '1'           # re-fetch both books before logging a gap
        self.slip_ticks = float(db.get_setting('slip_ticks', '1'))     # paper/live pessimism: ticks given up per leg
        self.confirm_delay = 0.35
        self.last_scan_secs = 0.0
        self._last_paper = {}
        self._traded = {}   # market slug -> time of its paper trade: at most ONE paper trade per market window
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

    def step(self):
        """One scan cycle. Kept separate from loop() so it can be tested without threads or sleeping."""
        size = self.live_size if self.live else self.paper_trade_size
        self.markets = self.pm.scan(self.min_net_edge, size, 'taker', self.assets, self.slots_ahead, self.slip_ticks, self.late_skip)
        self._confirm(self.markets, size)
        self.scans += 1
        self.opportunities = sum(1 for x in self.markets if x.get('qualified'))
        self.last_scan = self._now()
        self.last_error = self.pm.last_error
        self._record(self.markets)
        db.record_scan(self.session_id, self.markets, self.last_error, self.assets)
        self._paper_simulate(self.markets)
        if self.live and self.pm.creds():
            self.execute_best()

    def loop(self):
        while self.running:
            started = time.time()
            try:
                self.step()
            except Exception as e:
                self.last_error = str(e)
                try: db.record_scan(self.session_id, [], str(e), self.assets)
                except Exception: pass
            self.last_scan_secs = time.time() - started
            time.sleep(max(1.0, 2.0 - self.last_scan_secs))

    def _confirm(self, markets, size):
        """A gap that is real is still there a moment later; a phantom (stale / out-of-sync book) is not.
        Re-fetch both books for the best few raw candidates and keep only those that still clear every guard.
        Live trading ALWAYS requires this, whatever the setting."""
        need = self.confirm or self.live
        raw = [x for x in markets if x.get('raw_qualified')]
        if not raw: return
        if not need:
            return
        for x in raw[:3]:
            if not x.get('qualified'):
                continue   # already rejected by a guard (late window, minimum order size)
            time.sleep(self.confirm_delay)
            try:
                fresh = self.pm.requote(x, self.min_net_edge, size, 'taker', self.slip_ticks, self.late_skip)
            except Exception as e:
                fresh = None; self.pm.last_error = f'RE-CHECK: {e}'
            if fresh and fresh.get('qualified'):
                keep = x['_m']; x.update(fresh); x['_m'] = keep; x['confirmed'] = True
            else:
                x['qualified'] = False; x['confirmed'] = False; x['disproved'] = True
                x['reason'] = 'gap gone on re-check' if fresh is not None else 'book unavailable on re-check'
        for x in raw[3:]:
            if x.get('qualified'):
                x['qualified'] = False; x['reason'] = 'not re-checked (only the best 3 per scan are)'

    def _record(self, markets):
        sid = self.session_id
        for x in markets:
            # Persist every positive-gross gap so the 24-hour study can distinguish
            # gross opportunities from opportunities that actually survive costs.
            if x.get('gross_edge', 0) <= 0: continue
            reason = '' if x.get('qualified') else (x.get('reason') or 'NET_EDGE_BELOW_SAFETY_MARGIN_OR_COSTS')
            db.add_opportunity((
                sid,self._now(),x['question'],x['slug'],x['yes_ask'],x['no_ask'],x['pair_cost'],
                x['gross_edge'],x['fee_estimate'],x['net_edge'],x['executable_shares'],x['executable_spend'],
                x['liquidity_yes'],x['liquidity_no'],1 if x.get('qualified') else 0,reason,x.get('asset','BTC')))
        db.update_paper_session(sid, scans=self.scans, markets_seen=len(markets), opportunities=self.opportunities)

    def _paper_simulate(self, markets):
        """Log a hypothetical pair trade for each confirmed gap, at most ONE per market window (a gap that
        stays open is the same money, not new money: before v3.5 the same window could be counted twice)."""
        now = time.time()
        self._traded = {k: v for k, v in self._traded.items() if now - v < 3600}
        done = 0
        for c in [x for x in markets if x.get('qualified') and x.get('net_profit', 0) > 0]:
            key = c.get('slug') or c.get('condition_id') or c['question']
            if key in self._traded or done >= 3: continue
            if now - self._last_paper.get(c.get('asset', 'BTC'), 0) < self.cooldown_seconds: continue
            self._traded[key] = now; self._last_paper[c.get('asset', 'BTC')] = now; done += 1
            pnl = float(c['net_profit'])
            # Paper trade assumes both legs are filled at the measured executable VWAP (minus the slippage
            # allowance) and then redeemed/merged for $1 per matched share. This is a simulation, not a fill.
            db.add_trade((self.session_id,self._now(),'PAPER',c['question'],'SIM-YES','SIM-NO','SIMULATED_BOTH_LEGS',
                          c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],pnl,1,
                          'Hypothetical pair redemption/merge; not a real fill' + (' (re-checked)' if c.get('confirmed') else ''),
                          c.get('asset', 'BTC')))
            s = db.current_paper_session() or {}
            db.update_paper_session(self.session_id,
                paper_trades=int(s.get('paper_trades',0))+1,
                wins=int(s.get('wins',0))+1,
                simulated_spend=float(s.get('simulated_spend',0))+c['executable_spend'],
                simulated_pnl=float(s.get('simulated_pnl',0))+pnl,
                scans=self.scans, markets_seen=len(markets), opportunities=self.opportunities)

    def execute_best(self):
        """LIVE: trade every coin's confirmed gap this scan (best first, at most 3), one window at a time."""
        if not self._live_lock.acquire(blocking=False): return
        try:
            done = 0
            while self.live and done < 3:
                # Only gaps that survived the re-check, never the same window twice.
                candidates=[x for x in self.markets if x.get('qualified') and x.get('confirmed') and x.get('net_profit',0) > 0
                            and (x.get('slug') or x['question']) not in self._traded]
                if not candidates: break
                c=candidates[0]
                self._traded[c.get('slug') or c['question']] = time.time()
                self._execute_one(c); done += 1
        except Exception as e:
            self.last_error='LIVE EXECUTION: '+str(e)
            self.set_live(False)
        finally:
            self._live_lock.release()

    def _execute_one(self, c):
        try:
            # Hard daily loss guard. Failed/unwound trades are booked as (worst-case) losses below, so this can trip.
            if self.live_daily_pnl() <= -abs(self.max_daily_loss):
                self.set_live(False)
                self.last_error = 'LIVE disabled by daily-loss kill switch.'
                return
            from py_clob_client_v2 import OrderType, PartialCreateOrderOptions, Side, OrderArgs
            client=self.pm.client()
            if not client: return
            shares=float(c.get('executable_shares',0))
            if shares <= 0: return
            asset=c.get('asset','BTC')
            tick='%g' % float(c.get('tick_size') or 0.01)
            opts=PartialCreateOrderOptions(tick_size=tick if tick in ('0.1','0.01','0.001','0.0001') else '0.01')
            # The two legs must be the SAME number of shares. We therefore use FOK limit orders sized in
            # shares, each limited to the worst price needed to fill the measured depth.
            def worst_price(token, wanted, side):
                book=client.get_order_book(token)
                levels=self.pm._levels(book,side)   # asks ascending / bids descending
                remaining=wanted; worst=None
                for price,size in levels:
                    take=min(remaining,size); worst=price; remaining-=take
                    if remaining <= 1e-9: return worst
                return None
            yt,nt=c['tokens'][0],c['tokens'][1]
            y_price=worst_price(yt, shares, 'asks'); n_price=worst_price(nt, shares, 'asks')
            if y_price is None or n_price is None: return
            # Profit lock: each FOK order is limited to the worst price above, so the most this pair can cost is
            # fixed. Trade only if the pair is STILL profitable after fees at those worst prices.
            rate=float(c.get('fee_rate') or 0.07)
            worst_net=shares*(1.0-float(y_price)-float(n_price))-self.pm.fee_for(shares,float(y_price),rate)-self.pm.fee_for(shares,float(n_price),rate)
            if worst_net/shares < self.min_net_edge:
                self.last_error=f"LIVE skipped {asset}: not profitable at the worst fill price ({worst_net/shares:+.2%}/share)."
                return
            y=client.create_and_post_order(OrderArgs(token_id=yt,price=float(y_price),side=Side.BUY,size=shares),options=opts,order_type=OrderType.FOK)
            if not self._filled(y):
                db.add_trade((None,self._now(),'LIVE',c['question'],json.dumps(str(y)),'','FIRST_LEG_NOT_FILLED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],0,0,'First FOK leg not confirmed; nothing is held. Live mode stopped.',asset))
                self.set_live(False); return
            n=client.create_and_post_order(OrderArgs(token_id=nt,price=float(n_price),side=Side.BUY,size=shares),options=opts,order_type=OrderType.FOK)
            if not self._filled(n):
                self._unwind(client, c, yt, shares, y_price, y, n, opts, worst_price, asset)
                return
            pnl = shares * c['net_edge']
            db.add_trade((None,self._now(),'LIVE',c['question'],str(y),str(n),'BOTH_LEGS_FILLED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],pnl,1,'Both equal-share FOK legs confirmed. P&L is an ESTIMATE from the quoted prices; check the wallet.',asset))
            if self.auto_merge and c.get('condition_id'):
                self.try_merge(c['condition_id'], shares)
        except Exception as e:
            self.last_error='LIVE EXECUTION: '+str(e)
            self.set_live(False)

    def _unwind(self, client, c, token, shares, buy_price, y, n, opts, worst_price, asset):
        """Leg 1 filled but leg 2 did not: we hold one side only. Try to sell it straight back (FOK at the
        worst bid needed) and book the WORST-CASE loss so the daily-loss stop can see it."""
        from py_clob_client_v2 import OrderType, Side, OrderArgs
        cost = shares * float(buy_price)
        try:
            bid = worst_price(token, shares, 'bids')
            if bid is None: raise RuntimeError('not enough bid depth to sell back')
            sold = client.create_and_post_order(OrderArgs(token_id=token,price=float(bid),side=Side.SELL,size=shares),options=opts,order_type=OrderType.FOK)
            if self._filled(sold):
                loss = max(0.0, cost - shares * float(bid))
                db.add_trade((None,self._now(),'LIVE',c['question'],str(y),str(n),'SECOND_LEG_FAILED_UNWOUND',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],-loss,0,'Second leg failed; first leg sold back. Loss shown is a worst-case bound.',asset))
                self.last_error='LIVE: second leg failed, first leg was sold back. Live mode stopped.'
                self.set_live(False); return
            raise RuntimeError('sell-back order was not filled')
        except Exception as e:
            db.add_trade((None,self._now(),'LIVE',c['question'],str(y),str(n),'SECOND_LEG_FAILED_UNWIND_FAILED',c['executable_spend'],c['gross_edge'],c['fee_estimate'],c['net_edge'],-cost,0,
                          f'MANUAL ACTION REQUIRED: you are holding {shares:.4f} shares of one side ({token[:12]}…). Unwind failed: {e}',asset))
            self.last_error='LIVE: MANUAL ACTION REQUIRED, one-sided position held. Live mode stopped.'
            self.set_live(False)

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
        """True only when the response clearly says the FOK order matched. A response such as
        {'success': False, ...} is never a fill (the old check matched the KEY name 'success')."""
        d = resp if isinstance(resp, dict) else getattr(resp, '__dict__', None)
        if not isinstance(d, dict): return False
        if d.get('success') is False or str(d.get('success')).strip().lower() == 'false': return False
        if d.get('errorMsg') or d.get('error') or d.get('error_message'): return False
        status = str(d.get('status') or d.get('order_status') or d.get('state') or '').strip().lower()
        return status in ('matched', 'filled', 'confirmed')

bot=Bot()
