"""Typed, server-validated settings (conservative defaults). Stored in the settings table with the prefix b_. Live mode is never read from here at startup."""
from dataclasses import dataclass, fields, asdict

RANGES = {
    'scan_interval': (1, 60, 'seconds between scans'), 'max_quote_age': (1, 60, 'max age of an order-book observation (s)'),
    'max_spread': (0.01, 0.5, 'max bid-ask spread ($)'), 'min_depth': (1, 1e6, 'min contracts within the top 3 levels'),
    'min_net_edge': (0.0, 0.5, 'min expected net edge per contract after all costs ($)'), 'safety_margin': (0.0, 0.2, 'extra margin per contract ($)'),
    'slippage': (0.0, 0.1, 'assumed adverse price move per contract per leg ($)'),
    'max_order_notional': (1, 1e5, 'max order size ($)'), 'max_capital_per_trade': (1, 1e5, 'max capital per trade ($)'),
    'max_exposure_platform': (1, 1e6, 'max open capital per platform ($)'), 'max_total_exposure': (1, 1e6, 'max open capital in total ($)'),
    'max_open_positions': (1, 1000, 'max simultaneous open trades'), 'max_daily_loss': (0, 1e6, 'daily loss that trips the kill switch ($)'),
    'max_drawdown': (0, 1e6, 'drawdown that trips the kill switch ($)'), 'max_unmatched': (0, 1e5, 'max one-sided cross-platform exposure ($)'),
    'retry_count': (0, 5, 'order retries'), 'max_exec_seconds': (2, 120, 'max seconds to complete the second leg'), 'orders_per_minute': (1, 600, 'order submissions per minute'),
    'balance_polymarket': (1, 1e8, 'paper starting balance, Polymarket ($)'), 'balance_kalshi': (1, 1e8, 'paper starting balance, Kalshi ($)'),
    'depth_haircut': (0.1, 1.0, 'fraction of displayed depth assumed fillable in paper mode'), 'latency': (0.0, 10.0, 'paper latency before the order reaches the book (s)'),
    'min_remaining_5m': (5, 250, 'no new entry with less than this many seconds left (5-minute)'), 'min_remaining_15m': (10, 800, 'same, 15-minute'),
    'min_elapsed_5m': (0, 250, 'no entry in the first N seconds of a 5-minute window'), 'min_elapsed_15m': (0, 800, 'same, 15-minute'),
    'mom_weight': (0.0, 2.0, 'weight of recent BTC drift in the momentum model (0 = none)'), 'mom_lookback': (5, 300, 'momentum look-back (s)'),
    'mr_lookback': (5, 120, 'mean-reversion look-back (s)'), 'mr_jump': (0.02, 0.5, 'market move that counts as a jump'), 'mr_dev': (0.02, 0.5, 'gap to model value that counts as a dislocation'),
    'imb_weight': (0.0, 0.2, 'weight of order-book imbalance in the model probability (0 = filter only)'),
    'take_profit': (0.0, 5.0, 'exit at this gain on cost (0 = off)'), 'stop_loss': (0.0, 1.0, 'exit at this loss on cost (0 = off)'),
    'max_hold': (0, 3600, 'max holding time before exit (s, 0 = hold to settlement)'), 'exit_before_expiry': (0, 600, 'exit this many s before expiry (0 = off)'),
    'live_max_order_polymarket': (1, 1e5, 'LIVE max order, Polymarket ($)'), 'live_max_order_kalshi': (1, 1e5, 'LIVE max order, Kalshi ($)'),
    'live_max_exposure_polymarket': (1, 1e6, 'LIVE max open capital, Polymarket ($)'), 'live_max_exposure_kalshi': (1, 1e6, 'LIVE max open capital, Kalshi ($)'),
    'live_daily_loss_polymarket': (0, 1e6, 'LIVE daily loss limit, Polymarket ($)'), 'live_daily_loss_kalshi': (0, 1e6, 'LIVE daily loss limit, Kalshi ($)'),
}
BOOLS = ('s_arb', 's_momentum', 's_meanrev', 's_bundle', 'd5', 'd15', 'research_xplat')
CHOICES = {'kill_policy': ('hold_to_settlement', 'unwind_open'), 'recovery': ('complete_then_unwind', 'unwind_only', 'alert_only'),
           'leg_order': ('thinner_first', 'polymarket_first', 'kalshi_first'), 'kalshi_order_api': ('legacy', 'v2')}
