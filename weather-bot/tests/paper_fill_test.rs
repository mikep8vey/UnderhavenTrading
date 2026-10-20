use rust_decimal_macros::dec;
use std::collections::HashSet;
use weather_bot::execution::order_builder::{OrderBuilder, OrderType};
use weather_bot::execution::paper_broker::maker_fill_from_prints;
use weather_bot::polymarket::trades::{parse_prints, Print};

fn print(asset: &str, side: &str, price: rust_decimal::Decimal, size: rust_decimal::Decimal, ts: i64, tx: &str) -> Print {
    Print {
        condition_id: "0xcond".to_string(),
        asset: asset.to_string(),
        side: side.to_string(),
        price,
        size,
        timestamp: ts,
        tx: tx.to_string(),
        wallet: "0xwallet".to_string(),
    }
}

#[test]
fn parses_the_data_api_trade_shape() {
    let body = serde_json::json!([{
        "conditionId": "0xcond",
        "asset": "yes-token",
        "side": "SELL",
        "price": 0.45,
        "size": 12.5,
        "timestamp": 1715101105,
        "transactionHash": "0xabc",
        "proxyWallet": "0xwallet"
    }]);
    let prints = parse_prints(&body);
    assert_eq!(prints.len(), 1);
    assert_eq!(prints[0].condition_id, "0xcond");
    assert_eq!(prints[0].side, "SELL");
    assert_eq!(prints[0].price, dec!(0.45));
    assert_eq!(prints[0].size, dec!(12.5));
    assert_eq!(prints[0].timestamp, 1_715_101_105);
    assert_eq!(prints[0].tx, "0xabc");
}

#[test]
fn a_sell_print_at_or_below_the_bid_fills_a_resting_buy() {
    let req = OrderBuilder::new(false).maker("yes-token", "0xcond", dec!(0.45), dec!(10), false, dec!(0.01));
    assert_eq!(req.order_type, OrderType::Gtc);
    let mut seen = HashSet::new();
    let prints = vec![
        // taker sold at our bid: this is the fill
        print("yes-token", "SELL", dec!(0.45), dec!(4), 1_000, "0xa"),
        // taker sold through our bid: we still fill at our limit, capped by the print
        print("yes-token", "SELL", dec!(0.44), dec!(3), 1_001, "0xb"),
        // a buy print is someone taking the ask; it does not fill our bid
        print("yes-token", "BUY", dec!(0.44), dec!(100), 1_002, "0xc"),
        // above our bid: no fill
        print("yes-token", "SELL", dec!(0.46), dec!(100), 1_003, "0xd"),
        // different outcome token: ignore
        print("no-token", "SELL", dec!(0.40), dec!(100), 1_004, "0xe"),
        // before placement: ignore
        print("yes-token", "SELL", dec!(0.40), dec!(100), 900, "0xf"),
    ];
    assert_eq!(maker_fill_from_prints(&req, 1_000, dec!(10), &prints, &mut seen), dec!(7));
}

#[test]
fn prints_are_counted_once_and_capped_by_remaining_size() {
    let req = OrderBuilder::new(false).maker("yes-token", "0xcond", dec!(0.45), dec!(10), false, dec!(0.01));
    let mut seen = HashSet::new();
    let prints = vec![
        print("yes-token", "SELL", dec!(0.45), dec!(8), 1_000, "0xa"),
        // duplicate of the same print (the tape is polled repeatedly)
        print("yes-token", "SELL", dec!(0.45), dec!(8), 1_000, "0xa"),
        // only 2 shares remain, even though this print is 8
        print("yes-token", "SELL", dec!(0.45), dec!(8), 1_001, "0xb"),
    ];
    assert_eq!(maker_fill_from_prints(&req, 1_000, dec!(10), &prints, &mut seen), dec!(10));
    // a second poll with the same tape must not fill again
    assert_eq!(maker_fill_from_prints(&req, 1_000, dec!(0), &prints, &mut seen), dec!(0));
}

#[test]
fn prints_before_placement_are_ignored() {
    let req = OrderBuilder::new(false).maker("yes-token", "0xcond", dec!(0.45), dec!(10), false, dec!(0.01));
    let mut seen = HashSet::new();
    let prints = vec![
        print("yes-token", "SELL", dec!(0.45), dec!(2), 999, "0xa"),
        print("yes-token", "SELL", dec!(0.45), dec!(2), 998, "0xb"),
        print("yes-token", "SELL", dec!(0.45), dec!(2), 1_000, "0xc"),
    ];
    assert_eq!(maker_fill_from_prints(&req, 1_000, dec!(10), &prints, &mut seen), dec!(2));
}
