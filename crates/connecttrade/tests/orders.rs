//! The client against `ct-mock`. Orders: fills, replaces, broker rules, ids and limits.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "integration tests: a failing test should fail loudly; they run on plain OS threads, never on may workers, so they may sleep"
)]

mod common;

use common::*;
use connecttrade::models::*;
use connecttrade::{ops, ErrorCode};
use ct_mock::MockConfig;
use std::time::Duration;

#[test]
fn a_market_order_fills_in_two_parts_and_moves_the_position() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    env.price("AAPL", 190.0);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "mkt-1", Side::Buy, "10", OrderType::Mkt, None, None)))
        .unwrap();
    assert_eq!(o.normalized_status, Some(NormalizedStatus::Pending));
    let done = env.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    let fills = done.fills.unwrap();
    assert_eq!(fills.len(), 2, "Tradier's profile fills in two parts");
    assert_eq!(fills[0].quantity.as_deref(), Some("6"));
    let pos = env.client.send(&t.creds, ops::list_positions(&Default::default())).unwrap();
    assert_eq!(pos[0].quantity.as_deref(), Some("10"));
    // Tradier has transactions: a FILL and a FEE row per fill
    let tx = env.client.send(&t.creds, ops::list_transactions(&Default::default())).unwrap().data.unwrap();
    assert_eq!(tx.iter().filter(|r| r.transaction_type == Some(TransactionType::Fee)).count(), 2);
    assert_eq!(tx.iter().filter(|r| r.transaction_type == Some(TransactionType::Fill)).count(), 2);
}

#[test]
fn tradezero_has_no_transactions() {
    let env = world();
    let t = env.trader(OrderBroker::Tradezero);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "tz-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    env.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    let tx = env.client.send(&t.creds, ops::list_transactions(&Default::default())).unwrap();
    assert_eq!(
        tx.data.unwrap(),
        Vec::<TransactionResponse>::new(),
        "TradeZero has no transactions through Connect Trade"
    );
}

#[test]
fn a_limit_order_works_until_the_price_crosses() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    env.price("AAPL", 190.0);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "lmt-1", Side::Buy, "1", OrderType::Lmt, Some("185"), None)))
        .unwrap();
    env.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(env.order(&t, &o.order_id).normalized_status, Some(NormalizedStatus::Working));
    env.price("AAPL", 184.0);
    let f = env.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    assert_eq!(f.fills.unwrap()[0].price.as_deref(), Some("184"));
}

#[test]
fn a_simple_stop_is_moved_with_one_replace() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    env.price("AAPL", 190.0);
    let stop = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "stp-1", Side::Sell, "5", OrderType::Stp, None, Some("180"))))
        .unwrap();
    env.wait_status(&t, &stop.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "stp-1-r1");
    r.stop_price = Some("185".into());
    let moved = env.client.send(&t.creds, ops::replace_order(&t.account_id, &stop.order_id, &r)).unwrap();
    assert_ne!(moved.order_id, stop.order_id, "a replace answers with the new order");
    assert_eq!(env.order(&t, &stop.order_id).normalized_status, Some(NormalizedStatus::Replaced));
    env.wait_status(&t, &moved.order_id, NormalizedStatus::Working);
    // the moved stop triggers at its new price
    env.price("AAPL", 184.5);
    env.wait_status(&t, &moved.order_id, NormalizedStatus::Filled);
}

#[test]
fn tradier_cannot_change_quantity_in_place() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "q-1", Side::Buy, "5", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    env.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "q-1-r");
    r.order_qty = Some("3".into());
    let e = env.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::InvalidParameters));
    r.order_qty = None;
    r.order_limit_price = Some("2".into());
    env.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap();
}

