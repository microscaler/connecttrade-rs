//! Access tokens, users, connections and the mock's OAuth page.

use super::{bad, body, caller, err, in_secs, not_found, ok, to_broker_enum, to_order_broker, user, Q, R};
use crate::book::{id, now_str, Book, Connection, PendingLink, User};
use crate::AppState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use connecttrade::models::{
    ConnectionAccount, ConnectionCreateRequest, ConnectionRepairRequest, ConnectionResponse, ConnectionType,
    UserAccessTokenRequest, UserCreateRequest,
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) async fn create_platform_token(State(s): State<Arc<AppState>>, headers: HeaderMap) -> R {
    caller(&s, &headers, false)?;
    let token = format!("mock-platform-{}", id());
    s.book().tokens.insert(token.clone(), (None, Instant::now() + Duration::from_secs(3600)));
    ok(StatusCode::OK, json!({ "access_token": token, "expires_at": in_secs(3600), "token_type": "Bearer" }))
}

pub(crate) async fn create_user_token(State(s): State<Arc<AppState>>, headers: HeaderMap, b: Bytes) -> R {
    let uid = user(&s, &headers)?;
    let req: UserAccessTokenRequest = body(&b)?;
    if req.scopes.is_empty() {
        return Err(bad("INVALID_PARAMETERS: scopes must not be empty"));
    }
    let token = format!("mock-user-{}", id());
    s.book().tokens.insert(token.clone(), (Some(uid), Instant::now() + Duration::from_secs(900)));
    ok(StatusCode::OK, json!({ "access_token": token, "expires_at": in_secs(900), "scopes": req.scopes }))
}

pub(crate) async fn list_users(State(s): State<Arc<AppState>>, headers: HeaderMap) -> R {
    caller(&s, &headers, false)?;
    let data: Vec<_> = s.book().users.values().map(|u| json!({ "user_id": u.user_id })).collect();
    ok(StatusCode::OK, json!({ "data": data, "next_cursor": null, "prev_cursor": null }))
}

pub(crate) async fn create_user(State(s): State<Arc<AppState>>, headers: HeaderMap, b: Bytes) -> R {
    caller(&s, &headers, false)?;
    let req: UserCreateRequest = body(&b)?;
    if req.user_id.is_empty() || req.user_id.len() > 64 {
        return Err(bad("INVALID_PARAMETERS: user_id must be 1 to 64 characters"));
    }
    let mut book = s.book();
    if book.users.contains_key(&req.user_id) {
        return Err(err(StatusCode::CONFLICT, format!("User {} already exists", req.user_id)));
    }
    let secret = format!("mock-secret-{}", id());
    let u = User { user_id: req.user_id.clone(), secret: secret.clone(), created_at: now_str() };
    book.users.insert(req.user_id.clone(), u);
    ok(StatusCode::OK, json!({ "message": "User created", "user_id": req.user_id, "user_secret": secret }))
}

pub(crate) async fn delete_user(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    caller(&s, &headers, false)?;
    let uid = q.get("user_id").cloned().ok_or_else(|| bad("INVALID_PARAMETERS: user_id is required"))?;
    s.book().users.remove(&uid).ok_or_else(|| bad(format!("User {uid} not found")))?;
    ok(StatusCode::OK, json!({ "message": "User deleted", "user_id": uid }))
}

pub(crate) async fn rotate_user_secret(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(uid): Path<String>,
) -> R {
    caller(&s, &headers, false)?;
    let mut book = s.book();
    let u = book.users.get_mut(&uid).ok_or_else(|| not_found(format!("User {uid} not found")))?;
    u.secret = format!("mock-secret-{}", id());
    ok(StatusCode::OK, json!({ "message": "Secret rotated", "user_id": uid, "user_secret": u.secret }))
}

