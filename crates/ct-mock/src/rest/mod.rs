//! The REST API, route for route as in the spec, plus `/mock/*` controls for tests.
//!
//! - [`platform`]: access tokens, users, connections and the mock's OAuth page;
//! - [`account`]: accounts, balances, positions, transactions;
//! - [`orders`]: orders and order groups, after the checks in [`checks`];
//! - `/mock/*`, in [`controls`];
//! - here: shared helpers, credential checks and per-request faults.

pub(crate) mod account;
pub(crate) mod checks;
pub(crate) mod controls;
pub(crate) mod orders;
pub(crate) mod platform;

use crate::AppState;
use axum::body::Bytes;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use connecttrade::models::{BrokerEnum, OrderBroker};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, Instant};

pub(crate) type Q = Query<HashMap<String, String>>;
pub(crate) type R = Result<Response, Response>;

/// An error answer in Connect Trade's shape: `{"message": ...}`.
pub(crate) fn err(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "message": message.into() }))).into_response()
}

pub(crate) fn bad(message: impl Into<String>) -> Response {
    err(StatusCode::BAD_REQUEST, message)
}

pub(crate) fn not_found(message: impl Into<String>) -> Response {
    err(StatusCode::NOT_FOUND, message)
}

#[allow(clippy::unnecessary_wraps, reason = "the success arm of every handler's Result")]
pub(crate) fn ok<T: serde::Serialize>(status: StatusCode, v: T) -> R {
    Ok((status, Json(v)).into_response())
}

pub(crate) fn body<T: DeserializeOwned>(b: &Bytes) -> Result<T, Response> {
    serde_json::from_slice(b).map_err(|e| bad(format!("INVALID_PARAMETERS: {e}")))
}

fn header(h: &HeaderMap, k: &str) -> Option<String> {
    h.get(k).and_then(|v| v.to_str().ok()).map(String::from)
}

/// A decimal field of a request.
pub(crate) fn decimal(field: &str, v: Option<&String>) -> Result<Option<Decimal>, Response> {
    v.map(|x| Decimal::from_str(x).map_err(|_| bad(format!("INVALID_PARAMETERS: {field} is not a number: {x}"))))
        .transpose()
}

/// Two names for the same broker in the spec: `BrokerEnum` (connections) and `OrderBroker` (orders).
pub(crate) fn to_order_broker(b: &BrokerEnum) -> OrderBroker {
    serde_json::from_value(Value::String(b.as_str().to_string()))
        .unwrap_or_else(|_| OrderBroker::Unknown(b.as_str().to_string()))
}

pub(crate) fn to_broker_enum(b: &OrderBroker) -> BrokerEnum {
    serde_json::from_value(Value::String(b.as_str().to_string()))
        .unwrap_or_else(|_| BrokerEnum::Unknown(b.as_str().to_string()))
}

/// `n` seconds from now, as the API writes timestamps.
pub(crate) fn in_secs(n: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(n)).format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// Who is calling: the platform (`None`), or the platform acting for a user. Checks credential headers or a token.
pub(crate) fn caller(s: &AppState, h: &HeaderMap, need_user: bool) -> Result<Option<String>, Response> {
    let unauthorized = |m: &str| err(StatusCode::UNAUTHORIZED, m);
    if let Some(auth) = header(h, "authorization") {
        let token = auth.strip_prefix("Bearer ").ok_or_else(|| unauthorized("Invalid authorization header"))?;
        let (user, expires) =
            s.book().tokens.get(token).cloned().ok_or_else(|| unauthorized("Invalid access token"))?;
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
    match s.book().users.get(&uid) {
        Some(u) if u.secret == usec => Ok(Some(uid)),
        _ => Err(unauthorized("Invalid user credentials")),
    }
}

/// [`caller`] for operations that act for a user.
pub(crate) fn user(s: &AppState, h: &HeaderMap) -> Result<String, Response> {
    caller(s, h, true)?.ok_or_else(|| err(StatusCode::UNAUTHORIZED, "User credentials are required"))
}

pub(crate) async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "ct-mock" }))
}

/// Per-request faults, the same headers as `PriceWhisperer`'s pact-mock-server, plus delays.
pub(crate) async fn faults(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
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