#[test]
fn tradezero_cannot_modify_patch_is_501_and_the_order_stands() {
    // Connect Trade, 22 Nov 2026: where the broker cannot modify an order, PATCH is 501; the client cancels and
    // places itself (docs/connect-trade-answers.md)
    let env = world();
    let t = env.trader(OrderBroker::Tradezero);
    env.price("AAPL", 190.0);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "tzr-1", Side::Buy, "5", OrderType::Lmt, Some("180"), None)))
        .unwrap();
    env.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    let mut req = replace(&t, "tzr-1-r");
    req.order_limit_price = Some("181".into());
    let err = env.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &req)).unwrap_err();
    assert!(err.api().is_some_and(connecttrade::ApiError::is_not_supported), "{err}");
    assert!(!err.may_have_happened(), "a 501 changed nothing");
    assert_eq!(env.order(&t, &o.order_id).normalized_status, Some(NormalizedStatus::Working));
    // the client's own way: cancel, then a new order under a new client order id
    env.client.send(&t.creds, ops::cancel_order(&t.account_id, &o.order_id)).unwrap();
    env.wait_status(&t, &o.order_id, NormalizedStatus::Cancelled);
    let renewed = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "tzr-1-2", Side::Buy, "5", OrderType::Lmt, Some("181"), None)))
        .unwrap();
    env.wait_status(&t, &renewed.order_id, NormalizedStatus::Working);
}

#[test]
fn a_broker_that_reserves_shares_refuses_a_second_sell_for_them() {
    let env = world_with(MockConfig { reserve_shares: true, ..MockConfig::default() });
    let t = env.trader(OrderBroker::Tastytrade);
    env.price("AAPL", 190.0);
    let b = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "rs-buy", Side::Buy, "10", OrderType::Mkt, None, None)))
        .unwrap();
    env.wait_status(&t, &b.order_id, NormalizedStatus::Filled);
    env.client
        .send(&t.creds, ops::create_order(&order(&t, "rs-stop", Side::Sell, "10", OrderType::Stp, None, Some("180"))))
        .unwrap();
    let e = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "rs-tp", Side::Sell, "10", OrderType::Lmt, Some("200"), None)))
        .unwrap_err();
    assert!(e.to_string().contains("INSUFFICIENT_SHARES"), "{e}");
}

#[test]
fn tradezero_rejects_plain_stops_outside_regular_hours() {
    let env = world();
    let t = env.trader(OrderBroker::Tradezero);
    post_json(&format!("{}/mock/session", env.mock.url()), &serde_json::json!({ "session": "pre" }));
    let stop = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "pre-1", Side::Sell, "1", OrderType::Stp, None, Some("150"))))
        .unwrap();
    let r = env.wait_status(&t, &stop.order_id, NormalizedStatus::Rejected);
    assert!(r.broker_text.unwrap().starts_with("R79"));
    // a stop-limit is the way to protect a position pre-market
    let mut sl = order(&t, "pre-2", Side::Sell, "1", OrderType::Stplmt, Some("149"), Some("150"));
    sl.order_tif = OrderTif::DayPlus;
    let ok = env.client.send(&t.creds, ops::create_order(&sl)).unwrap();
    env.wait_status(&t, &ok.order_id, NormalizedStatus::Working);
}

#[test]
fn client_order_ids_are_unique_and_short() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    env.client
        .send(&t.creds, ops::create_order(&order(&t, "same", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    let e = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "same", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_conflict() && !e.api().unwrap().is_broken_connection());
    let long = "x".repeat(37);
    let e = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, &long, Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert_eq!(e.api().unwrap().status, 400);
}

#[test]
fn tradier_trading_calls_are_capped_at_sixty_a_minute() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    for i in 0..60 {
        env.client
            .send(
                &t.creds,
                ops::create_order(&order(&t, &format!("rl-{i}"), Side::Buy, "1", OrderType::Lmt, Some("1"), None)),
            )
            .unwrap();
    }
    let e = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "rl-60", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_rate_limited());
    // reads are not trading calls
    env.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap();
}

#[test]
fn list_orders_filters_by_status_and_validates_its_limit() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    let a = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "f-1", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    let b = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "f-2", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    env.wait_status(&t, &b.order_id, NormalizedStatus::Filled);
    let open = env
        .client
        .send(
            &t.creds,
            ops::list_orders(&ops::ListOrdersQuery { status: Some(OrderStatusFilter::Open), ..Default::default() }),
        )
        .unwrap();
    let ids: Vec<_> = open.data.unwrap().into_iter().map(|o| o.order_id).collect();
    assert_eq!(ids, vec![a.order_id]);
    let e = env
        .client
        .send(&t.creds, ops::list_orders(&ops::ListOrdersQuery { limit: Some(501), ..Default::default() }))
        .unwrap_err();
    assert_eq!(e.api().unwrap().status, 400, "the spec's status for GET /orders; the general docs say 422");
}
