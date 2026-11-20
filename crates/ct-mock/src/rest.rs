//! The REST API, route for route as in the spec, plus `/mock/*` controls for tests.

use crate::book::*;
use crate::brokers::profile;
use crate::AppState;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use connecttrade::models::*;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

type Q = Query<HashMap<String, String>>;
type R = Result<Response, Response>;

pub fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "message": message.into() }))).into_response()
}

fn bad(message: impl Into<String>) -> Response {
    err(StatusCode::BAD_REQUEST, message)
}

fn ok<T: serde::Serialize>(status: StatusCode, v: T) -> R {
    Ok((status, Json(v)).into_response())
}

fn body<T: DeserializeOwned>(b: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(b).map_err(|e| bad(format!("INVALID_PARAMETERS: {e}")))
}

fn header(h: &HeaderMap, k: &str) -> Option<String> {
    h.get(k).and_then(|v| v.to_str().ok()).map(String::from)
}

pub fn to_order_broker(b: &BrokerEnum) -> OrderBroker {
    serde_json::from_value(Value::String(b.as_str().to_string())).unwrap()
}

fn to_broker_enum(b: &OrderBroker) -> BrokerEnum {
    serde_json::from_value(Value::String(b.as_str().to_string())).unwrap()
}

/// Who is calling: the platform, or the platform acting for a user. Checks the credential headers or a token.
fn caller(s: &AppState, h: &HeaderMap, need_user: bool) -> Result<Option<String>, Response> {
    let unauthorized = |m: &str| err(StatusCode::UNAUTHORIZED, m);
    if let Some(auth) = header(h, "authorization") {
        let token = auth.strip_prefix("Bearer ").ok_or_else(|| unauthorized("Invalid authorization header"))?;
        let b = s.book.lock().unwrap();
        let (user, expires) = b.tokens.get(token).cloned().ok_or_else(|| unauthorized("Invalid access token"))?;
        if Instant::now() > expires {
            return Err(unauthorized("Access token has expired"));
        }
        if need_user && user.is_none() {
            return Err(unauthorized("A user access token is required"));
        }
        return Ok(user);
    }
    let id = header(h, "client-id");
    let secret = header(h, "client-secret");
    if id.as_deref() != Some(&s.config.client_id) || secret.as_deref() != Some(&s.config.client_secret) {
        return Err(unauthorized("Invalid client credentials"));
    }
    if !need_user {
        return Ok(None);
    }
    let uid = header(h, "user-id").ok_or_else(|| unauthorized("Missing user-id"))?;
    let usec = header(h, "user-secret").ok_or_else(|| unauthorized("Missing user-secret"))?;
    let b = s.book.lock().unwrap();
    match b.users.get(&uid) {
        Some(u) if u.secret == usec => Ok(Some(uid)),
        _ => Err(unauthorized("Invalid user credentials")),
    }
}

// ---------- tokens ----------

pub async fn create_platform_token(State(s): State<Arc<AppState>>, h: HeaderMap) -> R {
    caller(&s, &h, false)?;
    let token = format!("mock-platform-{}", id());
    s.book.lock().unwrap().tokens.insert(token.clone(), (None, Instant::now() + Duration::from_secs(3600)));
    ok(StatusCode::OK, json!({ "access_token": token, "expires_at": in_secs(3600), "token_type": "Bearer" }))
}

pub async fn create_user_token(State(s): State<Arc<AppState>>, h: HeaderMap, b: Bytes) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let req: UserAccessTokenRequest = body(&b)?;
    if req.scopes.is_empty() {
        return Err(bad("INVALID_PARAMETERS: scopes must not be empty"));
    }
    let token = format!("mock-user-{}", id());
    s.book.lock().unwrap().tokens.insert(token.clone(), (Some(user), Instant::now() + Duration::from_secs(900)));
    ok(StatusCode::OK, json!({ "access_token": token, "expires_at": in_secs(900), "scopes": req.scopes }))
}

fn in_secs(n: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(n)).format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

// ---------- users ----------

pub async fn list_users(State(s): State<Arc<AppState>>, h: HeaderMap) -> R {
    caller(&s, &h, false)?;
    let b = s.book.lock().unwrap();
    let data: Vec<Value> = b.users.values().map(|u| json!({ "user_id": u.user_id })).collect();
    ok(StatusCode::OK, json!({ "data": data, "next_cursor": null, "prev_cursor": null }))
}

