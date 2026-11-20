//! The client against `ct-mock`: every behaviour an order path depends on, end to end over HTTP and WebSocket.

use connecttrade::models::*;
use connecttrade::stream::{ServerMessage, StreamAuth, StreamConfig, Topic};
use connecttrade::{
    ops, Client, Credentials, Error, ErrorCode, HttpRequest, HttpResponse, Transport, TransportError, UreqTransport,
};
use ct_mock::{MockConfig, MockServer};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct World {
    mock: MockServer,
    client: Client<Arc<dyn Transport>>,
    platform: Credentials,
}

/// One connected user with one account at `broker`.
struct Trader {
    creds: Credentials,
    user_id: String,
    account_id: String,
    connection_id: String,
    broker: OrderBroker,
}

fn world() -> World {
    let mock = MockServer::start(MockConfig::default());
    let transport: Arc<dyn Transport> = Arc::new(UreqTransport::new(Duration::from_secs(2), Duration::from_secs(5)));
    let client = Client::new(mock.url(), transport);
    World { mock, client, platform: Credentials::platform("mock-client-id", "mock-client-secret") }
}

impl World {
    fn trader(&self, broker: OrderBroker) -> Trader {
        let user_id = format!("u-{}", uuid_like());
        let created = self
            .client
            .send(&self.platform, ops::create_user(&UserCreateRequest { user_id: user_id.clone() }))
            .unwrap();
        let creds = self.platform.for_user(&user_id, &created.user_secret).unwrap();
        let link = self
            .client
            .send(
                &creds,
                ops::create_connection(&ConnectionCreateRequest {
                    broker: Some(serde_json::from_value(serde_json::json!(broker.as_str())).unwrap()),
                    connection_type: Some(ConnectionType::Trade),
                    redirect_url: None,
                    redirect_on_error_url: None,
                }),
            )
            .unwrap();
        // the user logs in at the broker: on the mock, fetching the link completes it
        visit(&link.connection_request_url);
        let accounts = self.client.send(&creds, ops::list_accounts(&Default::default())).unwrap();
        assert_eq!(accounts.len(), 1);
        Trader {
            creds,
            user_id,
            account_id: accounts[0].account_id.clone(),
            connection_id: accounts[0].connection_id.clone(),
            broker,
        }
    }

    fn price(&self, symbol: &str, px: f64) {
        post_json(&format!("{}/mock/prices", self.mock.url()), &serde_json::json!({ symbol: px }));
    }

    fn order(&self, t: &Trader, id: &str) -> OrderResponse {
        self.client.send(&t.creds, ops::get_order(&t.account_id, id)).unwrap()
    }

