//! The client against `ct-mock`. Transport faults: what may have happened, and what did.
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
use connecttrade::{ops, Client, Credentials, Error, TransportError, UreqTransport};
use std::time::Duration;

#[test]
fn a_timeout_after_the_change_means_look_it_up_not_retry() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    let slow = Client::new(
        env.mock.url(),
        Faulty {
            inner: UreqTransport::new(Duration::from_secs(1), Duration::from_millis(300)),
            headers: vec![("x-mock-delay-after-ms", "1000".into())],
        },
    );
    let e = slow
        .send(&t.creds, ops::create_order(&order(&t, "lost-1", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(matches!(e, Error::Transport(TransportError::Timeout(_))), "{e:?}");
    assert!(e.may_have_happened());
    // the order exists: found by its client order id
    let found = env.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap().data.unwrap();
    assert!(found.iter().any(|o| o.cl_order_id == "lost-1"));

    // a timeout before the change: nothing exists
    let slow = Client::new(
        env.mock.url(),
        Faulty {
            inner: UreqTransport::new(Duration::from_secs(1), Duration::from_millis(300)),
            headers: vec![("x-mock-delay-before-ms", "1000".into())],
        },
    );
    let e = slow
        .send(&t.creds, ops::create_order(&order(&t, "lost-2", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(e.may_have_happened(), "the client cannot tell the two apart; only the lookup can");
    std::thread::sleep(Duration::from_millis(100));
    let found = env.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap().data.unwrap();
    assert!(!found.iter().any(|o| o.cl_order_id == "lost-2"));
}

#[test]
fn a_connection_refused_is_not_sent() {
    let client =
        Client::new("http://127.0.0.1:9", UreqTransport::new(Duration::from_millis(300), Duration::from_secs(1)));
    let creds = Credentials::platform("a", "b");
    let e = client.send(&creds, ops::list_users(&Default::default())).unwrap_err();
    assert!(matches!(e, Error::Transport(TransportError::NotSent(_))), "{e:?}");
    assert!(!e.may_have_happened());
}

#[test]
fn injected_faults_surface_as_api_errors() {
    let env = world();
    for (h, status) in [("x-auth-failure", 401), ("x-service-unavailable", 503), ("x-rate-limit", 429)] {
        let c =
            Client::new(env.mock.url(), Faulty { inner: UreqTransport::default(), headers: vec![(h, "true".into())] });
        let e = c.send(&env.platform, ops::list_users(&Default::default())).unwrap_err();
        assert_eq!(e.api().unwrap().status, status, "{h}");
    }
}