pub async fn create_user(State(s): State<Arc<AppState>>, h: HeaderMap, b: Bytes) -> R {
    caller(&s, &h, false)?;
    let req: UserCreateRequest = body(&b)?;
    if req.user_id.is_empty() || req.user_id.len() > 64 {
        return Err(bad("INVALID_PARAMETERS: user_id must be 1 to 64 characters"));
    }
    let mut book = s.book.lock().unwrap();
    if book.users.contains_key(&req.user_id) {
        return Err(err(StatusCode::CONFLICT, format!("User {} already exists", req.user_id)));
    }
    let secret = format!("mock-secret-{}", id());
    book.users.insert(
        req.user_id.clone(),
        User { user_id: req.user_id.clone(), secret: secret.clone(), created_at: now_str() },
    );
    ok(StatusCode::OK, json!({ "message": "User created", "user_id": req.user_id, "user_secret": secret }))
}

pub async fn delete_user(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    caller(&s, &h, false)?;
    let uid = q.get("user_id").cloned().ok_or_else(|| bad("INVALID_PARAMETERS: user_id is required"))?;
    let mut b = s.book.lock().unwrap();
    b.users.remove(&uid).ok_or_else(|| bad(format!("User {uid} not found")))?;
    ok(StatusCode::OK, json!({ "message": "User deleted", "user_id": uid }))
}

pub async fn rotate_user_secret(State(s): State<Arc<AppState>>, h: HeaderMap, Path(uid): Path<String>) -> R {
    caller(&s, &h, false)?;
    let mut b = s.book.lock().unwrap();
    let u = b.users.get_mut(&uid).ok_or_else(|| err(StatusCode::NOT_FOUND, format!("User {uid} not found")))?;
    u.secret = format!("mock-secret-{}", id());
    ok(StatusCode::OK, json!({ "message": "Secret rotated", "user_id": uid, "user_secret": u.secret }))
}

// ---------- connections ----------

fn connection_json(b: &Book, c: &Connection) -> ConnectionResponse {
    ConnectionResponse {
        connection_id: c.connection_id.clone(),
        user_id: Some(c.user_id.clone()),
        broker: Some(to_broker_enum(&c.broker)),
        accounts: Some(
            c.accounts
                .iter()
                .filter_map(|a| b.accounts.get(a))
                .map(|a| ConnectionAccount {
                    account_id: Some(a.account_id.clone()),
                    account_number: Some(a.account_number.clone()),
                })
                .collect(),
        ),
        connection_type: Some(c.connection_type.clone()),
        status: Some(c.status.clone()),
        created_at: Some(c.created_at.clone()),
        updated_at: Some(c.updated_at.clone()),
    }
}

pub async fn list_connections(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let rows: Vec<_> = b
        .connections
        .values()
        .filter(|c| c.user_id == user && q.get("status").is_none_or(|st| st == c.status.as_str()))
        .map(|c| connection_json(&b, c))
        .collect();
    ok(StatusCode::OK, rows)
}

pub async fn list_platform_connections(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    caller(&s, &h, false)?;
    let b = s.book.lock().unwrap();
    let rows: Vec<_> = b
        .connections
        .values()
        .rev()
        .filter(|c| q.get("status").is_none_or(|st| st == c.status.as_str()))
        .map(|c| connection_json(&b, c))
        .collect();
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}

pub async fn create_connection(State(s): State<Arc<AppState>>, h: HeaderMap, b: Bytes) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let req: ConnectionCreateRequest = body(&b)?;
    let broker = req.broker.as_ref().ok_or_else(|| bad("INVALID_PARAMETERS: broker is required"))?;
    let link = id();
    s.book.lock().unwrap().links.insert(
        link.clone(),
        PendingLink {
            user_id: user,
            broker: to_order_broker(broker),
            connection_type: req.connection_type.unwrap_or(ConnectionType::Trade),
            repair_of: None,
            redirect_url: req.redirect_url,
        },
    );
    ok(
        StatusCode::CREATED,
        json!({ "connection_request_url": format!("{}/mock/oauth/{link}", s.base_url()), "expires_at": in_secs(600) }),
    )
}

pub async fn get_connection(State(s): State<Arc<AppState>>, h: HeaderMap, Path(cid): Path<String>) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let c = b
        .connections
        .get(&cid)
        .filter(|c| c.user_id == user)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Connection not found"))?;
    ok(StatusCode::OK, connection_json(&b, c))
}

