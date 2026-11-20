//! The client against `ct-mock`. Brackets, OTO and OCO groups.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "integration tests: a failing test should fail loudly"
)]

mod common;

use common::*;
use connecttrade::models::*;
use connecttrade::{ops, ErrorCode};

#[test]
fn a_bracket_releases_its_exits_and_its_legs_cannot_be_touched() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    env.price("AAPL", 190.0);
    let mut b = order(&t, "br-1", Side::Buy, "10", OrderType::Lmt, Some("190"), None);
    b.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Bracket,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "195".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "185".into(), order_limit_price: None }),
    });
    let made = env.client.send(&t.creds, ops::create_order(&b)).unwrap();
    let group = made.order_group.unwrap();
    let members = group.orders.unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(members[0].group_role, Some(GroupRole::Entry));
    let tp = members[1].order_id.clone().unwrap();
    let sl = members[2].order_id.clone().unwrap();

    env.wait_status(&t, &made.order_id, NormalizedStatus::Filled);
    env.wait_status(&t, &sl, NormalizedStatus::Working);
    let fetched = env.client.send(&t.creds, ops::get_order_group(&t.account_id, &group.order_group_id)).unwrap();
    assert_eq!(fetched.group_status, Some(GroupStatus::Active));

    // the documented limits: no replace and no cancel of a single leg
    let mut moved = replace(&t, "br-1-move");
    moved.stop_price = Some("188".into());
    let refused = env.client.send(&t.creds, ops::replace_order(&t.account_id, &sl, &moved)).unwrap_err();
    assert_eq!(refused.code(), Some(&ErrorCode::GroupLegNotReplaceable));
    let refused = env.client.send(&t.creds, ops::cancel_order(&t.account_id, &sl)).unwrap_err();
    assert_eq!(refused.code(), Some(&ErrorCode::GroupLegNotCancellable));

    // the target fills, the stop is cancelled, the group completes
    env.price("AAPL", 196.0);
    env.wait_status(&t, &tp, NormalizedStatus::Filled);
    env.wait_status(&t, &sl, NormalizedStatus::Cancelled);
    let fetched = env.client.send(&t.creds, ops::get_order_group(&t.account_id, &group.order_group_id)).unwrap();
    assert_eq!(fetched.group_status, Some(GroupStatus::Completed));
}

#[test]
fn an_oco_is_cancelled_as_a_whole() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    env.price("AAPL", 190.0);
    let mut o = order(&t, "oco-1", Side::Sell, "10", OrderType::Lmt, None, None);
    o.order_type = None;
    o.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Oco,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "200".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "180".into(), order_limit_price: None }),
    });
    let made = env.client.send(&t.creds, ops::create_order(&o)).unwrap();
    let gid = made.order_group_id.clone().unwrap();
    env.wait_status(&t, &made.order_id, NormalizedStatus::Working);
    env.client.send(&t.creds, ops::cancel_order_group(&t.account_id, &gid)).unwrap();
    for m in env.client.send(&t.creds, ops::get_order_group(&t.account_id, &gid)).unwrap().orders.unwrap() {
        env.wait_status(&t, m.order_id.as_ref().unwrap(), NormalizedStatus::Cancelled);
    }
    // an OCO with an order type is refused before it reaches the broker
    let mut bad = o;
    bad.cl_order_id = "oco-2".into();
    bad.order_type = Some(OrderType::Lmt);
    let e = env.client.send(&t.creds, ops::create_order(&bad)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::InvalidParameters));
}

#[test]
fn groups_are_refused_where_the_broker_has_none() {
    let env = world();
    let t = env.trader(OrderBroker::Tradezero);
    let mut o = order(&t, "tz-oco", Side::Sell, "1", OrderType::Lmt, None, None);
    o.order_type = None;
    o.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Oco,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "200".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "180".into(), order_limit_price: None }),
    });
    let e = env.client.send(&t.creds, ops::create_order(&o)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::UnsupportedOrderClass));
    assert!(!e.may_have_happened());
}
