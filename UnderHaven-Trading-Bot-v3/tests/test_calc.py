"""Shared financial engine: fees, executable prices, position accounting, the corrected exit example."""
from decimal import Decimal as D
from app.btc.calc import (FeeModel, Position, check_displayed_card, exit_card, kalshi_fee, money, pair_edge, poly_fee, sell_net, walk, buy_all_in, fee_for_fill)


def test_kalshi_fee_matches_published_examples_and_rounds_up_per_order():
    assert kalshi_fee(100, '0.50') == D('1.75') and kalshi_fee(100, '0.10') == D('0.63')
    assert kalshi_fee(1, '0.50') == D('0.02')                 # 0.0175 rounds UP to the next cent
    assert kalshi_fee(100, '0.50', multiplier=0) == D('0')    # series with a zero multiplier
    assert kalshi_fee(100, '0.50', maker=True) == D('0') and kalshi_fee(100, '0.50', maker=True, maker_fees=True) == D('0.44')


def test_polymarket_fee_matches_docs_and_makers_pay_nothing():
    assert poly_fee(100, '0.50', '0.07') == D('1.75000')
    assert poly_fee(100, '0.05', '0.07') == D('0.33250') and poly_fee(100, '0.99', '0.07') == D('0.06930')
    assert poly_fee(100, '0.50', '0.07', maker=True) == 0 and poly_fee(100, '0.50', 0) == 0
    assert poly_fee(100, '0.50', '0.07', exponent=2) < poly_fee(100, '0.50', '0.07')     # feeSchedule.exponent is honoured


def test_walk_weighted_average_partial_depth_limit_lot_and_haircut():
    asks = [(D('0.50'), D('10')), (D('0.52'), D('20')), (D('0.60'), D('50'))]
    f = walk(asks, 25)
    assert f.qty == 25 and f.cost == D('0.50') * 10 + D('0.52') * 15 and f.vwap == f.cost / 25 and f.worst == D('0.52') and f.complete
    g = walk(asks, 100, limit='0.52')                         # a price cap stops the walk: only 30 contracts are available at or below it
    assert g.qty == 30 and not g.complete and g.worst == D('0.52')
    assert walk(asks, 25, lot=1).qty == 25 and walk([(D('0.5'), D('2.7'))], 3, lot=1).qty == 2
    h = walk(asks, 20, haircut=D('0.5'))                      # only half of each level is assumed fillable
    assert h.qty == 20 and h.levels[0][1] == D('5')
    bids = [(D('0.40'), D('10')), (D('0.30'), D('10'))]
    s = walk(bids, 15, side='sell'); assert s.cost == D('0.40') * 10 + D('0.30') * 5 and s.worst == D('0.30')


def test_position_average_cost_realized_unrealized_and_settlement():
    p = Position(); p.buy(10, '0.40', '0.17'); p.buy(10, '0.60', '0.17')
    assert p.qty == 20 and p.paid == D('10.34') and p.avg_cost == D('0.517') and p.avg_price == D('0.50')
    pnl = p.sell(10, '0.70', '0.10')
    assert pnl == D('7.00') - D('0.10') - D('5.17') and p.qty == 10 and p.paid == D('5.17')
    fm = FeeModel('kalshi')
    fill = walk([(D('0.55'), D('20'))], 10, side='sell')
    assert p.unrealized(fill, fm) == sell_net(fill, fm) - D('5.17')
    assert p.unrealized(walk([(D('0.55'), D('5'))], 10, side='sell'), fm) is None       # book cannot absorb it: no pretend value
    try: p.sell(11, '0.5'); raise AssertionError('oversell accepted')
    except ValueError: pass
    realized_before = p.realized
    p.settle(1); assert p.qty == 0 and p.realized == realized_before + (D('10') - D('5.17'))
    q = Position(); q.buy(5, '0.50', '0.10'); q.settle(0); assert q.realized == -D('2.60')


def test_complementary_pair_example_from_the_brief():
    # YES ask .86 on one venue + NO ask .08 on the other = .94 -> $0.06 gross per matched pair, before costs
    fm0 = FeeModel('polymarket', rate=D('0'))
    pe = pair_edge([(D('0.86'), D('500'))], [(D('0.08'), D('500'))], 100, fm0, fm0)
    assert pe.gross == D('6.00') and pe.net == D('6.00') and pe.complete and pe.ret == D('6.00') / D('94.00')
    wrong = pair_edge([(D('0.14'), D('500'))], [(D('0.92'), D('500'))], 100, fm0, fm0)       # the other direction costs $1.06
    assert wrong.gross == D('-6.00')
    withfees = pair_edge([(D('0.86'), D('500'))], [(D('0.08'), D('500'))], 100, FeeModel('polymarket', rate=D('0.07')), FeeModel('kalshi'),
                         slippage_per_unit=D('0.005'), safety_per_unit=D('0.01'))
    assert withfees.net < pe.net and withfees.fees == poly_fee(100, '0.86', '0.07') + kalshi_fee(100, '0.08')


def test_thin_depth_limits_the_matched_quantity_and_never_assumes_unlimited_liquidity():
    fm0 = FeeModel('polymarket', rate=D('0'))
    pe = pair_edge([(D('0.86'), D('30'))], [(D('0.08'), D('500'))], 100, fm0, fm0)
    assert pe.qty == 30 and not pe.complete                    # only 30 matched pairs exist at these prices


def test_corrected_exit_card_regression():
    """Polymarket NO sells at 97.0c, Kalshi YES sells at 2.0c, 103 contracts each."""
    q = D(103)
    exit_fee = kalshi_fee(q, '0.02'); assert exit_fee == D('0.15')               # the brief's "estimated Kalshi fees $0.15" is exactly this formula
    card = exit_card(q, [dict(venue='polymarket', side='NO', entry_avg='0.949', entry_fee=0, exit_price='0.97', exit_fee=0),
                         dict(venue='kalshi', side='YES', entry_avg='0.020', entry_fee=exit_fee, exit_price='0.02', exit_fee=exit_fee)])
    assert card['combined_sell'] == D('0.99') and card['proceeds'] == D('101.97')
    assert card['combined_entry'] == D('0.969') and card['cost_basis'] == D('99.807')
    # the stated figures conflict: cost 99.85 vs paid 99.99 vs a P&L that only subtracts the exit fee
    issues = check_displayed_card(card, displayed_cost='99.85', displayed_paid='99.99', displayed_pnl='1.97')
    assert any('displayed P&L' in i for i in issues)
    # reconcile from the fills: cost basis 99.85 (exact fill prices) + entry fees 0.14 = 99.99 paid in total
    exact = exit_card(q, [dict(venue='polymarket', side='NO', entry_avg=D('99.85') / q - D('0.02'), entry_fee=0, exit_price='0.97', exit_fee=0),
                          dict(venue='kalshi', side='YES', entry_avg='0.02', entry_fee='0.14', exit_price='0.02', exit_fee='0.15')])
    assert money(exact['cost_basis']) == D('99.85') and money(exact['paid']) == D('99.99')
    assert money(exact['net_pnl']) == D('1.83') == money(D('101.97') - D('0.15') - D('99.99'))
    assert money(exact['return_pct'] / 1) == D('1.83')
    assert not check_displayed_card(exact, displayed_cost='99.85', displayed_paid='99.99', displayed_pnl='1.83')
    # net P&L is always proceeds - exit fees - ALL-IN paid (cost basis + entry fees)
    assert exact['net_pnl'] == exact['proceeds'] - exact['exit_fees'] - exact['cost_basis'] - exact['entry_fees']