pub async fn delete_connection(State(s): State<Arc<AppState>>, h: HeaderMap, Path(cid): Path<String>) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let mut b = s.book.lock().unwrap();
    if !b.connections.get(&cid).is_some_and(|c| c.user_id == user) {
        return Err(err(StatusCode::NOT_FOUND, "Connection not found"));
    }
    let c = b.connections.remove(&cid).unwrap();
    for a in c.accounts {
        b.accounts.remove(&a);
    }
    ok(StatusCode::OK, json!({ "message": "Connection deleted", "connection_id": cid }))
}

pub async fn repair_connection(State(s): State<Arc<AppState>>, h: HeaderMap, Path(cid): Path<String>, b: Bytes) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let req: ConnectionRepairRequest = if b.is_empty() { ConnectionRepairRequest::default() } else { body(&b)? };
    let mut book = s.book.lock().unwrap();
    let c = book
        .connections
        .get(&cid)
        .filter(|c| c.user_id == user)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Connection not found"))?;
    let link = id();
    book.links.insert(
        link.clone(),
        PendingLink {
            user_id: user,
            broker: c.broker,
            connection_type: c.connection_type,
            repair_of: Some(cid),
            redirect_url: req.redirect_url,
        },
    );
    ok(
        StatusCode::OK,
        json!({ "connection_request_url": format!("{}/mock/oauth/{link}", s.base_url()), "expires_at": in_secs(600) }),
    )
}

/// The broker's login page, in mock form: visiting it completes the connection (or the repair).
pub async fn oauth(State(s): State<Arc<AppState>>, Path(link): Path<String>) -> R {
    let (c, redirect) = s
        .book
        .lock()
        .unwrap()
        .complete_link(&link)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Unknown or used link"))?;
    match redirect {
        Some(url) => Ok(Redirect::to(&format!("{url}?connection_id={}", c.connection_id)).into_response()),
        None => ok(StatusCode::OK, json!({ "connection_id": c.connection_id, "status": c.status.as_str() })),
    }
}

// ---------- accounts, balances, positions, transactions ----------

fn account_json(a: &Account) -> AccountResponse {
    AccountResponse {
        account_id: a.account_id.clone(),
        connection_id: a.connection_id.clone(),
        account_name: None,
        account_number: Some(a.account_number.clone()),
        institution_name: Some(to_broker_enum(&a.broker)),
        created_at: Some(a.created_at.clone()),
        last_update_ts: Some(now_str()),
        historical_data_sync_completed: Some(true),
    }
}

fn user_accounts<'a>(
    b: &'a Book,
    user: &'a str,
    q: &'a HashMap<String, String>,
) -> impl Iterator<Item = &'a Account> + 'a {
    b.accounts.values().filter(move |a| {
        a.user_id == user
            && q.get("connection_id").is_none_or(|c| c == &a.connection_id)
            && q.get("account_id").is_none_or(|c| c == &a.account_id)
            && q.get("broker").is_none_or(|x| x.eq_ignore_ascii_case(a.broker.as_str()))
    })
}

pub async fn list_accounts(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let rows: Vec<_> = user_accounts(&b, &user, &q).map(account_json).collect();
    ok(StatusCode::OK, rows)
}

pub async fn get_account(State(s): State<Arc<AppState>>, h: HeaderMap, Path(aid): Path<String>) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let a = b
        .accounts
        .get(&aid)
        .filter(|a| a.user_id == user)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Account not found"))?;
    ok(StatusCode::OK, account_json(a))
}

pub async fn list_balances(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let rows: Vec<_> = user_accounts(&b, &user, &q).filter_map(|a| b.balance_row(&a.account_id)).collect();
    ok(StatusCode::OK, rows)
}

pub async fn list_positions(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let symbols: Option<Vec<String>> = q.get("symbols").map(|x| x.split(',').map(String::from).collect());
    let mut rows = vec![];
    for a in user_accounts(&b, &user, &q) {
        rows.extend(
            b.position_rows(Some(&a.account_id))
                .into_iter()
                .filter(|p| symbols.as_ref().is_none_or(|s| p.symbol.as_ref().is_some_and(|x| s.contains(x)))),
        );
    }
    ok(StatusCode::OK, rows)
}

