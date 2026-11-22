//! Orders and order groups: the checks Connect Trade makes before a broker sees an order, then the book.

use super::checks::{check_replace, check_shape, connection_broken, session_rejection, BROKEN};
use super::{bad, body, decimal, err, not_found, ok, user, Q, R};
use crate::book::{id, now_str, num, Account, Book, Connection, Group, Order};
use crate::brokers::{profile, Profile, ReplaceMode};
use crate::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use connecttrade::models::{
    ComplexOrderClass, ComplexOrderParameters, ConnectionStatus, GroupRole, GroupStatus, Instrument, NormalizedStatus,
    OrderClass, OrderCreateRequest, OrderReplaceRequest, OrderType, Side,
};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Instant;

fn reverse(side: &Side) -> Side {
    match side {
        Side::Buy | Side::BuyToOpen | Side::BuyToCover | Side::BuyToClose => Side::Sell,
        _ => Side::Buy,
    }
}

/// The account and its connection, refused when the connection is broken or the broker does not match.
fn trading_account(b: &Book, uid: &str, req: &OrderCreateRequest) -> Result<(Account, Connection), Response> {
    let account = b
        .accounts
        .get(&req.account_id)
        .filter(|a| a.user_id == uid)
        .cloned()
        .ok_or_else(|| bad(format!("INVALID_PARAMETERS: account {} not found", req.account_id)))?;
    let conn = b.connections.get(&account.connection_id).cloned().ok_or_else(|| err(StatusCode::CONFLICT, BROKEN))?;
    if conn.status == ConnectionStatus::Broken {
        return Err(err(StatusCode::CONFLICT, BROKEN));
    }
    if account.broker != req.broker {
        return Err(bad(format!("INVALID_PARAMETERS: account {} is not a {} account", req.account_id, req.broker)));
    }
    if b.cl_ids.contains_key(&(account.account_id.clone(), req.cl_order_id.clone())) {
        return Err(err(StatusCode::CONFLICT, format!("Duplicate cl_order_id {}", req.cl_order_id)));
    }
    Ok((account, conn))
}

fn template(b: &Book, req: &OrderCreateRequest, account: &Account, p: &Profile) -> Result<Order, Response> {
    let now = now_str();
    Ok(Order {
        order_id: id(),
        cl_order_id: req.cl_order_id.clone(),
        user_id: account.user_id.clone(),
        connection_id: account.connection_id.clone(),
        account_id: account.account_id.clone(),
        account_number: account.account_number.clone(),
        broker: req.broker.clone(),
        side: req.side.clone().unwrap_or(Side::Buy),
        qty: decimal("order_qty", req.order_qty.as_ref())?.unwrap_or(Decimal::ONE),
        order_type: req.order_type.clone().unwrap_or(OrderType::Lmt),
        tif: req.order_tif.clone(),
        limit: decimal("order_limit_price", req.order_limit_price.as_ref())?,
        stop: decimal("stop_price", req.stop_price.as_ref())?,
        symbol: req.symbol.clone(),
        instrument: req.instrument.clone().unwrap_or(Instrument::Stk),
        legs: req.legs.clone(),
        status: NormalizedStatus::Pending,
        broker_status: "pending_new".into(),
        broker_text: None,
        entry_time: now.clone(),
        last_modified: now,
        last_exec: None,
        fills: vec![],
        class: OrderClass::Simple,
        group_id: None,
        role: None,
        parent: None,
        contingent: false,
        ack_at: Instant::now() + b.timings.ack,
        reject: session_rejection(p, b.session, req.order_type.as_ref()),
        triggered: false,
        cancel_at: None,
        replaced_by: None,
    })
}

/// An exit of a group: on the reverse side of an entry, or on the order's own side for an OCO.
fn exit(t: &Order, cl: String, role: GroupRole, entry: Option<&String>) -> Order {
    let mut x = t.clone();
    x.order_id = id();
    x.cl_order_id = cl;
    x.side = if entry.is_some() { reverse(&t.side) } else { t.side.clone() };
    x.role = Some(role);
    x.parent = entry.cloned();
    x.contingent = entry.is_some();
    x.reject = None;
    if x.contingent {
        x.broker_status = "contingent".into();
    }
    x
}