    fn wait_status(&self, t: &Trader, id: &str, want: NormalizedStatus) -> OrderResponse {
        let start = Instant::now();
        loop {
            let o = self.order(t, id);
            if o.normalized_status.as_ref() == Some(&want) {
                return o;
            }
            assert!(start.elapsed() < Duration::from_secs(3), "{id}: still {:?}, wanted {want:?}", o.normalized_status);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

fn visit(url: &str) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let r = agent.get(url).call().unwrap();
    assert_eq!(r.status().as_u16(), 200, "oauth link {url}");
}

fn post_json(url: &str, v: &serde_json::Value) {
    let r = ureq::post(url).send_json(v).unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

fn order(
    t: &Trader,
    cl: &str,
    side: Side,
    qty: &str,
    ty: OrderType,
    limit: Option<&str>,
    stop: Option<&str>,
) -> OrderCreateRequest {
    OrderCreateRequest {
        account_id: t.account_id.clone(),
        broker: t.broker.clone(),
        cl_order_id: cl.into(),
        side: Some(side),
        order_qty: Some(qty.into()),
        order_type: Some(ty),
        order_tif: OrderTif::Day,
        order_limit_price: limit.map(String::from),
        stop_price: stop.map(String::from),
        symbol: Some("AAPL".into()),
        instrument: Some(Instrument::Stk),
        complex_order_parameters: None,
        exec_dest: None,
        legs: None,
        order_notional: None,
        price_effect: None,
    }
}

fn replace(t: &Trader, new_cl: &str) -> OrderReplaceRequest {
    OrderReplaceRequest {
        new_cl_order_id: new_cl.into(),
        broker: t.broker.clone(),
        order_qty: None,
        order_notional: None,
        order_limit_price: None,
        stop_price: None,
    }
}

// ---------------------------------------------------------------------------------------------------------------

#[test]
fn users_are_managed_with_platform_credentials() {
    let w = world();
    let made = w.client.send(&w.platform, ops::create_user(&UserCreateRequest { user_id: "alice".into() })).unwrap();
    assert_eq!(made.user_id, "alice");
    let dup = w.client.send(&w.platform, ops::create_user(&UserCreateRequest { user_id: "alice".into() })).unwrap_err();
    assert_eq!(dup.api().unwrap().status, 409);
    let rotated = w.client.send(&w.platform, ops::rotate_user_secret("alice")).unwrap();
    assert_ne!(rotated.user_secret, made.user_secret);
    let list = w.client.send(&w.platform, ops::list_users(&Default::default())).unwrap();
    assert!(list.data.unwrap().iter().any(|u| u.user_id.as_deref() == Some("alice")));
    // the old secret no longer works
    let stale = w.platform.for_user("alice", &made.user_secret).unwrap();
    let e = w.client.send(&stale, ops::list_accounts(&Default::default())).unwrap_err();
    assert_eq!(e.api().unwrap().status, 401);
    w.client.send(&w.platform, ops::delete_user(&ops::DeleteUserQuery { user_id: Some("alice".into()) })).unwrap();
}

#[test]
fn a_user_operation_with_platform_credentials_fails_before_sending() {
    let w = world();
    let e = w.client.send(&w.platform, ops::list_accounts(&Default::default())).unwrap_err();
    assert!(matches!(e, Error::Credentials { missing: "user credentials", .. }), "{e:?}");
}

#[test]
fn a_connection_gives_an_account_with_a_balance() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    let conns = w.client.send(&t.creds, ops::list_connections(&Default::default())).unwrap();
    assert_eq!(conns[0].status, Some(ConnectionStatus::Active));
    let bal = w.client.send(&t.creds, ops::list_balances(&Default::default())).unwrap();
    assert_eq!(bal[0].cash.as_deref(), Some("100000"));
    let all = w.client.send(&w.platform, ops::list_platform_connections(&Default::default())).unwrap();
    assert!(all.data.unwrap().iter().any(|c| c.connection_id == t.connection_id));
}

#[test]
fn a_market_order_fills_in_two_parts_and_moves_the_position() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    w.price("AAPL", 190.0);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "mkt-1", Side::Buy, "10", OrderType::Mkt, None, None)))
        .unwrap();
    assert_eq!(o.normalized_status, Some(NormalizedStatus::Pending));
    let done = w.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    let fills = done.fills.unwrap();
    assert_eq!(fills.len(), 2, "Tradier's profile fills in two parts");
    assert_eq!(fills[0].quantity.as_deref(), Some("6"));
    let pos = w.client.send(&t.creds, ops::list_positions(&Default::default())).unwrap();
    assert_eq!(pos[0].quantity.as_deref(), Some("10"));
    // Tradier has transactions: a FILL and a FEE row per fill
    let tx = w.client.send(&t.creds, ops::list_transactions(&Default::default())).unwrap().data.unwrap();
    assert_eq!(tx.iter().filter(|r| r.transaction_type == Some(TransactionType::Fee)).count(), 2);
    assert_eq!(tx.iter().filter(|r| r.transaction_type == Some(TransactionType::Fill)).count(), 2);
}

#[test]
fn tradezero_has_no_transactions() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "tz-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    w.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    let tx = w.client.send(&t.creds, ops::list_transactions(&Default::default())).unwrap();
    assert!(tx.data.unwrap().is_empty());
}

