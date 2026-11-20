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
//!   replace (in place, or cancel then place with a gap on `TradeZero`) and cancel, with the group-leg rules;
//! - the error rules: `UNSUPPORTED_ORDER_CLASS`, `GROUP_LEG_NOT_REPLACEABLE`, `GROUP_LEG_NOT_CANCELLABLE`, `409` on
//!   a broken connection or a duplicate `cl_order_id`, `422` on bad query parameters, 429 at a broker's call limit;
//! - the stream at `/stream`: welcome, authenticate, subscribe, `subscribe_user`, ping, and the updates;
//! - faults per request: `X-Auth-Failure`, `X-Service-Unavailable`, `X-Rate-Limit` (as `PriceWhisperer`'s
//!   pact-mock-server), `X-Mock-Delay-Before-Ms` and `X-Mock-Delay-After-Ms` (a timeout before or after the change).
//!
//! Controls: `POST /mock/prices`, `POST /mock/session`, `POST /mock/connections/{id}/break`, `POST /mock/reset`.

#![allow(
    clippy::result_large_err,
    reason = "handlers return Result<Response, Response>, the usual axum shape; boxing every early return buys nothing"
)]

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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
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

/// What every handler shares.
#[derive(Debug)]
pub struct AppState {
    pub book: Mutex<Book>,
    pub config: MockConfig,
    pub events: broadcast::Sender<(String, ServerMessage)>,
    pub platform_sessions: AtomicUsize,
    base_url: OnceLock<String>,
}

impl AppState {
    /// The book, locked. A handler that panicked while holding it leaves it usable: the mock keeps serving and the
    /// failing test reports the panic, instead of every later request failing on a poisoned lock.
    pub fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The address the mock serves on, for the OAuth links it hands out.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base_url.get().map_or("http://localhost", String::as_str)
    }

    /// Publish what the last change queued for the stream.
    pub fn flush(&self, book: &mut Book) {
        for (user, msg) in book.outbox.drain(..) {
            // an error only means no stream session is listening
            let _no_listener = self.events.send((user, msg)).is_err();
        }
    }
}

/// Fresh state for a mock.
#[must_use]
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

/// Every route: the spec's, `/stream`, and the `/mock/*` controls.
pub fn router(state: Arc<AppState>) -> Router {
    use rest::{account, controls, faults, health, orders, platform};
    Router::new()
        .route("/auth/tokens/platform", post(platform::create_platform_token))
        .route("/auth/tokens/user", post(platform::create_user_token))
        .route("/users", get(platform::list_users).post(platform::create_user).delete(platform::delete_user))
        .route("/users/{user_id}/rotate-secret", post(platform::rotate_user_secret))
        .route("/connections", get(platform::list_connections).post(platform::create_connection))
        .route("/connections/{connection_id}", get(platform::get_connection).delete(platform::delete_connection))
        .route("/connections/{connection_id}/repair", post(platform::repair_connection))
        .route("/platform/connections", get(platform::list_platform_connections))
        .route("/accounts", get(account::list_accounts))
        .route("/accounts/{account_id}", get(account::get_account))
        .route("/balances", get(account::list_balances))
        .route("/orders", get(orders::list_orders).post(orders::create_order))
        .route(
            "/accounts/{account_id}/orders/{order_id}",
            get(orders::get_order).delete(orders::cancel_order).patch(orders::replace_order),
        )
        .route(
            "/accounts/{account_id}/order-groups/{order_group_id}",
            get(orders::get_order_group).delete(orders::cancel_order_group),
        )
        .route("/positions", get(account::list_positions))
        .route("/transactions", get(account::list_transactions))
        .route("/stream", get(stream::upgrade))
        .route("/mock/oauth/{link}", get(platform::oauth))
        .route("/mock/prices", post(controls::set_prices))
        .route("/mock/session", post(controls::set_session))
        .route("/mock/connections/{connection_id}/break", post(controls::break_connection))
        .route("/mock/reset", post(controls::reset))
        .route("/health", get(health))
        .layer(axum::middleware::from_fn(faults))
        .with_state(state)
}

/// Serve on `listener` until the process ends: the order engine and the HTTP server.
///
/// # Errors
///
/// When the listener fails.
pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let addr = listener.local_addr()?;
    // set once per state; a second serve of the same state keeps the first address
    let _already_set = state.base_url.set(format!("http://{addr}")).is_err();
    let engine = Arc::clone(&state);
    tokio::spawn(async move {
        let mut every = tokio::time::interval(engine.config.tick);
        loop {
            every.tick().await;
            let mut b = engine.book();
            b.tick();
            engine.flush(&mut b);
        }
    });
    axum::serve(listener, router(state)).await
}

/// A mock running on its own thread and runtime, for tests in blocking code.
#[derive(Debug)]
pub struct MockServer {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
}

impl MockServer {
    /// Start on a free local port. The server lives until the process ends.
    ///
    /// # Errors
    ///
    /// When the runtime cannot start or no port can be bound.
    #[allow(
        clippy::disallowed_types,
        reason = "waits once, on the test's own thread, for the server thread to report its address"
    )]
    pub fn start(config: MockConfig) -> std::io::Result<MockServer> {
        let state = state(config);
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<SocketAddr>>();
        let s = Arc::clone(&state);
        std::thread::Builder::new().name("ct-mock".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    let _receiver_gone = tx.send(Err(e)).is_err();
                    return;
                }
            };
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
                    Ok(l) => l,
                    Err(e) => {
                        let _receiver_gone = tx.send(Err(e)).is_err();
                        return;
                    }
                };
                let _receiver_gone = tx.send(listener.local_addr()).is_err();
                if let Err(e) = serve(listener, s).await {
                    tracing::error!(error = %e, "ct-mock stopped");
                }
            });
        })?;
        let addr = rx.recv().map_err(std::io::Error::other)??;
        Ok(MockServer { addr, state })
    }

    /// The REST base URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// The stream URL.
    #[must_use]
    pub fn stream_url(&self) -> String {
        format!("ws://{}/stream", self.addr)
    }
}