pub async fn list_transactions(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let accounts: Vec<&Account> = user_accounts(&b, &user, &q).collect();
    let types: Option<Vec<String>> = q.get("transaction_types").map(|x| x.split(',').map(String::from).collect());
    let mut rows: Vec<TransactionResponse> = b
        .txns
        .iter()
        .filter(|t| accounts.iter().any(|a| a.account_id == t.account_id))
        .filter(|t| types.as_ref().is_none_or(|ts| ts.iter().any(|x| x == t.kind.as_str())))
        .map(|t| {
            let a = &b.accounts[&t.account_id];
            TransactionResponse {
                transaction_id: t.id.clone(),
                connection_id: Some(a.connection_id.clone()),
                account_id: Some(a.account_id.clone()),
                account_number: Some(a.account_number.clone()),
                transaction_type: Some(t.kind.clone()),
                symbol: t.symbol.clone(),
                price: t.price.map(num),
                qty: t.qty.map(num),
                side: t.side.clone(),
                amount: Some(num(t.amount)),
                currency: Some("USD".into()),
                fee: (t.kind == TransactionType::Fee).then(|| num(-t.amount)),
                broker: Some(a.broker.as_str().to_string()),
                transaction_time: Some(t.time.clone()),
                order_id: t.order_id.clone(),
                ..TransactionResponse::default_with_id(t.id.clone())
            }
        })
        .collect();
    rows.reverse();
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 500);
    rows.truncate(limit);
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}

trait DefaultWithId {
    fn default_with_id(id: String) -> Self;
}

impl DefaultWithId for TransactionResponse {
    fn default_with_id(id: String) -> Self {
        TransactionResponse {
            transaction_id: id,
            account_id: None,
            account_number: None,
            amount: None,
            broker: None,
            broker_description: None,
            broker_transaction_id: None,
            broker_transaction_sub_type: None,
            broker_transaction_type: None,
            connection_id: None,
            currency: None,
            fee: None,
            fx_rate: None,
            order_id: None,
            price: None,
            qty: None,
            settlement_date: None,
            side: None,
            symbol: None,
            trade_date: None,
            transaction_time: None,
            transaction_type: None,
        }
    }
}

// ---------- orders ----------

fn parse_num(field: &str, v: &Option<String>) -> Result<Option<f64>, Response> {
    match v {
        None => Ok(None),
        Some(x) => {
            x.parse::<f64>().map(Some).map_err(|_| bad(format!("INVALID_PARAMETERS: {field} is not a number: {x}")))
        }
    }
}

/// The checks Connect Trade makes before a create reaches the broker.
fn check_shape(req: &OrderCreateRequest) -> Result<(), Response> {
    if req.cl_order_id.is_empty() || req.cl_order_id.len() > 36 {
        return Err(bad("INVALID_PARAMETERS: cl_order_id must be 1 to 36 characters"));
    }
    let oco = req.complex_order_parameters.as_ref().is_some_and(|c| c.order_class == ComplexOrderClass::Oco);
    if oco {
        if req.order_type.is_some() || req.order_limit_price.is_some() || req.stop_price.is_some() {
            return Err(bad("INVALID_PARAMETERS: OCO orders must omit order_type, order_limit_price and stop_price"));
        }
    } else {
        let t = req.order_type.as_ref().ok_or_else(|| bad("INVALID_PARAMETERS: order_type is required"))?;
        if matches!(t, OrderType::Lmt | OrderType::Stplmt) && req.order_limit_price.is_none() {
            return Err(bad("INVALID_PARAMETERS: order_limit_price is required for LMT and STPLMT"));
        }
        if matches!(t, OrderType::Stp | OrderType::Stplmt) && req.stop_price.is_none() {
            return Err(bad("INVALID_PARAMETERS: stop_price is required for STP and STPLMT"));
        }
    }
    if let Some(c) = &req.complex_order_parameters {
        match c.order_class {
            ComplexOrderClass::Bracket | ComplexOrderClass::Oco if c.take_profit.is_none() || c.stop_loss.is_none() => {
                return Err(bad("INVALID_PARAMETERS: BRACKET and OCO need both take_profit and stop_loss"))
            }
            ComplexOrderClass::Oto if c.take_profit.is_some() == c.stop_loss.is_some() => {
                return Err(bad("INVALID_PARAMETERS: OTO needs exactly one of take_profit or stop_loss"))
            }
            _ => {}
        }
    }
    let multileg = req.instrument.as_ref() == Some(&Instrument::MlegOpt);
    if multileg {
        let n = req.legs.as_ref().map_or(0, Vec::len);
        if !(2..=4).contains(&n) {
            return Err(bad("INVALID_PARAMETERS: multi-leg orders need 2 to 4 legs"));
        }
    } else if req.symbol.is_none() {
        return Err(bad("INVALID_PARAMETERS: symbol is required"));
    }
    if !multileg && req.side.is_none() {
        return Err(bad("INVALID_PARAMETERS: side is required"));
    }
    if req.order_qty.is_none() && req.order_notional.is_none() {
        return Err(bad("INVALID_PARAMETERS: order_qty or order_notional is required"));
    }
    Ok(())
}