#[test]
fn a_limit_order_works_until_the_price_crosses() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    w.price("AAPL", 190.0);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "lmt-1", Side::Buy, "1", OrderType::Lmt, Some("185"), None)))
        .unwrap();
    w.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(w.order(&t, &o.order_id).normalized_status, Some(NormalizedStatus::Working));
    w.price("AAPL", 184.0);
    let f = w.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    assert_eq!(f.fills.unwrap()[0].price.as_deref(), Some("184"));
}

#[test]
fn a_simple_stop_is_moved_with_one_replace() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    w.price("AAPL", 190.0);
    let stop = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "stp-1", Side::Sell, "5", OrderType::Stp, None, Some("180"))))
        .unwrap();
    w.wait_status(&t, &stop.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "stp-1-r1");
    r.stop_price = Some("185".into());
    let moved = w.client.send(&t.creds, ops::replace_order(&t.account_id, &stop.order_id, &r)).unwrap();
    assert_ne!(moved.order_id, stop.order_id, "a replace answers with the new order");
    assert_eq!(w.order(&t, &stop.order_id).normalized_status, Some(NormalizedStatus::Replaced));
    w.wait_status(&t, &moved.order_id, NormalizedStatus::Working);
    // the moved stop triggers at its new price
    w.price("AAPL", 184.5);
    w.wait_status(&t, &moved.order_id, NormalizedStatus::Filled);
}

#[test]
fn tradier_cannot_change_quantity_in_place() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "q-1", Side::Buy, "5", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    w.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "q-1-r");
    r.order_qty = Some("3".into());
    let e = w.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::InvalidParameters));
    r.order_qty = None;
    r.order_limit_price = Some("2".into());
    w.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap();
}

#[test]
fn tradezero_replaces_by_cancel_then_place() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    w.price("AAPL", 190.0);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "tzr-1", Side::Buy, "5", OrderType::Lmt, Some("180"), None)))
        .unwrap();
    w.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "tzr-1-r");
    r.order_limit_price = Some("181".into());
    let n = w.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap();
    // during the gap the old order is still live
    assert_eq!(w.order(&t, &o.order_id).normalized_status, Some(NormalizedStatus::PendingReplace));
    w.wait_status(&t, &o.order_id, NormalizedStatus::Replaced);
    w.wait_status(&t, &n.order_id, NormalizedStatus::Working);
}

#[test]
fn tradezero_old_order_filling_in_the_gap_rejects_the_replacement() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    w.price("AAPL", 190.0);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "gap-1", Side::Buy, "1", OrderType::Lmt, Some("180"), None)))
        .unwrap();
    w.wait_status(&t, &o.order_id, NormalizedStatus::Working);
    let mut r = replace(&t, "gap-1-r");
    r.order_limit_price = Some("170".into());
    let n = w.client.send(&t.creds, ops::replace_order(&t.account_id, &o.order_id, &r)).unwrap();
    w.price("AAPL", 179.0); // the old order fills before its cancel lands
    w.wait_status(&t, &o.order_id, NormalizedStatus::Filled);
    let rejected = w.wait_status(&t, &n.order_id, NormalizedStatus::Rejected);
    assert!(rejected.broker_text.unwrap().contains("filled before"));
}

#[test]
fn tradezero_rejects_plain_stops_outside_regular_hours() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    post_json(&format!("{}/mock/session", w.mock.url()), &serde_json::json!({ "session": "pre" }));
    let stop = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "pre-1", Side::Sell, "1", OrderType::Stp, None, Some("150"))))
        .unwrap();
    let r = w.wait_status(&t, &stop.order_id, NormalizedStatus::Rejected);
    assert!(r.broker_text.unwrap().starts_with("R79"));
    // a stop-limit is the way to protect a position pre-market
    let mut sl = order(&t, "pre-2", Side::Sell, "1", OrderType::Stplmt, Some("149"), Some("150"));
    sl.order_tif = OrderTif::DayPlus;
    let ok = w.client.send(&t.creds, ops::create_order(&sl)).unwrap();
    w.wait_status(&t, &ok.order_id, NormalizedStatus::Working);
}

