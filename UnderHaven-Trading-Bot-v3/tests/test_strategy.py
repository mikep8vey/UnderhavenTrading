from app.polymarket import PolymarketService


def test_fee_formula_crypto():
    assert PolymarketService.fee_for(100, 0.50, 0.07) == 1.75


def test_quote_walks_depth():
    levels=[(0.50, 5), (0.51, 10)]
    cost,vwap=PolymarketService.quote_buy(levels, 10)
    assert round(cost,2)==5.05
    assert round(vwap,4)==0.505


def test_pair_net_edge():
    yes=[(0.54,100)]
    no=[(0.43,100)]
    y=PolymarketService.quote_buy(yes,100)
    n=PolymarketService.quote_buy(no,100)
    gross=100-(y[0]+n[0])
    fee=PolymarketService.fee_for(100,y[1],0.07)+PolymarketService.fee_for(100,n[1],0.07)
    assert gross == 3.0
    assert fee > gross