fn reverse(side: &Side) -> Side {
    match side {
        Side::Buy | Side::BuyToOpen | Side::BuyToCover | Side::BuyToClose => Side::Sell,
        _ => Side::Buy,
    }
}

pub async fn create_order(State(s): State<Arc<AppState>>, h: HeaderMap, b: Bytes) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let req: OrderCreateRequest = body(&b)?;
    check_shape(&req)?;
    let qty = parse_num("order_qty", &req.order_qty)?.unwrap_or(1.0);
    let limit = parse_num("order_limit_price", &req.order_limit_price)?;
    let stop = parse_num("stop_price", &req.stop_price)?;

    let mut book = s.book.lock().unwrap();
    let account = book
        .accounts
        .get(&req.account_id)
        .filter(|a| a.user_id == user)
        .cloned()
        .ok_or_else(|| bad(format!("INVALID_PARAMETERS: account {} not found", req.account_id)))?;
    let conn = book.connections[&account.connection_id].clone();
    if conn.status == ConnectionStatus::Broken {
        return Err(err(StatusCode::CONFLICT, "Connection is BROKEN; repair it before trading"));
    }
    if account.broker != req.broker {
        return Err(bad(format!("INVALID_PARAMETERS: account {} is not a {} account", req.account_id, req.broker)));
    }
    if book.cl_ids.contains_key(&(account.account_id.clone(), req.cl_order_id.clone())) {
        return Err(err(StatusCode::CONFLICT, format!("Duplicate cl_order_id {}", req.cl_order_id)));
    }
    let p = profile(&req.broker);
    let instrument = req.instrument.clone().unwrap_or(Instrument::Stk);
    if let Some(c) = &req.complex_order_parameters {
        if !p.accepts(&c.order_class, &instrument) {
            return Err(bad(format!(
                "UNSUPPORTED_ORDER_CLASS: {} does not support {} orders for {}",
                p.code,
                c.order_class.as_str(),
                instrument.as_str()
            )));
        }
    }
    if book.rate_limited(&account.account_id, &p) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("RATE_LIMITED: {} trading call limit reached", p.code)));
    }

    let now = now_str();
    let ack = Instant::now() + book.timings.ack;
    let side = req.side.clone().unwrap_or(Side::Buy);
    // session rules the broker enforces after accepting the order (an asynchronous rejection, as on TradeZero)
    let reject = if p.stops_regular_hours_only && book.session != Session::Regular {
        match req.order_type {
            Some(OrderType::Stp) => Some("R79: Stop orders are not allowed at this time".to_string()),
            Some(OrderType::Mkt) => Some("R78: Market orders are not allowed at this time".to_string()),
            _ => None,
        }
    } else {
        None
    };
    let template = Order {
        order_id: id(),
        cl_order_id: req.cl_order_id.clone(),
        user_id: user.clone(),
        connection_id: conn.connection_id.clone(),
        account_id: account.account_id.clone(),
        account_number: account.account_number.clone(),
        broker: req.broker.clone(),
        side: side.clone(),
        qty,
        order_type: req.order_type.clone().unwrap_or(OrderType::Lmt),
        tif: req.order_tif.clone(),
        limit,
        stop,
        symbol: req.symbol.clone(),
        instrument: instrument.clone(),
        legs: req.legs.clone(),
        status: NormalizedStatus::Pending,
        broker_status: "pending_new".into(),
        broker_text: None,
        entry_time: now.clone(),
        last_modified: now.clone(),
        last_exec: None,
        fills: vec![],
        class: OrderClass::Simple,
        group_id: None,
        role: None,
        parent: None,
        contingent: false,
        ack_at: ack,
        reject,
        triggered: false,
        cancel_at: None,
        replaced_by: None,
    };

    let Some(c) = req.complex_order_parameters.clone() else {
        let oid = template.order_id.clone();
        book.insert_order(template);
        let resp = book.order_response(&book.orders[&oid]);
        s.flush(&mut book);
        return ok(StatusCode::CREATED, resp);
    };

    // a complex order: a group with its members, entry first
    let gid = id();
    let class: OrderClass = serde_json::from_value(Value::String(c.order_class.as_str().into())).unwrap();
    let mut members = vec![];
    let entry_id = if c.order_class == ComplexOrderClass::Oco {
        None
    } else {
        let mut e = template.clone();
        e.cl_order_id = format!("{}:EN", req.cl_order_id);
        e.class = class.clone();
        e.group_id = Some(gid.clone());
        e.role = Some(GroupRole::Entry);
        members.push(e);
        Some(members[0].order_id.clone())
    };
    let exit_side = if entry_id.is_some() { reverse(&side) } else { side.clone() };
    if let Some(tp) = &c.take_profit {
        let mut x = template.clone();
        x.order_id = id();
        x.cl_order_id = format!("{}:TP", req.cl_order_id);
        x.side = exit_side.clone();
        x.order_type = OrderType::Lmt;
        x.limit = parse_num("take_profit.order_limit_price", &Some(tp.order_limit_price.clone()))?;
        x.stop = None;
        x.class = class.clone();
        x.group_id = Some(gid.clone());
        x.role = Some(GroupRole::TakeProfit);
        x.parent = entry_id.clone();
        x.contingent = entry_id.is_some();
        x.reject = None;
        if x.contingent {
            x.broker_status = "contingent".into();
        }
        members.push(x);
    }
    if let Some(sl) = &c.stop_loss {
        let mut x = template.clone();
        x.order_id = id();
        x.cl_order_id = format!("{}:SL", req.cl_order_id);
        x.side = exit_side.clone();
        x.stop = parse_num("stop_loss.stop_price", &Some(sl.stop_price.clone()))?;
        x.limit = parse_num("stop_loss.order_limit_price", &sl.order_limit_price)?;
        x.order_type = if x.limit.is_some() { OrderType::Stplmt } else { OrderType::Stp };
        x.class = class.clone();
        x.group_id = Some(gid.clone());
        x.role = Some(GroupRole::StopLoss);
        x.parent = entry_id.clone();
        x.contingent = entry_id.is_some();
        x.reject = None;
        if x.contingent {
            x.broker_status = "contingent".into();
        }
        members.push(x);
    }
    let group = Group {
        group_id: gid.clone(),
        class,
        account_id: account.account_id.clone(),
        user_id: user,
        connection_id: conn.connection_id,
        cl_order_id: req.cl_order_id.clone(),
        broker: req.broker.clone(),
        status: if entry_id.is_some() { GroupStatus::PendingEntry } else { GroupStatus::Active },
        members: members.iter().map(|m| m.order_id.clone()).collect(),
        created: now.clone(),
        modified: now,
    };
    let first = members[0].order_id.clone();
    book.groups.insert(gid.clone(), group);
    for m in members {
        book.insert_order(m);
    }
    // the group's own cl_order_id is taken too
    book.cl_ids.insert((account.account_id.clone(), req.cl_order_id.clone()), first.clone());
    let mut resp = serde_json::to_value(book.order_response(&book.orders[&first])).unwrap();
    resp["order_group"] = serde_json::to_value(book.group_response(&book.groups[&gid])).unwrap();
    s.flush(&mut book);
    ok(StatusCode::CREATED, resp)
}

