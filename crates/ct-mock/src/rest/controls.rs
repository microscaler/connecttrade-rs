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

/// `POST /mock/session` `{"session": "pre" | "regular" | "post"}`.
pub(crate) async fn set_session(State(s): State<Arc<AppState>>, Json(v): Json<Value>) -> R {
    let session = match v.get("session").and_then(Value::as_str) {
        Some("pre") => Session::Pre,
        Some("regular") => Session::Regular,
        Some("post") => Session::Post,
        _ => return Err(bad("session must be pre, regular or post")),
    };
    s.book().session = session;
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
    *b = Book::new(timings);
    ok(StatusCode::OK, json!({ "ok": true }))
}