/// The members of a complex order, entry first.
fn members(t: &Order, c: &ComplexOrderParameters, class: &OrderClass, gid: &str) -> Result<Vec<Order>, Response> {
    let mut members = vec![];
    let mut base = t.clone();
    base.class = class.clone();
    base.group_id = Some(gid.to_string());
    let entry = if c.order_class == ComplexOrderClass::Oco {
        None
    } else {
        let mut e = base.clone();
        e.cl_order_id = format!("{}:EN", t.cl_order_id);
        e.role = Some(GroupRole::Entry);
        let eid = e.order_id.clone();
        members.push(e);
        Some(eid)
    };
    if let Some(tp) = &c.take_profit {
        let mut x = exit(&base, format!("{}:TP", t.cl_order_id), GroupRole::TakeProfit, entry.as_ref());
        x.order_type = OrderType::Lmt;
        x.limit = decimal("take_profit.order_limit_price", Some(&tp.order_limit_price))?;
        x.stop = None;
        members.push(x);
    }
    if let Some(sl) = &c.stop_loss {
        let mut x = exit(&base, format!("{}:SL", t.cl_order_id), GroupRole::StopLoss, entry.as_ref());
        x.stop = decimal("stop_loss.stop_price", Some(&sl.stop_price))?;
        x.limit = decimal("stop_loss.order_limit_price", sl.order_limit_price.as_ref())?;
        x.order_type = if x.limit.is_some() { OrderType::Stplmt } else { OrderType::Stp };
        members.push(x);
    }
    Ok(members)
}

fn place_group(s: &AppState, b: &mut Book, t: &Order, c: &ComplexOrderParameters) -> R {
    let gid = id();
    let class: OrderClass = serde_json::from_value(Value::String(c.order_class.as_str().into()))
        .unwrap_or_else(|_| OrderClass::Unknown(c.order_class.as_str().into()));
    let members = members(t, c, &class, &gid)?;
    let first = members.first().map(|m| m.order_id.clone()).ok_or_else(|| bad("INVALID_PARAMETERS: empty group"))?;
    let has_entry = c.order_class != ComplexOrderClass::Oco;
    let now = now_str();
    b.groups.insert(
        gid.clone(),
        Group {
            group_id: gid.clone(),
            class,
            account_id: t.account_id.clone(),
            user_id: t.user_id.clone(),
            connection_id: t.connection_id.clone(),
            cl_order_id: t.cl_order_id.clone(),
            broker: t.broker.clone(),
            status: if has_entry { GroupStatus::PendingEntry } else { GroupStatus::Active },
            members: members.iter().map(|m| m.order_id.clone()).collect(),
            created: now.clone(),
            modified: now,
        },
    );
    for m in members {
        b.insert_order(m);
    }
    // the group's own cl_order_id is taken too
    b.cl_ids.insert((t.account_id.clone(), t.cl_order_id.clone()), first.clone());
    let order = b.orders.get(&first).map(|o| b.order_response(o));
    let group = b.groups.get(&gid).map(|g| b.group_response(g));
    let mut resp = serde_json::to_value(order).unwrap_or(Value::Null);
    if let Some(obj) = resp.as_object_mut() {
        obj.insert("order_group".into(), serde_json::to_value(group).unwrap_or(Value::Null));
    }
    s.flush(b);
    ok(StatusCode::CREATED, resp)
}