pub async fn list_orders(State(s): State<Arc<AppState>>, h: HeaderMap, q: Q) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let symbols: Option<Vec<String>> = q.get("symbols").map(|x| x.split(',').map(String::from).collect());
    let mut rows: Vec<OrderResponse> = b
        .order_seq
        .iter()
        .filter_map(|id| b.orders.get(id))
        .filter(|o| o.user_id == user)
        .filter(|o| q.get("account_id").is_none_or(|a| a == &o.account_id))
        .filter(|o| q.get("connection_id").is_none_or(|a| a == &o.connection_id))
        .filter(|o| q.get("order_group_id").is_none_or(|g| o.group_id.as_ref() == Some(g)))
        .filter(|o| q.get("broker").is_none_or(|x| x.eq_ignore_ascii_case(o.broker.as_str())))
        .filter(|o| q.get("side").is_none_or(|x| x == o.side.as_str()))
        .filter(|o| symbols.as_ref().is_none_or(|s| o.symbol.as_ref().is_some_and(|x| s.contains(x))))
        .filter(|o| match q.get("status").map(String::as_str) {
            None | Some("all") => true,
            Some("open") => o.open(),
            Some("closed") => !o.open(),
            Some(st) => o.status.as_str() == st,
        })
        .map(|o| b.order_response(o))
        .collect();
    let asc = q.get("sort").is_some_and(|s| s.ends_with(":asc"));
    if !asc {
        rows.reverse();
    }
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50);
    if !(1..=500).contains(&limit) {
        // the spec lists 400 for GET /orders; the general docs say 422 for bad query parameters (asked)
        return Err(bad("INVALID_PARAMETERS: limit must be between 1 and 500"));
    }
    rows.truncate(limit);
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}