#[test]
fn a_bracket_releases_its_exits_and_its_legs_cannot_be_touched() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    w.price("AAPL", 190.0);
    let mut b = order(&t, "br-1", Side::Buy, "10", OrderType::Lmt, Some("190"), None);
    b.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Bracket,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "195".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "185".into(), order_limit_price: None }),
    });
    let made = w.client.send(&t.creds, ops::create_order(&b)).unwrap();
    let group = made.order_group.unwrap();
    let members = group.orders.unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(members[0].group_role, Some(GroupRole::Entry));
    let tp = members[1].order_id.clone().unwrap();
    let sl = members[2].order_id.clone().unwrap();

    w.wait_status(&t, &made.order_id, NormalizedStatus::Filled);
    w.wait_status(&t, &sl, NormalizedStatus::Working);
    let g = w.client.send(&t.creds, ops::get_order_group(&t.account_id, &group.order_group_id)).unwrap();
    assert_eq!(g.group_status, Some(GroupStatus::Active));

    // the documented limits: no replace and no cancel of a single leg
    let mut r = replace(&t, "br-1-move");
    r.stop_price = Some("188".into());
    let e = w.client.send(&t.creds, ops::replace_order(&t.account_id, &sl, &r)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::GroupLegNotReplaceable));
    let e = w.client.send(&t.creds, ops::cancel_order(&t.account_id, &sl)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::GroupLegNotCancellable));

    // the target fills, the stop is cancelled, the group completes
    w.price("AAPL", 196.0);
    w.wait_status(&t, &tp, NormalizedStatus::Filled);
    w.wait_status(&t, &sl, NormalizedStatus::Cancelled);
    let g = w.client.send(&t.creds, ops::get_order_group(&t.account_id, &group.order_group_id)).unwrap();
    assert_eq!(g.group_status, Some(GroupStatus::Completed));
}

#[test]
fn an_oco_is_cancelled_as_a_whole() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    w.price("AAPL", 190.0);
    let mut o = order(&t, "oco-1", Side::Sell, "10", OrderType::Lmt, None, None);
    o.order_type = None;
    o.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Oco,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "200".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "180".into(), order_limit_price: None }),
    });
    let made = w.client.send(&t.creds, ops::create_order(&o)).unwrap();
    let gid = made.order_group_id.clone().unwrap();
    w.wait_status(&t, &made.order_id, NormalizedStatus::Working);
    w.client.send(&t.creds, ops::cancel_order_group(&t.account_id, &gid)).unwrap();
    for m in w.client.send(&t.creds, ops::get_order_group(&t.account_id, &gid)).unwrap().orders.unwrap() {
        w.wait_status(&t, m.order_id.as_ref().unwrap(), NormalizedStatus::Cancelled);
    }
    // an OCO with an order type is refused before it reaches the broker
    let mut bad = o.clone();
    bad.cl_order_id = "oco-2".into();
    bad.order_type = Some(OrderType::Lmt);
    let e = w.client.send(&t.creds, ops::create_order(&bad)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::InvalidParameters));
}

#[test]
fn groups_are_refused_where_the_broker_has_none() {
    let w = world();
    let t = w.trader(OrderBroker::Tradezero);
    let mut o = order(&t, "tz-oco", Side::Sell, "1", OrderType::Lmt, None, None);
    o.order_type = None;
    o.complex_order_parameters = Some(ComplexOrderParameters {
        order_class: ComplexOrderClass::Oco,
        take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "200".into() }),
        stop_loss: Some(ComplexOrderStopLoss { stop_price: "180".into(), order_limit_price: None }),
    });
    let e = w.client.send(&t.creds, ops::create_order(&o)).unwrap_err();
    assert_eq!(e.code(), Some(&ErrorCode::UnsupportedOrderClass));
    assert!(!e.may_have_happened());
}

