//! The client against `ct-mock`. The order stream.
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
use connecttrade::ops;
use connecttrade::stream::{ServerMessage, StreamAuth, StreamConfig, Topic};
use std::time::{Duration, Instant};

#[test]
fn the_stream_delivers_an_orders_life() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    let mut s = platform_stream(&env, &[&t.user_id]).connect().unwrap();
    assert!(s.session_id().is_some());
    env.price("AAPL", 190.0);
    let o = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "st-1", Side::Buy, "4", OrderType::Mkt, None, None)))
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut seen = vec![];
    let mut position_seen = false;
    let mut balance_seen = false;
    while !seen.contains(&NormalizedStatus::Filled) {
        match s.next_message().unwrap() {
            ServerMessage::Orders { data } if data.order_id == o.order_id => seen.push(data.normalized_status.unwrap()),
            ServerMessage::Positions { data } => {
                position_seen |= data.iter().any(|p| p.symbol.as_deref() == Some("AAPL"));
            }
            ServerMessage::Balances { .. } => balance_seen = true,
            _ => {}
        }
    }
    assert_eq!(
        seen,
        vec![
            NormalizedStatus::Pending,
            NormalizedStatus::Working,
            NormalizedStatus::PartiallyFilled,
            NormalizedStatus::Filled
        ]
    );
    assert!(position_seen && balance_seen);
    let rtt = s.round_trip(7, Duration::from_secs(2), |_| {}).unwrap();
    assert!(rtt < Duration::from_secs(1));
}

#[test]
fn the_stream_only_sends_subscribed_users_and_topics() {
    let env = world();
    let mine = env.trader(OrderBroker::Tradier);
    let other = env.trader(OrderBroker::Tradier);
    let mut cfg = platform_stream(&env, &[&mine.user_id]);
    cfg.topics = vec![Topic::Orders];
    let mut s = cfg.connect().unwrap();
    env.client
        .send(&other.creds, ops::create_order(&order(&other, "other-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    let o = env
        .client
        .send(&mine.creds, ops::create_order(&order(&mine, "mine-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match s.next_message() {
            Ok(ServerMessage::Orders { data }) => {
                assert_eq!(data.user_id.as_deref(), Some(mine.user_id.as_str()));
                assert_eq!(data.order_id, o.order_id);
            }
            Ok(ServerMessage::Positions { .. } | ServerMessage::Balances { .. }) => {
                panic!("not subscribed to that topic")
            }
            Ok(_) => {}
            Err(connecttrade::stream::StreamError::Timeout(_)) => break,
            Err(e) => panic!("{e}"),
        }
    }
}

#[test]
fn the_stream_refuses_bad_credentials_and_too_many_sessions() {
    let env = world();
    let mut bad = platform_stream(&env, &[]);
    bad.auth = StreamAuth::Client { client_id: "mock-client-id".into(), client_secret: "wrong".into() };
    assert!(matches!(bad.connect(), Err(connecttrade::stream::StreamError::AuthRefused(_))));
    let good = platform_stream(&env, &[]);
    let sessions: Vec<_> = (0..10).map(|_| good.connect().unwrap()).collect();
    match good.connect() {
        Err(connecttrade::stream::StreamError::AuthRefused(m)) => assert!(m.contains("Concurrent session limit (10)")),
        other => panic!("{:?}", other.map(|_| ())),
    }
    drop(sessions);
}

#[test]
fn a_user_token_session_is_locked_to_its_user() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    let tok = env
        .client
        .send(&t.creds, ops::create_user_token(&UserAccessTokenRequest { scopes: vec![TokenScope::Read] }))
        .unwrap();
    let mut cfg = StreamConfig::new(env.mock.stream_url(), StreamAuth::Token(tok.access_token));
    cfg.handshake_timeout = Duration::from_secs(2);
    let mut s = cfg.connect().unwrap();
    assert_eq!(s.user_id(), Some(t.user_id.as_str()));
    s.subscribe_user("someone-else").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert!(matches!(s.next_message().unwrap(), ServerMessage::SubscriptionFailed { .. }));
}
