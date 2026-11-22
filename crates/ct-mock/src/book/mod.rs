//! The mock's state: users, connections, accounts, orders and groups. The engine that moves orders through their
//! life is in [`engine`]; how the state is shown as API objects is in [`render`].
//!
//! Prices, quantities and cash are `Decimal`, as they are strings on the wire: no float arithmetic anywhere.
//! Time is real (a ticker every few milliseconds), so a test waits for a state rather than stepping a clock.

mod engine;
mod render;

use crate::brokers::{profile, Profile};
use connecttrade::models::{
    ConnectionStatus, ConnectionType, GroupRole, GroupStatus, Instrument, NormalizedStatus, OrderBroker, OrderClass,
    OrderLegRequest, OrderTif, OrderType, Side, TransactionType,
};
use connecttrade::stream::ServerMessage;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Cash in a new account.
pub const START_CASH: Decimal = Decimal::from_parts(100_000, 0, 0, false, 0);
/// The price of a symbol nobody has set.
pub const DEFAULT_PRICE: Decimal = Decimal::ONE_HUNDRED;
/// Per-share fee the mock charges brokers without a modelled schedule, so fee rows exist (`TradeZero`'s is
/// [`crate::fees`]).
pub const FEE_PER_SHARE: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

/// Which session the mock's market is in (set through `/mock/session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Pre,
    Regular,
    Post,
}

/// A platform user.
#[derive(Debug, Clone)]
pub struct User {
    pub user_id: String,
    pub secret: String,
    pub created_at: String,
}

/// A user's link to one broker.
#[derive(Debug, Clone)]
pub struct Connection {
    pub connection_id: String,
    pub user_id: String,
    pub broker: OrderBroker,
    pub connection_type: ConnectionType,
    pub status: ConnectionStatus,
    pub created_at: String,
    pub updated_at: String,
    pub accounts: Vec<String>,
}

/// One brokerage account.
#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: String,
    pub connection_id: String,
    pub user_id: String,
    pub account_number: String,
    pub broker: OrderBroker,
    pub created_at: String,
    pub cash: Decimal,
}

/// An OAuth link handed out by `create_connection` or `repair_connection`, completed by visiting it.
#[derive(Debug, Clone)]
pub struct PendingLink {
    pub user_id: String,
    pub broker: OrderBroker,
    pub connection_type: ConnectionType,
    /// Set for a repair.
    pub repair_of: Option<String>,
    pub redirect_url: Option<String>,
}

/// One execution.
#[derive(Debug, Clone)]
pub struct FillRec {
    pub fill_id: String,
    pub time: String,
    pub qty: Decimal,
    pub price: Decimal,
}

/// An order and where it is in its life.
#[derive(Debug, Clone)]
pub struct Order {
    pub order_id: String,
    pub cl_order_id: String,
    pub user_id: String,
    pub connection_id: String,
    pub account_id: String,
    pub account_number: String,
    pub broker: OrderBroker,
    pub side: Side,
    pub qty: Decimal,
    pub order_type: OrderType,
    pub tif: OrderTif,
    pub limit: Option<Decimal>,
    pub stop: Option<Decimal>,
    pub symbol: Option<String>,
    pub instrument: Instrument,
    pub legs: Option<Vec<OrderLegRequest>>,
    pub status: NormalizedStatus,
    pub broker_status: String,
    pub broker_text: Option<String>,
    pub entry_time: String,
    pub last_modified: String,
    pub last_exec: Option<String>,
    pub fills: Vec<FillRec>,
    pub class: OrderClass,
    pub group_id: Option<String>,
    pub role: Option<GroupRole>,
    pub parent: Option<String>,
    /// Waiting for its entry to fill.
    pub contingent: bool,
    /// When a pending order is acknowledged.
    pub ack_at: Instant,
    /// Reject on acknowledgement, with this broker text.
    pub reject: Option<String>,
    pub triggered: bool,
    pub cancel_at: Option<Instant>,
    /// For a cancel-then-place replace: the order that replaces this one, and when the cancel lands.
    pub replaced_by: Option<(String, Instant)>,
}

impl Order {
    /// Quantity filled so far.
    #[must_use]
    pub fn filled_qty(&self) -> Decimal {
        self.fills.iter().map(|f| f.qty).sum()
    }

    /// Still able to fill or change.
    #[must_use]
    pub fn open(&self) -> bool {
        matches!(
            self.status,
            NormalizedStatus::Pending
                | NormalizedStatus::Working
                | NormalizedStatus::PartiallyFilled
                | NormalizedStatus::PendingCancel
                | NormalizedStatus::PendingReplace
        )
    }

    fn buys(&self) -> bool {
        matches!(self.side, Side::Buy | Side::BuyToOpen | Side::BuyToClose | Side::BuyToCover)
    }
}

/// A bracket, OTO or OCO group.
#[derive(Debug, Clone)]
pub struct Group {
    pub group_id: String,
    pub class: OrderClass,
    pub account_id: String,
    pub user_id: String,
    pub connection_id: String,
    pub cl_order_id: String,
    pub broker: OrderBroker,
    pub status: GroupStatus,
    pub members: Vec<String>,
    pub created: String,
    pub modified: String,
}

/// A holding: signed quantity and average price.
#[derive(Debug, Clone, Default)]
pub struct Position {
    pub qty: Decimal,
    pub avg: Decimal,
}

/// A transaction row (fills and fees).
#[derive(Debug, Clone)]
pub struct Txn {
    pub id: String,
    pub account_id: String,
    pub kind: TransactionType,
    pub symbol: Option<String>,
    pub qty: Option<Decimal>,
    pub price: Option<Decimal>,
    pub amount: Decimal,
    pub time: String,
    pub order_id: Option<String>,
    pub side: Option<String>,
}

