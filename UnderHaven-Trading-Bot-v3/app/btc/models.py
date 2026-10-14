"""Normalised BTC Up/Down contract and the strict cross-platform match result."""
import re
from dataclasses import dataclass, field, asdict
from decimal import Decimal
from typing import List, Optional


@dataclass
class BtcMarket:
    venue: str                       # 'polymarket' | 'kalshi'
    market_id: str                   # Polymarket market id | Kalshi ticker
    duration: int                    # 300 | 900 seconds
    start_ts: float                  # window start (epoch)
    end_ts: float                    # window end / settlement reference time
    title: str = ''
    rules: str = ''
    up_id: str = ''                  # Polymarket outcome asset id for UP
    down_id: str = ''                # Polymarket outcome asset id for DOWN (Kalshi: blank; YES=UP, NO=DOWN)
    slug: str = ''
    tradable: bool = False
    status: str = ''
    tick: Decimal = Decimal('0.01')
    min_notional: Decimal = Decimal('0')       # Polymarket orderMinSize is a USDC notional
    min_qty: Decimal = Decimal('1')            # Kalshi: whole contracts
    seconds_delay: float = 0.0
    fee_rate: Decimal = Decimal('0.07')        # polymarket feeSchedule.rate
    fee_exponent: Decimal = Decimal('1')
    fee_multiplier: Decimal = Decimal('1')     # kalshi
    maker_fees: bool = False
    fee_known: bool = False                    # True when the exchange's own data stated the fee parameters
    oracle: str = 'UNKNOWN'                    # CHAINLINK | CF_BENCHMARKS | BINANCE | COINBASE | UNKNOWN
    averaging: str = 'UNKNOWN'                 # POINT | AVG_60S | UNKNOWN
    tie_rule: str = 'UNKNOWN'                  # UP_ON_TIE | DOWN_ON_TIE | UNKNOWN
    outcome_ok: bool = False                   # UP/DOWN meaning verified
    result: Optional[str] = None               # 'up' | 'down' once settled
    flags: List[str] = field(default_factory=list)

    @property
    def key(self): return f'{self.venue}:{self.market_id}'

    def to_dict(self):
        d = asdict(self)
        return {k: (str(v) if isinstance(v, Decimal) else v) for k, v in d.items()}


@dataclass
class Check:
    name: str
    verdict: str          # PASS | REVIEW | REJECT
    detail: str = ''


@dataclass
class MatchResult:
    status: str           # VERIFIED | NEEDS_REVIEW | REJECTED
    checks: List[Check]

    def to_dict(self):
        return {'status': self.status, 'checks': [{'name': c.name, 'verdict': c.verdict, 'detail': c.detail} for c in self.checks]}

    @property
    def reasons(self): return [f'{c.name}: {c.detail}' for c in self.checks if c.verdict != 'PASS']


def detect_oracle(text):
    s = (text or '').lower()
    if 'chainlink' in s: return 'CHAINLINK'
    if re.search(r'cf benchmarks|\bbrti\b|real[- ]time index|\brti\b', s): return 'CF_BENCHMARKS'
    if 'binance' in s: return 'BINANCE'
    if 'coinbase' in s: return 'COINBASE'
    if 'pyth' in s: return 'PYTH'
    return 'UNKNOWN'


def detect_averaging(text):
    s = (text or '').lower()
    if re.search(r'60[- ]second|sixty[- ]second|average of (?:the )?(?:60|sixty)|simple average', s): return 'AVG_60S'
    if re.search(r'price (?:at|of) the (?:end|beginning|start)|closing price|at the end of the|at the beginning of the|price to beat', s): return 'POINT'
    return 'UNKNOWN'


def detect_tie(text):
    s = (text or '').lower()
    if re.search(r'greater than or equal|at or above|equal to or (?:greater|higher|above)|at least|or higher|>=|≥', s): return 'UP_ON_TIE'
    if re.search(r'strictly (?:greater|higher|above)|greater than the|higher than the|\bhigher than\b|\bgreater than\b|>', s): return 'DOWN_ON_TIE'
    return 'UNKNOWN'


@dataclass
class PairInfo:
    """A Polymarket window and the Kalshi window it is being compared with, plus the strict match verdict."""
    poly: BtcMarket
    kal: BtcMarket
    match: MatchResult

    @property
    def key(self): return f'{self.poly.market_id}|{self.kal.market_id}'
    @property
    def status(self): return self.match.status
    @property
    def reasons(self): return self.match.reasons