pub(crate) async fn create_order(State(s): State<Arc<AppState>>, headers: HeaderMap, b: Bytes) -> R {
    let uid = user(&s, &headers)?;
    let req: OrderCreateRequest = body(&b)?;
    check_shape(&req)?;
    let mut book = s.book();
    let (account, _) = trading_account(&book, &uid, &req)?;
    let p = profile(&req.broker);
    let instrument = req.instrument.clone().unwrap_or(Instrument::Stk);
    if let Some(c) = &req.complex_order_parameters {
        if !p.accepts(&c.order_class, &instrument) {
            let (code, class, inst) = (p.code, c.order_class.as_str(), instrument.as_str());
            return Err(bad(format!("UNSUPPORTED_ORDER_CLASS: {code} does not support {class} orders for {inst}")));
        }
    }
    if book.rate_limited(&account.account_id, &p) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("RATE_LIMITED: {} trading call limit reached", p.code)));
    }
    let t = template(&book, &req, &account, &p)?;
    // ASSUMPTION: a short sale the broker cannot borrow for is refused on the create, as a 400 with the broker's
    // words, rather than accepted and rejected after
    if t.side == Side::SellShort && t.symbol.as_ref().is_some_and(|x| book.hard_to_borrow.contains(x)) {
        let sym = t.symbol.unwrap_or_default();
        return Err(bad(format!("SHORT_SALE_NOT_ALLOWED: {sym} is not available to borrow")));
    }
    // MockConfig::reserve_shares: a sell for more than the long position less the sells already working is refused
    if book.reserve_shares && t.side == Side::Sell {
        if let Some(sym) = t.symbol.clone() {
            let held = book.positions.get(&(t.account_id.clone(), sym.clone())).map_or(Decimal::ZERO, |p| p.qty);
            let working: Decimal = book
                .orders
                .values()
                .filter(|o| o.account_id == t.account_id && o.symbol.as_deref() == Some(sym.as_str()))
                .filter(|o| o.side == Side::Sell && o.open())
                .map(|o| o.qty - o.filled_qty())
                .sum();
            if t.qty > held - working {
                return Err(bad(format!(
                    "INSUFFICIENT_SHARES: {} {sym} held, {} already in working sell orders",
                    num(held),
                    num(working)
                )));
            }
        }
    }
    if let Some(c) = &req.complex_order_parameters {
        return place_group(&s, &mut book, &t, c);
    }
    let oid = t.order_id.clone();
    book.insert_order(t);
    let resp = book.orders.get(&oid).map(|o| book.order_response(o));
    s.flush(&mut book);
    ok(StatusCode::CREATED, resp)
}

fn status_matches(o: &Order, filter: Option<&str>) -> bool {
    match filter {
        None | Some("all") => true,
        Some("open") => o.open(),
        Some("closed") => !o.open(),
        Some(st) => o.status.as_str() == st,
    }
}

pub(crate) async fn list_orders(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50);
    if !(1..=500).contains(&limit) {
        // the spec lists 400 for GET /orders; the general docs say 422 for bad query parameters (asked)
        return Err(bad("INVALID_PARAMETERS: limit must be between 1 and 500"));
    }
    let b = s.book();
    let symbols: Option<Vec<&str>> = q.get("symbols").map(|x| x.split(',').collect());
    let field = |k: &str, v: &str| q.get(k).is_none_or(|x| x == v);
    let mut rows: Vec<_> = b
        .order_seq
        .iter()
        .filter_map(|id| b.orders.get(id))
        .filter(|o| o.user_id == uid && field("account_id", &o.account_id) && field("connection_id", &o.connection_id))
        .filter(|o| q.get("order_group_id").is_none_or(|g| o.group_id.as_ref() == Some(g)))
        .filter(|o| q.get("broker").is_none_or(|x| x.eq_ignore_ascii_case(o.broker.as_str())))
        .filter(|o| field("side", o.side.as_str()))
        .filter(|o| symbols.as_ref().is_none_or(|list| o.symbol.as_ref().is_some_and(|x| list.contains(&x.as_str()))))
        .filter(|o| status_matches(o, q.get("status").map(String::as_str)))
        .map(|o| b.order_response(o))
        .collect();
    if !q.get("sort").is_some_and(|s| s.ends_with(":asc")) {
        rows.reverse();
    }
    rows.truncate(limit);
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}

fn owned_order(b: &Book, uid: &str, aid: &str, oid: &str) -> Result<Order, Response> {
    b.orders
        .get(oid)
        .filter(|o| o.user_id == uid && o.account_id == aid)
        .cloned()
        .ok_or_else(|| not_found(format!("Order {oid} not found")))
}

pub(crate) async fn get_order(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((aid, oid)): Path<(String, String)>,
) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let o = owned_order(&b, &uid, &aid, &oid)?;
    ok(StatusCode::OK, b.order_response(&o))
}

pub(crate) async fn cancel_order(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((aid, oid)): Path<(String, String)>,
) -> R {
    let uid = user(&s, &headers)?;
    let mut b = s.book();
    let o = owned_order(&b, &uid, &aid, &oid)?;
    if connection_broken(&b, &o.connection_id) {
        return Err(err(StatusCode::CONFLICT, BROKEN));
    }
    if o.group_id.is_some() {
        return Err(bad("GROUP_LEG_NOT_CANCELLABLE: cancel the order group instead"));
    }
    if !o.open() || o.status == NormalizedStatus::PendingCancel {
        return Err(bad(format!("Order {oid} is not open ({})", o.status)));
    }
    let p = profile(&o.broker);
    if b.rate_limited(&o.account_id, &p) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("RATE_LIMITED: {} trading call limit reached", p.code)));
    }
    let at = Instant::now() + b.timings.cancel;
    if let Some(x) = b.orders.get_mut(&oid) {
        x.cancel_at = Some(at);
    }
    b.set_status(&oid, NormalizedStatus::PendingCancel, "pending_cancel");
    s.flush(&mut b);
    ok(StatusCode::OK, json!({ "order_id": oid, "status": "pending_cancel", "message": "Cancel request submitted" }))
}