fn owned_order(b: &Book, user: &str, aid: &str, oid: &str) -> Result<Order, Response> {
    b.orders
        .get(oid)
        .filter(|o| o.user_id == user && o.account_id == aid)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Order {oid} not found")))
}

fn connection_broken(b: &Book, o: &Order) -> bool {
    b.connections.get(&o.connection_id).is_some_and(|c| c.status == ConnectionStatus::Broken)
}

pub async fn get_order(State(s): State<Arc<AppState>>, h: HeaderMap, Path((aid, oid)): Path<(String, String)>) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let o = owned_order(&b, &user, &aid, &oid)?;
    ok(StatusCode::OK, b.order_response(&o))
}

pub async fn cancel_order(State(s): State<Arc<AppState>>, h: HeaderMap, Path((aid, oid)): Path<(String, String)>) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let mut b = s.book.lock().unwrap();
    let o = owned_order(&b, &user, &aid, &oid)?;
    if connection_broken(&b, &o) {
        return Err(err(StatusCode::CONFLICT, "Connection is BROKEN; repair it before trading"));
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

pub async fn replace_order(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path((aid, oid)): Path<(String, String)>,
    b: Bytes,
) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let req: OrderReplaceRequest = body(&b)?;
    let mut book = s.book.lock().unwrap();
    let o = owned_order(&book, &user, &aid, &oid)?;
    if connection_broken(&book, &o) {
        return Err(err(StatusCode::CONFLICT, "Connection is BROKEN; repair it before trading"));
    }
    if o.group_id.is_some() {
        return Err(bad("GROUP_LEG_NOT_REPLACEABLE: a member of an order group cannot be replaced"));
    }
    if req.broker != o.broker {
        return Err(bad("INVALID_PARAMETERS: broker does not match the order"));
    }
    if !matches!(o.status, NormalizedStatus::Pending | NormalizedStatus::Working | NormalizedStatus::PartiallyFilled) {
        return Err(bad(format!("Order {oid} cannot be replaced ({})", o.status)));
    }
    if req.new_cl_order_id.is_empty() || req.new_cl_order_id.len() > 36 {
        return Err(bad("INVALID_PARAMETERS: new_cl_order_id must be 1 to 36 characters"));
    }
    if book.cl_ids.contains_key(&(aid.clone(), req.new_cl_order_id.clone())) {
        return Err(err(StatusCode::CONFLICT, format!("Duplicate cl_order_id {}", req.new_cl_order_id)));
    }
    let p = profile(&o.broker);
    if req.order_qty.is_some() && !p.replace_quantity {
        return Err(bad(format!(
            "INVALID_PARAMETERS: {} cannot change an order's quantity; cancel and place a new order",
            p.code
        )));
    }
    if book.rate_limited(&o.account_id, &p) {
        return Err(err(StatusCode::TOO_MANY_REQUESTS, format!("RATE_LIMITED: {} trading call limit reached", p.code)));
    }
    let mut n = o.clone();
    n.order_id = id();
    n.cl_order_id = req.new_cl_order_id.clone();
    if let Some(q) = parse_num("order_qty", &req.order_qty)? {
        n.qty = q;
    }
    if let Some(l) = parse_num("order_limit_price", &req.order_limit_price)? {
        n.limit = Some(l);
    }
    if let Some(st) = parse_num("stop_price", &req.stop_price)? {
        n.stop = Some(st);
        n.triggered = false;
    }
    n.fills = vec![];
    n.status = NormalizedStatus::Pending;
    n.broker_status = "pending_new".into();
    n.entry_time = now_str();
    n.last_modified = n.entry_time.clone();
    n.last_exec = None;
    n.ack_at = Instant::now() + book.timings.ack;
    n.replaced_by = None;
    n.cancel_at = None;
    let new_id = book.start_replace(&oid, n, p.replace);
    let resp = book.order_response(&book.orders[&new_id]);
    s.flush(&mut book);
    ok(StatusCode::OK, resp)
}