#[test]
fn a_broken_connection_refuses_trading_until_repaired() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    post_json(&format!("{}/mock/connections/{}/break", w.mock.url(), t.connection_id), &serde_json::json!({}));
    let e = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "br-c", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_broken_connection(), "{e:?}");
    let link =
        w.client.send(&t.creds, ops::repair_connection(&t.connection_id, &ConnectionRepairRequest::default())).unwrap();
    visit(&link.connection_request_url);
    let c = w.client.send(&t.creds, ops::get_connection(&t.connection_id)).unwrap();
    assert_eq!(c.status, Some(ConnectionStatus::Active));
    w.client.send(&t.creds, ops::create_order(&order(&t, "br-c", Side::Buy, "1", OrderType::Mkt, None, None))).unwrap();
}

#[test]
fn client_order_ids_are_unique_and_short() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    w.client
        .send(&t.creds, ops::create_order(&order(&t, "same", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    let e = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "same", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_conflict() && !e.api().unwrap().is_broken_connection());
    let long = "x".repeat(37);
    let e = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, &long, Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert_eq!(e.api().unwrap().status, 400);
}

#[test]
fn tradier_trading_calls_are_capped_at_sixty_a_minute() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    for i in 0..60 {
        w.client
            .send(
                &t.creds,
                ops::create_order(&order(&t, &format!("rl-{i}"), Side::Buy, "1", OrderType::Lmt, Some("1"), None)),
            )
            .unwrap();
    }
    let e = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "rl-60", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_rate_limited());
    // reads are not trading calls
    w.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap();
}

#[test]
fn list_orders_filters_by_status_and_validates_its_limit() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    let a = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "f-1", Side::Buy, "1", OrderType::Lmt, Some("1"), None)))
        .unwrap();
    let b = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "f-2", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    w.wait_status(&t, &b.order_id, NormalizedStatus::Filled);
    let open = w
        .client
        .send(
            &t.creds,
            ops::list_orders(&ops::ListOrdersQuery { status: Some(OrderStatusFilter::Open), ..Default::default() }),
        )
        .unwrap();
    let ids: Vec<_> = open.data.unwrap().into_iter().map(|o| o.order_id).collect();
    assert_eq!(ids, vec![a.order_id]);
    let e = w
        .client
        .send(&t.creds, ops::list_orders(&ops::ListOrdersQuery { limit: Some(501), ..Default::default() }))
        .unwrap_err();
    assert_eq!(e.api().unwrap().status, 400, "the spec's status for GET /orders; the general docs say 422");
}

/// A transport that adds the mock's fault headers, standing in for a network that misbehaves.
struct Faulty {
    inner: UreqTransport,
    headers: Vec<(&'static str, String)>,
}

impl Transport for Faulty {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut r = r.clone();
        r.headers.extend(self.headers.iter().cloned());
        self.inner.send(&r)
    }
}

#[test]
fn a_timeout_after_the_change_means_look_it_up_not_retry() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    let slow = Client::new(
        w.mock.url(),
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
    let found = w.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap().data.unwrap();
    assert!(found.iter().any(|o| o.cl_order_id == "lost-1"));

    // a timeout before the change: nothing exists
    let slow = Client::new(
        w.mock.url(),
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
    let found = w.client.send(&t.creds, ops::list_orders(&Default::default())).unwrap().data.unwrap();
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
    let w = world();
    for (h, status) in [("x-auth-failure", 401), ("x-service-unavailable", 503), ("x-rate-limit", 429)] {
        let c =
            Client::new(w.mock.url(), Faulty { inner: UreqTransport::default(), headers: vec![(h, "true".into())] });
        let e = c.send(&w.platform, ops::list_users(&Default::default())).unwrap_err();
        assert_eq!(e.api().unwrap().status, status, "{h}");
    }
}

#[test]
fn tokens_stand_in_for_credentials() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    let tok = w
        .client
        .send(
            &t.creds,
            ops::create_user_token(&UserAccessTokenRequest { scopes: vec![TokenScope::Read, TokenScope::Trade] }),
        )
        .unwrap();
    let bearer = Credentials::Bearer(tok.access_token);
    let accounts = w.client.send(&bearer, ops::list_accounts(&Default::default())).unwrap();
    assert_eq!(accounts[0].account_id, t.account_id);
}

