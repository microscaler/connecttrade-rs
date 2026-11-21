//! `/mock/*`: what a test changes about the world.

use super::{bad, not_found, ok, R};
use crate::book::{now_str, Book, Session};
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use connecttrade::models::ConnectionStatus;
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

/// `POST /mock/prices` `{"AAPL": "190.5"}` (strings or numbers): set prices; crossing orders fill now.
pub(crate) async fn set_prices(State(s): State<Arc<AppState>>, Json(v): Json<HashMap<String, Value>>) -> R {
    let mut prices = Vec::with_capacity(v.len());
    for (sym, p) in v {
        let text = match &p {
            Value::String(x) => x.clone(),
            Value::Number(n) => n.to_string(),
            _ => return Err(bad(format!("bad price for {sym}"))),
        };
        let px = Decimal::from_str(&text).map_err(|_| bad(format!("bad price for {sym}: {text}")))?;
        prices.push((sym, px));
    }
    let mut b = s.book();
    b.prices.extend(prices);
    b.tick();
    s.flush(&mut b);
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/session` `{"session": "pre" | "regular" | "post", "new_day"?: true}`. With `new_day`, the day ends
/// first: every open day order expires.
pub(crate) async fn set_session(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> R {
    let session = match v.get("session").and_then(Value::as_str) {
        Some("pre") => Session::Pre,
        Some("regular") => Session::Regular,
        Some("post") => Session::Post,
        _ => return Err(bad("session must be pre, regular or post")),
    };
    let mut b = s.book();
    if v.get("new_day").and_then(Value::as_bool) == Some(true) {
        b.new_day();
    }
    b.session = session;
    s.flush(&mut b);
    ok(StatusCode::OK, json!({ "ok": true }))
}

fn decimal(v: Option<&Value>, what: &str) -> Result<Option<Decimal>, axum::response::Response> {
    let text = match v {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(x)) => x.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(_) => return Err(bad(format!("bad {what}"))),
    };
    Decimal::from_str(&text).map(Some).map_err(|_| bad(format!("bad {what}: {text}")))
}

/// `POST /mock/trade` `{"symbol": "AAPL", "price": "190.5", "size"?: 4}`: the market trades once (tape mode).
pub(crate) async fn trade(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> R {
    let symbol = v.get("symbol").and_then(Value::as_str).ok_or_else(|| bad("symbol is required"))?.to_string();
    let price = decimal(v.get("price"), "price")?.ok_or_else(|| bad("price is required"))?;
    let size = decimal(v.get("size"), "size")?;
    let mut b = s.book();
    if !b.tape {
        return Err(bad("the mock is not a tape: start it with MockConfig::tape, or set prices with /mock/prices"));
    }
    b.trade(&symbol, price, size);
    s.flush(&mut b);
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/connections/{id}/break`: the broker revoked the login; trading returns 409 until a repair.
pub(crate) async fn break_connection(State(s): State<Arc<AppState>>, Path(cid): Path<String>) -> R {
    let mut b = s.book();
    let c = b.connections.get_mut(&cid).ok_or_else(|| not_found("Connection not found"))?;
    c.status = ConnectionStatus::Broken;
    c.updated_at = now_str();
    ok(StatusCode::OK, json!({ "ok": true }))
}

/// `POST /mock/reset`: forget everything.
pub(crate) async fn reset(State(s): State<Arc<AppState>>) -> R {
    let mut b = s.book();
    let timings = b.timings.clone();
    let (tape, hard_to_borrow, rate_window) = (b.tape, b.hard_to_borrow.clone(), b.rate_window);
    *b = Book::new(timings);
    b.tape = tape;
    b.hard_to_borrow = hard_to_borrow;
    b.rate_window = rate_window;
    ok(StatusCode::OK, json!({ "ok": true }))
}
