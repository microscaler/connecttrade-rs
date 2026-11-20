//! Accounts, balances, positions and transactions.

use super::{not_found, ok, to_broker_enum, user, Q, R};
use crate::book::{now_str, num, Account, Book, Txn};
use crate::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use connecttrade::models::{AccountResponse, TransactionResponse, TransactionType};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

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

fn user_accounts<'a>(b: &'a Book, uid: &'a str, q: &'a HashMap<String, String>) -> impl Iterator<Item = &'a Account> {
    b.accounts.values().filter(move |a| {
        a.user_id == uid
            && q.get("connection_id").is_none_or(|c| c == &a.connection_id)
            && q.get("account_id").is_none_or(|c| c == &a.account_id)
            && q.get("broker").is_none_or(|x| x.eq_ignore_ascii_case(a.broker.as_str()))
    })
}

pub(crate) async fn list_accounts(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let rows: Vec<_> = user_accounts(&b, &uid, &q).map(account_json).collect();
    ok(StatusCode::OK, rows)
}

pub(crate) async fn get_account(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(aid): Path<String>) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let a = b.accounts.get(&aid).filter(|a| a.user_id == uid).ok_or_else(|| not_found("Account not found"))?;
    ok(StatusCode::OK, account_json(a))
}

pub(crate) async fn list_balances(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let rows: Vec<_> = user_accounts(&b, &uid, &q).filter_map(|a| b.balance_row(&a.account_id)).collect();
    ok(StatusCode::OK, rows)
}

pub(crate) async fn list_positions(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let symbols: Option<Vec<&str>> = q.get("symbols").map(|x| x.split(',').collect());
    let wanted =
        |sym: Option<&String>| symbols.as_ref().is_none_or(|list| sym.is_some_and(|x| list.contains(&x.as_str())));
    let rows: Vec<_> = user_accounts(&b, &uid, &q)
        .flat_map(|a| b.position_rows(Some(&a.account_id)))
        .filter(|p| wanted(p.symbol.as_ref()))
        .collect();
    ok(StatusCode::OK, rows)
}

fn transaction_json(a: &Account, t: &Txn) -> TransactionResponse {
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
        broker_description: None,
        broker_transaction_id: None,
        broker_transaction_sub_type: None,
        broker_transaction_type: None,
        fx_rate: None,
        settlement_date: None,
        trade_date: None,
    }
}

pub(crate) async fn list_transactions(State(s): State<Arc<AppState>>, headers: HeaderMap, q: Q) -> R {
    let uid = user(&s, &headers)?;
    let b = s.book();
    let accounts: HashMap<&str, &Account> = user_accounts(&b, &uid, &q).map(|a| (a.account_id.as_str(), a)).collect();
    let types: Option<Vec<&str>> = q.get("transaction_types").map(|x| x.split(',').collect());
    let limit = q.get("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).clamp(1, 500);
    let rows: Vec<TransactionResponse> = b
        .txns
        .iter()
        .rev()
        .filter(|t| types.as_ref().is_none_or(|ts| ts.contains(&t.kind.as_str())))
        .filter_map(|t| accounts.get(t.account_id.as_str()).map(|a| transaction_json(a, t)))
        .take(limit)
        .collect();
    ok(StatusCode::OK, json!({ "data": rows, "next_cursor": null, "prev_cursor": null }))
}