// ---------- the stream ----------

fn platform_stream(w: &World, users: &[&str]) -> StreamConfig {
    let mut c = StreamConfig::new(
        w.mock.stream_url(),
        StreamAuth::Client { client_id: "mock-client-id".into(), client_secret: "mock-client-secret".into() },
    );
    c.users = users.iter().map(|s| s.to_string()).collect();
    c.handshake_timeout = Duration::from_secs(2);
    c
}

#[test]
fn the_stream_delivers_an_orders_life() {
    let w = world();
    let t = w.trader(OrderBroker::Tradier);
    let mut s = platform_stream(&w, &[&t.user_id]).connect().unwrap();
    assert!(s.session_id().is_some());
    w.price("AAPL", 190.0);
    let o = w
        .client
        .send(&t.creds, ops::create_order(&order(&t, "st-1", Side::Buy, "4", OrderType::Mkt, None, None)))
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3)));
    let mut seen = vec![];
    let mut position_seen = false;
    let mut balance_seen = false;
    while !seen.contains(&NormalizedStatus::Filled) {
        match s.next_message().unwrap() {
            ServerMessage::Orders { data } if data.order_id == o.order_id => seen.push(data.normalized_status.unwrap()),
            ServerMessage::Positions { data } => {
                position_seen |= data.iter().any(|p| p.symbol.as_deref() == Some("AAPL"))
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
    let w = world();
    let mine = w.trader(OrderBroker::Tradier);
    let other = w.trader(OrderBroker::Tradier);
    let mut cfg = platform_stream(&w, &[&mine.user_id]);
    cfg.topics = vec![Topic::Orders];
    let mut s = cfg.connect().unwrap();
    w.client
        .send(&other.creds, ops::create_order(&order(&other, "other-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    let o = w
        .client
        .send(&mine.creds, ops::create_order(&order(&mine, "mine-1", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2)));
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
    let w = world();
    let mut bad = platform_stream(&w, &[]);
    bad.auth = StreamAuth::Client { client_id: "mock-client-id".into(), client_secret: "wrong".into() };
    assert!(matches!(bad.connect(), Err(connecttrade::stream::StreamError::AuthRefused(_))));
    let good = platform_stream(&w, &[]);
    let sessions: Vec<_> = (0..10).map(|_| good.connect().unwrap()).collect();
    match good.connect() {
        Err(connecttrade::stream::StreamError::AuthRefused(m)) => assert!(m.contains("Concurrent session limit (10)")),
        other => panic!("{:?}", other.map(|_| ())),
    }
    drop(sessions);
}

#[test]
fn a_user_token_session_is_locked_to_its_user() {
    let w = world();
    let t = w.trader(OrderBroker::Tastytrade);
    let tok = w
        .client
        .send(&t.creds, ops::create_user_token(&UserAccessTokenRequest { scopes: vec![TokenScope::Read] }))
        .unwrap();
    let mut cfg = StreamConfig::new(w.mock.stream_url(), StreamAuth::Token(tok.access_token));
    cfg.handshake_timeout = Duration::from_secs(2);
    let mut s = cfg.connect().unwrap();
    assert_eq!(s.user_id(), Some(t.user_id.as_str()));
    s.subscribe_user("someone-else").unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2)));
    assert!(matches!(s.next_message().unwrap(), ServerMessage::SubscriptionFailed { .. }));
}
