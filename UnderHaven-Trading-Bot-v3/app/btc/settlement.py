"""Settlement from VERIFIED exchange results. A position is never counted as won because its theoretical payout is $1: it stays OPEN /
AWAITING_SETTLEMENT until the exchange itself reports the winning outcome."""
import time
from . import store
from .calc import d, ZERO


class Settler:
    def __init__(self, mode, results_fn, now=time.time, grace=15):
        self.mode, self.results, self.now, self.grace = mode, results_fn, now, grace

    def run(self):
        settled, touched = [], set()
        for p in store.open_positions(self.mode):
            if p['window_end'] + self.grace > self.now(): continue
            res = None
            try: res = self.results(p['venue'], p['market_id'])
            except Exception as exc: store.event('WARNING', 'RESULT_LOOKUP_FAILED', f'{p["venue"]} {p["market_id"]}: {type(exc).__name__}')
            pos = store.pos_from_row(p)
            if res not in ('up', 'down'):
                if p['status'] != 'AWAITING_SETTLEMENT': store.position_update(p['id'], pos, 'AWAITING_SETTLEMENT')
                continue
            win = (p['side'] == 'UP' and res == 'up') or (p['side'] == 'DOWN' and res == 'down')
            pos.settle(1 if win else 0)
            store.position_update(p['id'], pos, 'SETTLED', settle_value=1 if win else 0)
            settled.append((p['id'], p['side'], res, win)); touched.add(p['group_id'])
        for gid in touched:
            legs = store.positions(self.mode, group_id=gid)
            if len(legs) == 2 and all(l['status'] == 'SETTLED' for l in legs) and len({l['venue'] for l in legs}) == 2:
                wins = sum(1 for l in legs if l['settle_value'] == 1)
                if wins != 1: store.event('WARNING', 'SETTLEMENT_MISMATCH', f'Cross-platform pair settled with {wins} winning legs (expected exactly 1): the two venues resolved differently.', {'group': gid})
        return settled
