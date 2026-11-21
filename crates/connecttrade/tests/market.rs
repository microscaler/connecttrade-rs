//! The mock as a market for a conformance suite: a tape that trades once at a time and in a given size, sessions
//! and the end of a day, a stock that cannot be borrowed, and a trading-call window shorter than a minute.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "integration tests: a failing test should fail loudly; they run on plain OS threads"
)]

mod common;

use common::*;
use connecttrade::models::*;
use connecttrade::ops;
use ct_mock::MockConfig;
use std::time::Duration;

fn tape() -> World {
    world_with(MockConfig {
        tape: true,
        hard_to_borrow: vec!["HTB".into()],
        rate_window: Duration::from_millis(300),
        ..MockConfig::default()
    })
}

fn trade(env: &World, symbol: &str, price: f64, size: Option<u32>) {
    post_json(
        &format!("{}/mock/trade", env.mock.url()),
        &serde_json::json!({ "symbol": symbol, "price": price, "size": size }),
    );
}

fn session(env: &World, session: &str, new_day: bool) {
    post_json(
        &format!("{}/mock/session", env.mock.url()),
        &serde_json::json!({ "session": session, "new_day": new_day }),
    );
}

fn settle() {
    std::thread::sleep(Duration::from_millis(60));
}

fn place(env: &World, t: &Trader, cl: &str, side: Side, qty: &str, limit: &str, tif: OrderTif) -> OrderCreateResponse {
    let mut o = order(t, cl, side, qty, OrderType::Lmt, Some(limit), None);
    o.order_tif = tif;
    env.client.send(&t.creds, ops::create_order(&o)).unwrap()
}

#[test]
fn on_a_tape_only_a_trade_fills_and_only_once() {
    let env = tape();
    let t = env.trader(OrderBroker::Tastytrade);
    let early = place(&env, &t, "t-1", Side::Buy, "10", "100", OrderTif::Day);
    settle();
    trade(&env, "AAPL", 100.0, None);
    let o = env.wait_status(&t, &early.order_id, NormalizedStatus::Filled);
    // in two parts, both at this trade
    assert_eq!(o.fills.unwrap().len(), 2);
    // placed after the trade at its price: waits for the next trade
    let late = place(&env, &t, "t-2", Side::Buy, "10", "100", OrderTif::Day);
    settle();
    assert_eq!(env.order(&t, &late.order_id).normalized_status, Some(NormalizedStatus::Working));
    trade(&env, "AAPL", 100.5, None);
    settle();
    assert_eq!(env.order(&t, &late.order_id).normalized_status, Some(NormalizedStatus::Working), "above the limit");
}

#[test]
fn a_trade_fills_at_most_its_size() {
    let env = tape();
    let t = env.trader(OrderBroker::Tradezero);
    let o = place(&env, &t, "s-1", Side::Buy, "10", "50", OrderTif::Day);
    settle();
    trade(&env, "AAPL", 50.0, Some(4));
    let p = env.wait_status(&t, &o.order_id, NormalizedStatus::PartiallyFilled);
    let filled: u32 = p.fills.unwrap().iter().map(|f| f.quantity.as_ref().unwrap().parse::<u32>().unwrap()).sum();
    assert_eq!(filled, 4);
    trade(&env, "AAPL", 50.0, Some(6));
    env.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
}

#[test]
fn a_new_day_ends_day_orders_and_after_hours_only_extended_ones_trade() {
    let env = tape();
    let t = env.trader(OrderBroker::Tradezero);
    let day = place(&env, &t, "d-1", Side::Buy, "1", "50", OrderTif::Day);
    let gtc = place(&env, &t, "d-2", Side::Buy, "1", "50", OrderTif::Gtc);
    settle();
    session(&env, "regular", true);
    env.wait_status(&t, &day.order_id, NormalizedStatus::Expired);
    assert_eq!(env.order(&t, &gtc.order_id).normalized_status, Some(NormalizedStatus::Working));
    session(&env, "post", false);
    let ext = place(&env, &t, "d-3", Side::Buy, "1", "50", OrderTif::DayPlus);
    settle();
    trade(&env, "AAPL", 49.0, None);
    env.wait_status(&t, &ext.order_id, NormalizedStatus::Filled);
    assert_eq!(env.order(&t, &gtc.order_id).normalized_status, Some(NormalizedStatus::Working), "GTC, after hours");
    session(&env, "regular", false);
    trade(&env, "AAPL", 49.0, None);
    env.wait_status(&t, &gtc.order_id, NormalizedStatus::Filled);
}

#[test]
fn a_stock_that_cannot_be_borrowed_is_not_sold_short() {
    let env = tape();
    let t = env.trader(OrderBroker::Tradezero);
    let mut short = order(&t, "h-1", Side::SellShort, "1", OrderType::Lmt, Some("10"), None);
    short.symbol = Some("HTB".into());
    let e = env.client.send(&t.creds, ops::create_order(&short)).unwrap_err();
    assert_eq!(e.api().unwrap().status, 400, "{e}");
    let mut buy = order(&t, "h-2", Side::Buy, "1", OrderType::Lmt, Some("10"), None);
    buy.symbol = Some("HTB".into());
    env.client.send(&t.creds, ops::create_order(&buy)).unwrap();
}

#[test]
fn the_trading_call_window_is_the_configured_one() {
    let env = tape();
    let t = env.trader(OrderBroker::Tradier);
    for i in 0..60 {
        place(&env, &t, &format!("r-{i}"), Side::Buy, "1", "1", OrderTif::Day);
    }
    let mut o = order(&t, "r-60", Side::Buy, "1", OrderType::Lmt, Some("1"), None);
    assert!(env.client.send(&t.creds, ops::create_order(&o)).unwrap_err().api().unwrap().is_rate_limited());
    std::thread::sleep(Duration::from_millis(350));
    o.cl_order_id = "r-61".into();
    env.client.send(&t.creds, ops::create_order(&o)).unwrap();
}