STRATEGIES = ('arb', 'momentum', 'meanrev', 'bundle')


@dataclass
class BtcSettings:
    scan_interval: float = 2.0
    max_quote_age: float = 5.0
    max_spread: float = 0.10
    min_depth: float = 10.0
    min_net_edge: float = 0.02
    safety_margin: float = 0.01
    slippage: float = 0.005
    max_order_notional: float = 10.0
    max_capital_per_trade: float = 10.0
    max_exposure_platform: float = 40.0
    max_total_exposure: float = 60.0
    max_open_positions: float = 6
    max_daily_loss: float = 5.0
    max_drawdown: float = 15.0
    max_unmatched: float = 5.0
    retry_count: float = 1
    max_exec_seconds: float = 15
    orders_per_minute: float = 30
    balance_polymarket: float = 20.0
    balance_kalshi: float = 20.0
    depth_haircut: float = 0.7
    latency: float = 0.4
    min_remaining_5m: float = 30
    min_remaining_15m: float = 60
    min_elapsed_5m: float = 20
    min_elapsed_15m: float = 60
    mom_weight: float = 0.5
    mom_lookback: float = 30
    mr_lookback: float = 20
    mr_jump: float = 0.08
    mr_dev: float = 0.08
    imb_weight: float = 0.0
    take_profit: float = 0.0
    stop_loss: float = 0.0
    max_hold: float = 0
    exit_before_expiry: float = 0
    live_max_order_polymarket: float = 5.0
    live_max_order_kalshi: float = 5.0
    live_max_exposure_polymarket: float = 10.0
    live_max_exposure_kalshi: float = 10.0
    live_daily_loss_polymarket: float = 3.0
    live_daily_loss_kalshi: float = 3.0
    s_arb: bool = True
    s_momentum: bool = True
    s_meanrev: bool = True
    s_bundle: bool = True
    d5: bool = True
    d15: bool = True
    research_xplat: bool = False
    kill_policy: str = 'hold_to_settlement'
    recovery: str = 'complete_then_unwind'
    leg_order: str = 'thinner_first'
    kalshi_order_api: str = 'legacy'
    live_strategies: tuple = ()

    def to_dict(self):
        d = asdict(self); d['live_strategies'] = list(self.live_strategies); return d


def validate(raw):
    out = {}
    for k, v in raw.items():
        if k in RANGES:
            lo, hi, desc = RANGES[k]
            try: f = float(v)
            except (TypeError, ValueError): raise ValueError(f'{k} ({desc}) must be a number.')
            if f != f or not (lo <= f <= hi): raise ValueError(f'{k} ({desc}) must be between {lo} and {hi}.')
            out[k] = f
        elif k in BOOLS: out[k] = str(v).lower() in ('1', 'true', 'on', 'yes')
        elif k in CHOICES:
            if v not in CHOICES[k]: raise ValueError(f'{k} must be one of {", ".join(CHOICES[k])}.')
            out[k] = v
        elif k == 'live_strategies':
            items = v if isinstance(v, (list, tuple)) else [x for x in str(v).split(',') if x]
            bad = [x for x in items if x not in STRATEGIES]
            if bad: raise ValueError(f'unknown strategy {bad[0]}')
            out[k] = tuple(items)
    return out


def load(getter):
    s, raw = BtcSettings(), {}
    for f in fields(BtcSettings):
        v = getter('b_' + f.name, None)
        if v not in (None, ''): raw[f.name] = v
    for k, v in raw.items():            # a corrupt stored value must never crash the scanner: skip it and keep the safe default
        try:
            for kk, vv in validate({k: v}).items(): setattr(s, kk, vv)
        except ValueError: pass
    return s


def save(setter, clean):
    for k, v in clean.items():
        setter('b_' + k, ','.join(v) if isinstance(v, tuple) else ('1' if v is True else '0' if v is False else v))
