//! The mock's state: users, connections, accounts, orders and groups, and the engine that moves orders through
//! their life as prices change. Time is real (a ticker every few milliseconds), so a test waits for a state rather
//! than stepping a clock.

use crate::brokers::{profile, Profile, ReplaceMode};
use connecttrade::models::*;
use connecttrade::stream::ServerMessage;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const START_CASH: f64 = 100_000.0;
pub const DEFAULT_PRICE: f64 = 100.0;
/// Per-share fee the mock charges, so fee rows exist.
pub const FEE_PER_SHARE: f64 = 0.005;

/// Which session the mock's market is in (set through `/mock/session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    Pre,
    Regular,
    Post,
}

#[derive(Debug, Clone)]
pub struct User {
    pub user_id: String,
    pub secret: String,
    pub created_at: String,
}

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

#[derive(Debug, Clone)]
pub struct Account {
    pub account_id: String,
    pub connection_id: String,
    pub user_id: String,
    pub account_number: String,
    pub broker: OrderBroker,
    pub created_at: String,
    pub cash: f64,
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

#[derive(Debug, Clone)]
pub struct FillRec {
    pub fill_id: String,
    pub time: String,
    pub qty: f64,
    pub price: f64,
}

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
    pub qty: f64,
    pub order_type: OrderType,
    pub tif: OrderTif,
    pub limit: Option<f64>,
    pub stop: Option<f64>,
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
    pub fn filled_qty(&self) -> f64 {
        self.fills.iter().map(|f| f.qty).sum()
    }

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

#[derive(Debug, Clone, Default)]
pub struct Position {
    pub qty: f64,
    pub avg: f64,
}

#[derive(Debug, Clone)]
pub struct Txn {
    pub id: String,
    pub account_id: String,
    pub kind: TransactionType,
    pub symbol: Option<String>,
    pub qty: Option<f64>,
    pub price: Option<f64>,
    pub amount: f64,
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
    pub prices: HashMap<String, f64>,
    pub session: Session,
    pub tokens: HashMap<String, (Option<String>, Instant)>,
    pub rate: HashMap<String, VecDeque<Instant>>,
    pub timings: Timings,
    /// Updates for the stream: (user, message).
    pub outbox: Vec<(String, ServerMessage)>,
    next_partial: HashMap<String, Instant>,
}

pub fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn num(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" {
        "0".into()
    } else {
        s.to_string()
    }
}

