"""Process-wide singletons. Importing this module creates the objects but starts NO threads; run.py starts the scanner."""
from .polymarket import PolymarketService
from .btc.feeds import PriceFeed
from .btc.service import BtcService

pm = PolymarketService()
feed = PriceFeed()
svc = BtcService(pm, feed)


def start():
    return svc.start()