fn connection_json(b: &Book, c: &Connection) -> ConnectionResponse {
    let accounts = c
        .accounts
        .iter()
        .filter_map(|a| b.accounts.get(a))
        .map(|a| ConnectionAccount {
            account_id: Some(a.account_id.clone()),
            account_number: Some(a.account_number.clone()),
        })
        .collect();
    ConnectionResponse {
        connection_id: c.connection_id.clone(),
        user_id: Some(c.user_id.clone()),
        broker: Some(to_broker_enum(&c.broker)),
        accounts: Some(accounts),
        connection_type: Some(c.connection_type.clone()),
        status: Some(c.status.clone()),
        created_at: Some(c.created_at.clone()),
        updated_at: Some(c.updated_at.clone()),
    }
}

pub(crate) async fn list_connections(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let rows: Vec<_> = b
        .connections
        .values()
        .filter(|c| c.user_id == uid && q.get("status").is_none_or(|st| st == c.status.as_str()))
        .map(|c| connection_json(&b, c))
        .collect();
    ok(StatusCode::OK, rows)
}

pub(crate) async fn list_platform_connections(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    caller(&s, &headers, false)?;
    let b = s.book();
    let rows: Vec<_> = b
        .connections
        .values()
        .rev()
        .filter(|c| q.get("status").is_none_or(|st| st == c.status.as_str()))
        .map(|c| connection_json(&b, c))
        .collect();
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}

fn link_answer(s: &AppState, link: &str, status: StatusCode) -> R {
    let url = format!("{}/mock/oauth/{link}", s.base_url());
    ok(status, json!({ "connection_request_url": url, "expires_at": in_secs(600) }))
}

pub(crate) async fn create_connection(State(s): State<Arc<AppState>>, headers: HeaderMap, b: Bytes) -> R {
    let uid = user(&s, &headers)?;
    let req: ConnectionCreateRequest = body(&b)?;
    let broker = req.broker.as_ref().ok_or_else(|| bad("INVALID_PARAMETERS: broker is required"))?;
    let link = id();
    let pending = PendingLink {
        user_id: uid,
        broker: to_order_broker(broker),
        connection_type: req.connection_type.unwrap_or(ConnectionType::Trade),
        repair_of: None,
        redirect_url: req.redirect_url,
    };
    s.book().links.insert(link.clone(), pending);
    link_answer(&s, &link, StatusCode::CREATED)
}

fn owned_connection(b: &Book, uid: &str, cid: &str) -> Result<Connection, axum::response::Response> {
    b.connections.get(cid).filter(|c| c.user_id == uid).cloned().ok_or_else(|| not_found("Connection not found"))
}

pub(crate) async fn get_connection(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(cid): Path<String>) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let c = owned_connection(&b, &uid, &cid)?;
    ok(StatusCode::OK, connection_json(&b, &c))
}

pub(crate) async fn delete_connection(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(cid): Path<String>,
) -> R {
    let uid = user(&s, &headers)?;
    let mut b = s.book();
    let c = owned_connection(&b, &uid, &cid)?;
    b.connections.remove(&cid);
    for a in c.accounts {
        b.accounts.remove(&a);
    }
    ok(StatusCode::OK, json!({ "message": "Connection deleted", "connection_id": cid }))
}

pub(crate) async fn repair_connection(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(cid): Path<String>,
    b: Bytes,
) -> R {
    let uid = user(&s, &headers)?;
    let req: ConnectionRepairRequest = if b.is_empty() { ConnectionRepairRequest::default() } else { body(&b)? };
    let link = id();
    {
        let mut book = s.book();
        let c = owned_connection(&book, &uid, &cid)?;
        let pending = PendingLink {
            user_id: uid,
            broker: c.broker,
            connection_type: c.connection_type,
            repair_of: Some(cid),
            redirect_url: req.redirect_url,
        };
        book.links.insert(link.clone(), pending);
    }
    link_answer(&s, &link, StatusCode::OK)
}

/// The broker's login page, in mock form: visiting it completes the connection (or the repair).
pub(crate) async fn oauth(State(s): State<Arc<AppState>>, Path(link): Path<String>) -> R {
    let (c, redirect) = s.book().complete_link(&link).ok_or_else(|| not_found("Unknown or used link"))?;
    match redirect {
        Some(url) => Ok(Redirect::to(&format!("{url}?connection_id={}", c.connection_id)).into_response()),
        None => ok(StatusCode::OK, json!({ "connection_id": c.connection_id, "status": c.status.as_str() })),
    }
}