impl Book {
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
            timings,
            outbox: vec![],
            next_partial: HashMap::new(),
        }
    }

    pub fn price(&self, symbol: &str) -> f64 {
        *self.prices.get(symbol).unwrap_or(&DEFAULT_PRICE)
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
        let n = self.accounts.len() + 1;
        let account = Account {
            account_id: account_id.clone(),
            connection_id: connection_id.clone(),
            user_id: link.user_id.clone(),
            account_number: format!("{}{:06}", p.account_prefix, 100_000 + n),
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
        let q = self.rate.entry(account_id.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() as u32 >= limit {
            return true;
        }
        q.push_back(now);
        false
    }

    pub fn emit_order(&mut self, order_id: &str) {
        if let Some(o) = self.orders.get(order_id) {
            let msg = ServerMessage::Orders { data: Box::new(self.order_response(o)) };
            self.outbox.push((o.user_id.clone(), msg));
        }
    }

    pub fn order_response(&self, o: &Order) -> OrderResponse {
        OrderResponse {
            user_id: Some(o.user_id.clone()),
            connection_id: Some(o.connection_id.clone()),
            account_id: o.account_id.clone(),
            order_id: o.order_id.clone(),
            account_number: Some(o.account_number.clone()),
            cl_order_id: o.cl_order_id.clone(),
            platform: Some("mock".into()),
            broker: Some(o.broker.as_str().to_string()),
            normalized_status: Some(o.status.clone()),
            broker_status: Some(o.broker_status.clone()),
            side: Some(o.side.clone()),
            order_qty: Some(num(o.qty)),
            order_notional: None,
            order_type: Some(o.order_type.clone()),
            broker_order_type: Some(o.order_type.as_str().to_ascii_lowercase()),
            order_tif: Some(o.tif.clone()),
            order_limit_price: Some(o.limit.map(num).unwrap_or_else(|| "0".into())),
            price_effect: None,
            symbol: o.symbol.clone(),
            instrument: Some(o.instrument.clone()),
            exec_dest: None,
            entry_time: Some(o.entry_time.clone()),
            last_exec_time: o.last_exec.clone(),
            last_modified_time: Some(o.last_modified.clone()),
            stop_price: o.stop.map(num),
            broker_text: o.broker_text.clone(),
            fills: Some(
                o.fills
                    .iter()
                    .map(|f| Fill {
                        fill_id: Some(f.fill_id.clone()),
                        time: Some(f.time.clone()),
                        quantity: Some(num(f.qty)),
                        price: Some(num(f.price)),
                    })
                    .collect(),
            ),
            legs: None,
            order_class: Some(o.class.clone()),
            order_group_id: o.group_id.clone(),
            group_role: o.role.clone(),
            parent_order_id: o.parent.clone(),
            system_time: Some(now_str()),
        }
    }

    pub fn group_response(&self, g: &Group) -> OrderGroup {
        let account_number = self.accounts.get(&g.account_id).map(|a| a.account_number.clone());
        OrderGroup {
            user_id: Some(g.user_id.clone()),
            connection_id: Some(g.connection_id.clone()),
            account_id: g.account_id.clone(),
            account_number,
            platform: Some("mock".into()),
            broker: Some(g.broker.as_str().to_string()),
            order_group_id: g.group_id.clone(),
            order_class: Some(g.class.clone()),
            cl_order_id: Some(g.cl_order_id.clone()),
            group_status: Some(g.status.clone()),
            broker_data: None,
            created_time: Some(g.created.clone()),
            last_modified_time: Some(g.modified.clone()),
            orders: Some(
                g.members
                    .iter()
                    .filter_map(|m| self.orders.get(m))
                    .map(|o| OrderGroupMember {
                        group_role: o.role.clone(),
                        order_id: Some(o.order_id.clone()),
                        cl_order_id: Some(o.cl_order_id.clone()),
                        parent_order_id: o.parent.clone(),
                        side: Some(o.side.clone()),
                        order_qty: Some(num(o.qty)),
                        order_type: Some(o.order_type.clone()),
                        order_limit_price: o.limit.map(num),
                        stop_price: o.stop.map(num),
                        order_tif: Some(o.tif.clone()),
                        normalized_status: Some(o.status.clone()),
                        broker_status: Some(o.broker_status.clone()),
                    })
                    .collect(),
            ),
        }
    }

    /// One engine step: acknowledgements, cancels, replaces, triggers and fills.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let ids: Vec<String> = self.order_seq.clone();
        for oid in ids {
            let Some(o) = self.orders.get(&oid).cloned() else { continue };
            // cancel-then-place replace: when the cancel lands, settle the replacement, also when the old order
            // filled in the gap (it is then no longer open, so this comes before that check)
            if let Some((new_id, at)) = &o.replaced_by {
                if now >= *at || !o.open() {
                    self.finish_replace(&oid, new_id);
                    continue;
                }
                // until then the old order can still fill
            }
            if o.contingent || !o.open() {
                continue;
            }
            if o.status == NormalizedStatus::PendingCancel {
                if o.cancel_at.is_some_and(|t| now >= t) {
                    self.set_status(&oid, NormalizedStatus::Cancelled, "cancelled");
                    self.after_terminal(&oid);
                }
                continue;
            }
            if o.status == NormalizedStatus::Pending {
                if now < o.ack_at {
                    continue;
                }
                if let Some(text) = o.reject.clone() {
                    self.orders.get_mut(&oid).unwrap().broker_text = Some(text);
                    self.set_status(&oid, NormalizedStatus::Rejected, "rejected");
                    self.after_terminal(&oid);
                    continue;
                }
                self.set_status(&oid, NormalizedStatus::Working, "accepted");
            }
            self.try_fill(&oid, now);
        }
    }

    fn try_fill(&mut self, oid: &str, now: Instant) {
        let o = self.orders[oid].clone();
        if !matches!(
            o.status,
            NormalizedStatus::Working | NormalizedStatus::PartiallyFilled | NormalizedStatus::PendingReplace
        ) {
            return;
        }
        if self.next_partial.get(oid).is_some_and(|t| now < *t) {
            return;
        }
        let Some(symbol) = o.symbol.clone() else { return };
        let px = self.price(&symbol);
        let buy = o.buys();
        let mut triggered = o.triggered;
        if !triggered && matches!(o.order_type, OrderType::Stp | OrderType::Stplmt) {
            let stop = o.stop.unwrap_or(px);
            triggered = if buy { px >= stop } else { px <= stop };
            if triggered {
                self.orders.get_mut(oid).unwrap().triggered = true;
            } else {
                return;
            }
        }
        let marketable = match o.order_type {
            OrderType::Mkt | OrderType::Stp => true,
            OrderType::Lmt | OrderType::Stplmt => {
                let l = o.limit.unwrap_or(px);
                if buy {
                    px <= l
                } else {
                    px >= l
                }
            }
            OrderType::Unknown(_) => false,
        };
        if !marketable {
            return;
        }
        let p = profile(&o.broker);
        let remaining = o.qty - o.filled_qty();
        let part = if p.partial_fills && o.fills.is_empty() && remaining >= 2.0 {
            (remaining * 0.6).floor()
        } else {
            remaining
        };
        let price = match o.order_type {
            OrderType::Lmt | OrderType::Stplmt => {
                let l = o.limit.unwrap_or(px);
                if buy {
                    px.min(l)
                } else {
                    px.max(l)
                }
            }
            _ => px,
        };
        self.fill(oid, part, price, &p);
        if part < remaining {
            self.next_partial.insert(oid.to_string(), now + self.timings.partial_gap);
        }
    }

    fn fill(&mut self, oid: &str, qty: f64, price: f64, p: &Profile) {
        let t = now_str();
        let (account_id, symbol, buy, done, user_id) = {
            let o = self.orders.get_mut(oid).unwrap();
            o.fills.push(FillRec { fill_id: id(), time: t.clone(), qty, price });
            o.last_exec = Some(t.clone());
            o.last_modified = t.clone();
            let done = o.filled_qty() >= o.qty - 1e-9;
            o.status = if done { NormalizedStatus::Filled } else { NormalizedStatus::PartiallyFilled };
            o.broker_status = if done { "filled".into() } else { "partially_filled".into() };
            (o.account_id.clone(), o.symbol.clone().unwrap_or_default(), o.buys(), done, o.user_id.clone())
        };
        // position, cash, transactions
        let signed = if buy { qty } else { -qty };
        let pos = self.positions.entry((account_id.clone(), symbol.clone())).or_default();
        if pos.qty == 0.0 || (pos.qty > 0.0) == (signed > 0.0) {
            pos.avg = (pos.avg * pos.qty.abs() + price * qty) / (pos.qty.abs() + qty);
        }
        pos.qty += signed;
        if pos.qty.abs() < 1e-9 {
            pos.qty = 0.0;
            pos.avg = 0.0;
        }
        let fee = FEE_PER_SHARE * qty;
        if let Some(a) = self.accounts.get_mut(&account_id) {
            a.cash -= signed * price + fee;
        }
        if p.transactions {
            self.txns.push(Txn {
                id: id(),
                account_id: account_id.clone(),
                kind: TransactionType::Fill,
                symbol: Some(symbol.clone()),
                qty: Some(qty),
                price: Some(price),
                amount: -signed * price,
                time: t.clone(),
                order_id: Some(oid.to_string()),
                side: Some(if buy { "BUY".into() } else { "SELL".into() }),
            });
            self.txns.push(Txn {
                id: id(),
                account_id: account_id.clone(),
                kind: TransactionType::Fee,
                symbol: Some(symbol),
                qty: None,
                price: None,
                amount: -fee,
                time: t,
                order_id: Some(oid.to_string()),
                side: None,
            });
        }
        self.emit_order(oid);
        self.emit_account(&user_id, &account_id);
        if done {
            self.after_terminal(oid);
        }
    }

    pub fn emit_account(&mut self, user_id: &str, account_id: &str) {
        let positions = self.position_rows(Some(account_id));
        self.outbox.push((user_id.to_string(), ServerMessage::Positions { data: positions }));
        if let Some(b) = self.balance_row(account_id) {
            self.outbox.push((user_id.to_string(), ServerMessage::Balances { data: b }));
        }
    }

    pub fn position_rows(&self, account: Option<&str>) -> Vec<PositionResponse> {
        self.positions
            .iter()
            .filter(|((a, _), p)| account.is_none_or(|x| x == a) && p.qty != 0.0)
            .filter_map(|((a, sym), p)| {
                let acct = self.accounts.get(a)?;
                let last = self.price(sym);
                let upl = (last - p.avg) * p.qty;
                Some(PositionResponse {
                    connection_id: Some(acct.connection_id.clone()),
                    account_id: Some(a.clone()),
                    user_id: Some(acct.user_id.clone()),
                    account_number: Some(acct.account_number.clone()),
                    broker_position_id: None,
                    symbol: Some(sym.clone()),
                    quantity: Some(num(p.qty)),
                    last_price: Some(num(last)),
                    position_average_price: Some(num(p.avg)),
                    unrealized_pl: Some(num(upl)),
                    daily_pl: Some(num(upl)),
                    total_pl: Some(num(upl)),
                    instrument: Some(Instrument::Stk),
                    system_time: Some(now_str()),
                })
            })
            .collect()
    }

    pub fn balance_row(&self, account_id: &str) -> Option<BalanceResponse> {
        let a = self.accounts.get(account_id)?;
        let equity: f64 = a.cash
            + self
                .positions
                .iter()
                .filter(|((acc, _), _)| acc == account_id)
                .map(|((_, s), p)| p.qty * self.price(s))
                .sum::<f64>();
        Some(BalanceResponse {
            account_number: Some(a.account_number.clone()),
            connection_id: Some(a.connection_id.clone()),
            account_id: Some(a.account_id.clone()),
            user_id: Some(a.user_id.clone()),
            currency_code: Some("USD".into()),
            cash: Some(num(a.cash)),
            buying_power: Some(num(equity * 2.0)),
            initial_margin: Some("0".into()),
            maintenance_margin: Some("0".into()),
            reg_t_buying_power: Some(num(equity * 2.0)),
            system_time: Some(now_str()),
        })
    }

    pub fn set_status(&mut self, oid: &str, s: NormalizedStatus, broker_status: &str) {
        if let Some(o) = self.orders.get_mut(oid) {
            o.status = s;
            o.broker_status = broker_status.into();
            o.last_modified = now_str();
        }
        self.emit_order(oid);
    }

    /// After an order reaches a final state: release a bracket's exits, cancel the other side of an OCO, and keep
    /// the group's status.
    fn after_terminal(&mut self, oid: &str) {
        let o = self.orders[oid].clone();
        let Some(gid) = o.group_id.clone() else { return };
        let members = self.groups[&gid].members.clone();
        let mut group_status = None;
        match (&o.role, &o.status) {
            (Some(GroupRole::Entry), NormalizedStatus::Filled) => {
                let ack = Instant::now() + self.timings.ack;
                for m in &members {
                    if let Some(x) = self.orders.get_mut(m) {
                        if x.contingent {
                            x.contingent = false;
                            x.ack_at = ack;
                            x.broker_status = "pending".into();
                        }
                    }
                }
                group_status = Some(GroupStatus::Active);
            }
            (
                Some(GroupRole::Entry),
                NormalizedStatus::Cancelled | NormalizedStatus::Rejected | NormalizedStatus::Expired,
            ) => {
                for m in &members {
                    if m != oid && self.orders.get(m).is_some_and(|x| x.open()) {
                        self.set_status(m, NormalizedStatus::Cancelled, "cancelled");
                    }
                }
                group_status = Some(if o.status == NormalizedStatus::Rejected {
                    GroupStatus::Rejected
                } else {
                    GroupStatus::Cancelled
                });
            }
            (Some(GroupRole::TakeProfit | GroupRole::StopLoss), NormalizedStatus::Filled) => {
                for m in &members {
                    let sibling_exit = self.orders.get(m).is_some_and(|x| {
                        m != oid && x.open() && matches!(x.role, Some(GroupRole::TakeProfit | GroupRole::StopLoss))
                    });
                    if sibling_exit {
                        self.set_status(m, NormalizedStatus::Cancelled, "cancelled");
                    }
                }
                group_status = Some(GroupStatus::Completed);
            }
            _ => {}
        }
        if members.iter().all(|m| self.orders.get(m).is_some_and(|x| !x.open())) && group_status.is_none() {
            let any_fill = members.iter().any(|m| self.orders[m].status == NormalizedStatus::Filled);
            group_status = Some(if any_fill { GroupStatus::Completed } else { GroupStatus::Cancelled });
        }
        if let Some(s) = group_status {
            if let Some(g) = self.groups.get_mut(&gid) {
                g.status = s;
                g.modified = now_str();
            }
        }
    }

    fn finish_replace(&mut self, old: &str, new: &str) {
        let filled = self.orders.get(old).is_some_and(|o| o.status == NormalizedStatus::Filled);
        if let Some(o) = self.orders.get_mut(old) {
            o.replaced_by = None;
        }
        if filled {
            // the original filled during the gap: the replacement is not placed
            if let Some(n) = self.orders.get_mut(new) {
                n.reject = Some("Original order filled before the replacement could be placed".into());
                n.ack_at = Instant::now();
                n.contingent = false;
            }
            return;
        }
        let part_filled = self.orders.get(old).map(|o| o.filled_qty()).unwrap_or(0.0);
        self.set_status(old, NormalizedStatus::Replaced, "replaced");
        if let Some(n) = self.orders.get_mut(new) {
            // what filled on the original is not placed again
            n.qty -= part_filled;
            n.contingent = false;
            n.ack_at = Instant::now();
        }
    }

    pub fn start_replace(&mut self, old: &str, mut new: Order, mode: ReplaceMode) -> String {
        let new_id = new.order_id.clone();
        match mode {
            ReplaceMode::Native => {
                let part_filled = self.orders[old].filled_qty();
                new.qty -= part_filled;
                self.set_status(old, NormalizedStatus::Replaced, "replaced");
                self.insert_order(new);
            }
            ReplaceMode::CancelThenPlace { gap_ms } => {
                new.contingent = true;
                let at = Instant::now() + Duration::from_millis(gap_ms);
                if let Some(o) = self.orders.get_mut(old) {
                    o.replaced_by = Some((new_id.clone(), at));
                    o.status = NormalizedStatus::PendingReplace;
                    o.broker_status = "pending_cancel".into();
                }
                self.emit_order(old);
                self.insert_order(new);
            }
        }
        new_id
    }

    pub fn insert_order(&mut self, o: Order) {
        let oid = o.order_id.clone();
        self.cl_ids.insert((o.account_id.clone(), o.cl_order_id.clone()), oid.clone());
        self.order_seq.push(oid.clone());
        self.orders.insert(oid.clone(), o);
        self.emit_order(&oid);
    }
}
