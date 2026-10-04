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


def test_btc_15m_filter_accepts_15m_and_rejects_5m():
    # Regression: a plain '5m' substring check used to reject every 15m market.
    f = PolymarketService.is_btc_15m
    assert f({'question': 'Bitcoin Up or Down 15m', 'slug': 'btc-updown-15m-1759500900'})
    assert not f({'question': 'Bitcoin Up or Down 5m', 'slug': 'btc-updown-5m-1759500900'})
    assert not f({'question': 'Ethereum Up or Down 15m', 'slug': 'eth-updown-15m-1759500900'})


def test_scan_history_aggregates_per_minute(tmp_path):
    from app import db
    db.DB_PATH = tmp_path / 't.db'
    sid = db.start_paper_session()
    db.record_scan(sid, [], '')  # empty scan first must not break MIN/MAX (NULL handling)
    db.record_scan(sid, [{'gross_edge': -.01, 'net_edge': -.045, 'pair_cost': 1.01, 'qualified': False}], '')
    db.record_scan(sid, [{'gross_edge': .03, 'net_edge': .012, 'pair_cost': .97, 'qualified': True}], 'boom')
    with db.connect() as c:
        r = dict(c.execute('SELECT * FROM scan_stats').fetchone())
    assert r['scans'] == 3 and r['qualified_scans'] == 1 and r['errors'] == 1
    assert round(r['best_net'], 3) == .012 and round(r['min_pair'], 2) == .97
    d = db.report_data('2000-01-01T00:00:00Z')
    assert d['total']['scans'] == 3 and len(d['hours']) == 1
