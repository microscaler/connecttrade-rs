//! `ct-mock`: a stateful mock of the Connect Trade API, REST and order stream, for exercising a client before it
//! is pointed at Connect Trade.
//!
//! It is a model of Connect Trade as documented, with each broker's documented rules (see [`brokers`]) and our
//! assumptions where the documents are silent, each marked `ASSUMPTION` there. When a paper account disagrees, the
//! mock is corrected.
//!
//! What it does:
//! - users, connections (the OAuth link is a URL on the mock; fetching it completes the login), accounts,
//!   balances, positions, transactions (fills and fees, except for brokers without transactions);
//! - orders through their life: pending → working → filled (in two parts on brokers that partially fill), stops
//!   and limits filling when `/mock/prices` crosses them; brackets, OTO and OCO groups with their contingent exits;
//!   replace (in place, or cancel then place with a gap on TradeZero) and cancel, with the group-leg rules;
//! - the error rules: `UNSUPPORTED_ORDER_CLASS`, `GROUP_LEG_NOT_REPLACEABLE`, `GROUP_LEG_NOT_CANCELLABLE`, `409` on
//!   a broken connection or a duplicate `cl_order_id`, `422` on bad query parameters, 429 at a broker's call limit;
//! - the stream at `/stream`: welcome, authenticate, subscribe, subscribe_user, ping, and the updates;
//! - faults per request: `X-Auth-Failure`, `X-Service-Unavailable`, `X-Rate-Limit` (as PriceWhisperer's
//!   pact-mock-server), `X-Mock-Delay-Before-Ms` and `X-Mock-Delay-After-Ms` (a timeout before or after the change).
//!
//! Controls: `POST /mock/prices`, `POST /mock/session`, `POST /mock/connections/{id}/break`, `POST /mock/reset`.

// Handlers return `Result<Response, Response>`, the usual axum shape; boxing every early return buys nothing in a
// test server.
#![allow(clippy::result_large_err)]

pub mod book;
pub mod brokers;
pub mod rest;
pub mod stream;

use axum::routing::{get, post};
use axum::Router;
use book::{Book, Timings};
use connecttrade::stream::ServerMessage;
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::broadcast;

/// Credentials the mock accepts, and its timings.
#[derive(Debug, Clone)]
pub struct MockConfig {
    pub client_id: String,
    pub client_secret: String,
    pub timings: Timings,
    /// How long a stream session has to authenticate (30 seconds at Connect Trade).
    pub auth_timeout: Duration,
    /// How often the order engine runs.
    pub tick: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            client_id: "mock-client-id".into(),
            client_secret: "mock-client-secret".into(),
            timings: Timings::default(),
            auth_timeout: Duration::from_secs(30),
            tick: Duration::from_millis(5),
        }
    }
}

pub struct AppState {
    pub book: Mutex<Book>,
    pub config: MockConfig,
    pub events: broadcast::Sender<(String, ServerMessage)>,
    pub platform_sessions: AtomicUsize,
    base_url: OnceLock<String>,
}

impl AppState {
    pub fn base_url(&self) -> &str {
        self.base_url.get().map(String::as_str).unwrap_or("http://localhost")
    }

    /// Publish what the last change queued for the stream.
    pub fn flush(&self, book: &mut Book) {
        for (user, msg) in book.outbox.drain(..) {
            let _ = self.events.send((user, msg));
        }
    }
}

pub fn state(config: MockConfig) -> Arc<AppState> {
    let (events, _) = broadcast::channel(4096);
    Arc::new(AppState {
        book: Mutex::new(Book::new(config.timings.clone())),
        config,
        events,
        platform_sessions: AtomicUsize::new(0),
        base_url: OnceLock::new(),
    })
}

pub fn router(state: Arc<AppState>) -> Router {
    use rest::*;
    Router::new()
        .route("/auth/tokens/platform", post(create_platform_token))
        .route("/auth/tokens/user", post(create_user_token))
        .route("/users", get(list_users).post(create_user).delete(delete_user))
        .route("/users/{user_id}/rotate-secret", post(rotate_user_secret))
        .route("/connections", get(list_connections).post(create_connection))
        .route("/connections/{connection_id}", get(get_connection).delete(delete_connection))
        .route("/connections/{connection_id}/repair", post(repair_connection))
        .route("/platform/connections", get(list_platform_connections))
        .route("/accounts", get(list_accounts))
        .route("/accounts/{account_id}", get(get_account))
        .route("/balances", get(list_balances))
        .route("/orders", get(list_orders).post(create_order))
        .route("/accounts/{account_id}/orders/{order_id}", get(get_order).delete(cancel_order).patch(replace_order))
        .route("/accounts/{account_id}/order-groups/{order_group_id}", get(get_order_group).delete(cancel_order_group))
        .route("/positions", get(list_positions))
        .route("/transactions", get(list_transactions))
        .route("/stream", get(stream::upgrade))
        .route("/mock/oauth/{link}", get(oauth))
        .route("/mock/prices", post(set_prices))
        .route("/mock/session", post(set_session))
        .route("/mock/connections/{connection_id}/break", post(break_connection))
        .route("/mock/reset", post(reset))
        .route("/health", get(health))
        .layer(axum::middleware::from_fn(faults))
        .with_state(state)
}

/// Serve on `addr` until the process ends: the order engine and the HTTP server.
pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    let _ = state.base_url.set(format!("http://{addr}"));
    let engine = state.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(engine.config.tick);
        loop {
            every.tick().await;
            let mut b = engine.book.lock().unwrap();
            b.tick();
            engine.flush(&mut b);
        }
    });
    axum::serve(listener, router(state)).await
}

/// A mock running on its own thread and runtime, for tests in blocking code.
pub struct MockServer {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
}

impl MockServer {
    /// Start on a free port. The server lives until the process ends.
    pub fn start(config: MockConfig) -> MockServer {
        let state = state(config);
        let (tx, rx) = std::sync::mpsc::channel();
        let s = state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                serve(listener, s).await.unwrap();
            });
        });
        let addr = rx.recv().unwrap();
        MockServer { addr, state }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn stream_url(&self) -> String {
        format!("ws://{}/stream", self.addr)
    }
}