pub async fn get_order_group(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path((aid, gid)): Path<(String, String)>,
) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let b = s.book.lock().unwrap();
    let g = b
        .groups
        .get(&gid)
        .filter(|g| g.user_id == user && g.account_id == aid)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Order group {gid} not found")))?;
    ok(StatusCode::OK, b.group_response(g))
}

pub async fn cancel_order_group(
    State(s): State<Arc<AppState>>,
    h: HeaderMap,
    Path((aid, gid)): Path<(String, String)>,
) -> R {
    let user = caller(&s, &h, true)?.unwrap();
    let mut b = s.book.lock().unwrap();
    let g = b
        .groups
        .get(&gid)
        .filter(|g| g.user_id == user && g.account_id == aid)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("Order group {gid} not found")))?;
    if b.connections.get(&g.connection_id).is_some_and(|c| c.status == ConnectionStatus::Broken) {
        return Err(err(StatusCode::CONFLICT, "Connection is BROKEN; repair it before trading"));
    }
    if matches!(
        g.status,
        GroupStatus::Completed | GroupStatus::Cancelled | GroupStatus::Rejected | GroupStatus::Expired
    ) {
        return Err(err(StatusCode::CONFLICT, format!("Order group {gid} is already {}", g.status)));
    }
    let at = Instant::now() + b.timings.cancel;
    for m in &g.members {
        if b.orders.get(m).is_some_and(|o| o.open()) {
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

// ---------- /mock controls ----------

/// `POST /mock/prices` `{"AAPL": "190.5"}` or numbers: set prices; crossing orders fill on the next tick.
pub async fn set_prices(State(s): State<Arc<AppState>>, Json(v): Json<HashMap<String, Value>>) -> R {
    let mut b = s.book.lock().unwrap();
    for (sym, p) in v {
        let px = match &p {
            Value::Number(n) => n.as_f64(),
            Value::String(x) => x.parse().ok(),
            _ => None,
        }
        .ok_or_else(|| bad(format!("bad price for {sym}")))?;
        b.prices.insert(sym, px);
    }
    b.tick();
    s.flush(&mut b);
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/session` `{"session": "pre" | "regular" | "post"}`.
pub async fn set_session(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> R {
    let sess = match v["session"].as_str() {
        Some("pre") => Session::Pre,
        Some("regular") => Session::Regular,
        Some("post") => Session::Post,
        _ => return Err(bad("session must be pre, regular or post")),
    };
    s.book.lock().unwrap().session = sess;
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/connections/{id}/break`: the broker revoked the login; trading returns 409 until a repair.
pub async fn break_connection(State(s): State<Arc<AppState>>, Path(cid): Path<String>) -> R {
    let mut b = s.book.lock().unwrap();
    let c = b.connections.get_mut(&cid).ok_or_else(|| err(StatusCode::NOT_FOUND, "Connection not found"))?;
    c.status = ConnectionStatus::Broken;
    c.updated_at = now_str();
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/reset`: forget everything.
pub async fn reset(State(s): State<Arc<AppState>>) -> R {
    let mut b = s.book.lock().unwrap();
    let timings = b.timings.clone();
    *b = Book::new(timings);
    ok(StatusCode::OK, json!({ "ok": true }))
}

pub async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "ct-mock" }))
}

/// Per-request faults, the same headers as PriceWhisperer's pact-mock-server, plus delays.
pub async fn faults(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let h = req.headers().clone();
    let on = |k: &str| h.get(k).and_then(|v| v.to_str().ok()) == Some("true");
    if on("x-auth-failure") {
        return err(StatusCode::UNAUTHORIZED, "Invalid client credentials");
    }
    if on("x-service-unavailable") {
        return err(StatusCode::SERVICE_UNAVAILABLE, "Service unavailable");
    }
    if on("x-rate-limit") {
        let mut r = err(StatusCode::TOO_MANY_REQUESTS, "RATE_LIMITED: too many requests");
        if let Some(v) = h.get("x-rate-limit-retry-after") {
            r.headers_mut().insert("retry-after", v.clone());
        }
        return r;
    }
    let ms = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    // before the change is made: a client that times out here sent a change that did not happen
    if let Some(d) = ms("x-mock-delay-before-ms") {
        tokio::time::sleep(Duration::from_millis(d)).await;
    }
    let resp = next.run(req).await;
    // after the change is made: a client that times out here sent a change that did happen
    if let Some(d) = ms("x-mock-delay-after-ms") {
        tokio::time::sleep(Duration::from_millis(d)).await;
    }
    resp
}