/// Mock timings.
#[derive(Debug, Clone)]
pub struct Timings {
    /// Pending to working.
    pub ack: Duration,
    /// Between the two parts of a partial fill.
    pub partial_gap: Duration,
    /// Pending cancel to cancelled.
    pub cancel: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            ack: Duration::from_millis(20),
            partial_gap: Duration::from_millis(40),
            cancel: Duration::from_millis(20),
        }
    }
}

/// Everything the mock knows.
#[derive(Debug)]
pub struct Book {
    pub users: BTreeMap<String, User>,
    pub connections: BTreeMap<String, Connection>,
    pub accounts: BTreeMap<String, Account>,
    pub links: HashMap<String, PendingLink>,
    pub orders: BTreeMap<String, Order>,
    pub order_seq: Vec<String>,
    pub groups: BTreeMap<String, Group>,
    pub cl_ids: HashMap<(String, String), String>,
    pub positions: BTreeMap<(String, String), Position>,
    pub txns: Vec<Txn>,
    pub prices: HashMap<String, Decimal>,
    pub session: Session,
    /// Access tokens: the user they act for (none for a platform token) and when they expire.
    pub tokens: HashMap<String, (Option<String>, Instant)>,
    pub rate: HashMap<String, VecDeque<Instant>>,
    /// The window [`Profile::trading_calls_per_minute`] counts in.
    pub rate_window: Duration,
    /// Orders fill only on `/mock/trade` ([`crate::MockConfig::tape`]).
    pub tape: bool,
    /// Symbols that cannot be borrowed for a short sale.
    pub hard_to_borrow: HashSet<String>,
    /// Sells are refused beyond the shares not already in working sells ([`crate::MockConfig::reserve_shares`]).
    pub reserve_shares: bool,
    pub timings: Timings,
    /// Updates for the stream: (user, message).
    pub outbox: Vec<(String, ServerMessage)>,
    next_partial: HashMap<String, Instant>,
}

/// Now, as the API writes timestamps.
#[must_use]
pub fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// A fresh identifier.
#[must_use]
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A decimal as the API writes it: no trailing zeros.
#[must_use]
pub fn num(v: Decimal) -> String {
    v.normalize().to_string()
}

impl Book {
    /// An empty book.
    #[must_use]
    pub fn new(timings: Timings) -> Self {
        Book {
            users: BTreeMap::new(),
            connections: BTreeMap::new(),
            accounts: BTreeMap::new(),
            links: HashMap::new(),
            orders: BTreeMap::new(),
            order_seq: vec![],
            groups: BTreeMap::new(),
            cl_ids: HashMap::new(),
            positions: BTreeMap::new(),
            txns: vec![],
            prices: HashMap::new(),
            session: Session::Regular,
            tokens: HashMap::new(),
            rate: HashMap::new(),
            rate_window: Duration::from_secs(60),
            tape: false,
            hard_to_borrow: HashSet::new(),
            reserve_shares: false,
            timings,
            outbox: vec![],
            next_partial: HashMap::new(),
        }
    }

    /// The market a [`crate::MockConfig`] asks for: tape or continuous, what cannot be borrowed, the rate window.
    #[must_use]
    pub fn with_market(mut self, config: &crate::MockConfig) -> Self {
        self.tape = config.tape;
        self.hard_to_borrow = config.hard_to_borrow.iter().cloned().collect();
        self.rate_window = config.rate_window;
        self.reserve_shares = config.reserve_shares;
        self
    }

    /// The current price of `symbol`.
    #[must_use]
    pub fn price(&self, symbol: &str) -> Decimal {
        self.prices.get(symbol).copied().unwrap_or(DEFAULT_PRICE)
    }

    /// Complete an OAuth link: a new connection with one account, or a repaired one.
    pub fn complete_link(&mut self, link_id: &str) -> Option<(Connection, Option<String>)> {
        let link = self.links.remove(link_id)?;
        let now = now_str();
        if let Some(cid) = &link.repair_of {
            let c = self.connections.get_mut(cid)?;
            c.status = ConnectionStatus::Active;
            c.updated_at = now;
            return Some((c.clone(), link.redirect_url));
        }
        let p = profile(&link.broker);
        let connection_id = id();
        let account_id = id();
        let n = self.accounts.len() + 100_001;
        let account = Account {
            account_id: account_id.clone(),
            connection_id: connection_id.clone(),
            user_id: link.user_id.clone(),
            account_number: format!("{}{n:06}", p.account_prefix),
            broker: link.broker.clone(),
            created_at: now.clone(),
            cash: START_CASH,
        };
        self.accounts.insert(account_id.clone(), account);
        let c = Connection {
            connection_id: connection_id.clone(),
            user_id: link.user_id,
            broker: link.broker,
            connection_type: link.connection_type,
            status: ConnectionStatus::Active,
            created_at: now.clone(),
            updated_at: now,
            accounts: vec![account_id],
        };
        self.connections.insert(connection_id, c.clone());
        Some((c, link.redirect_url))
    }

    /// True when another trading call on this account would exceed the broker's limit; records the call if not.
    pub fn rate_limited(&mut self, account_id: &str, p: &Profile) -> bool {
        let Some(limit) = p.trading_calls_per_minute else { return false };
        let now = Instant::now();
        let window = self.rate_window;
        let q = self.rate.entry(account_id.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > window) {
            q.pop_front();
        }
        if q.len() >= limit {
            return true;
        }
        q.push_back(now);
        false
    }
}
