//! Shared setup for the tests against `ct-mock`: a mock, a client, and a connected trader.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    reason = "test helpers: a failing setup should fail loudly; they run on plain OS threads, never on may workers, so they may sleep"
)]
#![allow(dead_code, reason = "each test file uses its own subset of these helpers")]

use connecttrade::models::*;
use connecttrade::stream::{StreamAuth, StreamConfig};
use connecttrade::{ops, Client, Credentials, HttpRequest, HttpResponse, Transport, TransportError, UreqTransport};
use ct_mock::{MockConfig, MockServer};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct World {
    pub mock: MockServer,
    pub client: Client<Arc<dyn Transport>>,
    pub platform: Credentials,
}

/// One connected user with one account at `broker`.
pub struct Trader {
    pub creds: Credentials,
    pub user_id: String,
    pub account_id: String,
    pub connection_id: String,
    pub broker: OrderBroker,
}

pub fn world() -> World {
    let mock = MockServer::start(MockConfig::default()).expect("the mock starts");
    let transport: Arc<dyn Transport> = Arc::new(UreqTransport::new(Duration::from_secs(2), Duration::from_secs(5)));
    let client = Client::new(mock.url(), transport);
    World { mock, client, platform: Credentials::platform("mock-client-id", "mock-client-secret") }
}

impl World {
    pub fn trader(&self, broker: OrderBroker) -> Trader {
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

    pub fn price(&self, symbol: &str, px: f64) {
        post_json(&format!("{}/mock/prices", self.mock.url()), &serde_json::json!({ symbol: px }));
    }

    pub fn order(&self, t: &Trader, id: &str) -> OrderResponse {
        self.client.send(&t.creds, ops::get_order(&t.account_id, id)).unwrap()
    }

    #[allow(clippy::needless_pass_by_value, reason = "reads as wait_status(.., NormalizedStatus::Filled)")]
    pub fn wait_status(&self, t: &Trader, id: &str, want: NormalizedStatus) -> OrderResponse {
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

pub fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))
}

pub fn visit(url: &str) {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let r = agent.get(url).call().unwrap();
    assert_eq!(r.status().as_u16(), 200, "oauth link {url}");
}

pub fn post_json(url: &str, v: &serde_json::Value) {
    let r = ureq::post(url).send_json(v).unwrap();
    assert_eq!(r.status().as_u16(), 200);
}

pub fn order(
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

pub fn replace(t: &Trader, new_cl: &str) -> OrderReplaceRequest {
    OrderReplaceRequest {
        new_cl_order_id: new_cl.into(),
        broker: t.broker.clone(),
        order_qty: None,
        order_notional: None,
        order_limit_price: None,
        stop_price: None,
    }
}

/// A transport that adds the mock's fault headers, standing in for a network that misbehaves.
pub struct Faulty {
    pub inner: UreqTransport,
    pub headers: Vec<(&'static str, String)>,
}

impl Transport for Faulty {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut r = r.clone();
        r.headers.extend(self.headers.iter().cloned());
        self.inner.send(&r)
    }
}

pub fn platform_stream(w: &World, users: &[&str]) -> StreamConfig {
    let mut c = StreamConfig::new(
        w.mock.stream_url(),
        StreamAuth::Client { client_id: "mock-client-id".into(), client_secret: "mock-client-secret".into() },
    );
    c.users = users.iter().map(ToString::to_string).collect();
    c.handshake_timeout = Duration::from_secs(2);
    c
}