fn replacement(b: &Book, o: &Order, req: &OrderReplaceRequest) -> Result<Order, Response> {
    let mut n = o.clone();
    n.order_id = id();
    n.cl_order_id.clone_from(&req.new_cl_order_id);
    if let Some(q) = decimal("order_qty", req.order_qty.as_ref())? {
        n.qty = q;
    }
    if let Some(l) = decimal("order_limit_price", req.order_limit_price.as_ref())? {
        n.limit = Some(l);
    }
    if let Some(st) = decimal("stop_price", req.stop_price.as_ref())? {
        n.stop = Some(st);
        n.triggered = false;
    }
    n.fills = vec![];
    n.status = NormalizedStatus::Pending;
    n.broker_status = "pending_new".into();
    n.entry_time = now_str();
    n.last_modified.clone_from(&n.entry_time);
    n.last_exec = None;
    n.ack_at = Instant::now() + b.timings.ack;
    n.replaced_by = None;
    n.cancel_at = None;
    Ok(n)
}

pub(crate) async fn replace_order(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((aid, oid)): Path<(String, String)>,
    raw: Bytes,
) -> R {
    let uid = user(&s, &headers)?;
    let req: OrderReplaceRequest = body(&raw)?;
    let mut book = s.book();
    let order = owned_order(&book, &uid, &aid, &oid)?;
    let p = profile(&order.broker);
    // Connect Trade, 22 Nov 2026: where the broker cannot modify an order, PATCH is 501 and the client cancels and
    // places itself
    if p.replace == ReplaceMode::Unsupported {
        return Err(err(
            StatusCode::NOT_IMPLEMENTED,
            format!(
                "NOT_SUPPORTED: {} does not support order modification; cancel the order and place a new one",
                p.code
            ),
        ));
    }
    check_replace(&book, &order, &aid, &req, &p)?;
    if book.rate_limited(&order.account_id, &p) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("RATE_LIMITED: {} trading call limit reached", p.code)));
    }
    let next = replacement(&book, &order, &req)?;
    let new_id = book.start_replace(&oid, next, p.replace);
    let resp = book.orders.get(&new_id).map(|x| book.order_response(x));
    s.flush(&mut book);
    ok(StatusCode::OK, resp)
}

fn owned_group(b: &Book, uid: &str, aid: &str, gid: &str) -> Result<Group, Response> {
    b.groups
        .get(gid)
        .filter(|g| g.user_id == uid && g.account_id == aid)
        .cloned()
        .ok_or_else(|| not_found(format!("Order group {gid} not found")))
}

pub(crate) async fn get_order_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((aid, gid)): Path<(String, String)>,
) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let g = owned_group(&b, &uid, &aid, &gid)?;
    ok(StatusCode::OK, b.group_response(&g))
}

pub(crate) async fn cancel_order_group(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((aid, gid)): Path<(String, String)>,
) -> R {
    let uid = user(&s, &headers)?;
    let mut b = s.book();
    let g = owned_group(&b, &uid, &aid, &gid)?;
    if connection_broken(&b, &g.connection_id) {
        return Err(err(StatusCode::CONFLICT, BROKEN));
    }
    if matches!(
        g.status,
        GroupStatus::Completed | GroupStatus::Cancelled | GroupStatus::Rejected | GroupStatus::Expired
    ) {
        return Err(err(StatusCode::CONFLICT, format!("Order group {gid} is already {}", g.status)));
    }
    let at = Instant::now() + b.timings.cancel;
    for m in &g.members {
        let open = b.orders.get(m).is_some_and(Order::open);
        if open {
            if let Some(o) = b.orders.get_mut(m) {
                o.cancel_at = Some(at);
                o.contingent = false;
            }
            b.set_status(m, NormalizedStatus::PendingCancel, "pending_cancel");
        }
    }
    s.flush(&mut b);
    ok(
        StatusCode::OK,
        json!({ "order_id": gid, "status": "pending_cancel", "message": "Group cancel request submitted" }),
    )
}
