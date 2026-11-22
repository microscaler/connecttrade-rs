//! The client against `ct-mock`. Users, credentials, tokens and connections.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "integration tests: a failing test should fail loudly"
)]

mod common;

use common::*;
use connecttrade::models::*;
use connecttrade::{ops, Credentials, Error};

#[test]
fn users_are_managed_with_platform_credentials() {
    let env = world();
    let made =
        env.client.send(&env.platform, ops::create_user(&UserCreateRequest { user_id: "alice".into() })).unwrap();
    assert_eq!(made.user_id, "alice");
    let dup =
        env.client.send(&env.platform, ops::create_user(&UserCreateRequest { user_id: "alice".into() })).unwrap_err();
    assert_eq!(dup.api().unwrap().status, 409);
    let rotated = env.client.send(&env.platform, ops::rotate_user_secret("alice")).unwrap();
    assert_ne!(rotated.user_secret, made.user_secret);
    let list = env.client.send(&env.platform, ops::list_users(&Default::default())).unwrap();
    assert!(list.data.unwrap().iter().any(|u| u.user_id.as_deref() == Some("alice")));
    // the old secret no longer works
    let stale = env.platform.for_user("alice", &made.user_secret).unwrap();
    let e = env.client.send(&stale, ops::list_accounts(&Default::default())).unwrap_err();
    assert_eq!(e.api().unwrap().status, 401);
    env.client.send(&env.platform, ops::delete_user(&ops::DeleteUserQuery { user_id: Some("alice".into()) })).unwrap();
}

#[test]
fn a_user_operation_with_platform_credentials_fails_before_sending() {
    let env = world();
    let e = env.client.send(&env.platform, ops::list_accounts(&Default::default())).unwrap_err();
    assert!(matches!(e, Error::Credentials { missing: "user credentials", .. }), "{e:?}");
}

#[test]
fn a_connection_gives_an_account_with_a_balance() {
    let env = world();
    let t = env.trader(OrderBroker::Tradezero);
    let conns = env.client.send(&t.creds, ops::list_connections(&Default::default())).unwrap();
    assert_eq!(conns[0].status, Some(ConnectionStatus::Active));
    let bal = env.client.send(&t.creds, ops::list_balances(&Default::default())).unwrap();
    assert_eq!(bal[0].cash.as_deref(), Some("100000"));
    let all = env.client.send(&env.platform, ops::list_platform_connections(&Default::default())).unwrap();
    assert!(all.data.unwrap().iter().any(|c| c.connection_id == t.connection_id));
}

#[test]
fn a_broken_connection_refuses_trading_until_repaired() {
    let env = world();
    let t = env.trader(OrderBroker::Tradier);
    post_json(&format!("{}/mock/connections/{}/break", env.mock.url(), t.connection_id), &serde_json::json!({}));
    let e = env
        .client
        .send(&t.creds, ops::create_order(&order(&t, "br-c", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap_err();
    assert!(e.api().unwrap().is_broken_connection(), "{e:?}");
    let link = env
        .client
        .send(&t.creds, ops::repair_connection(&t.connection_id, &ConnectionRepairRequest::default()))
        .unwrap();
    visit(&link.connection_request_url);
    let c = env.client.send(&t.creds, ops::get_connection(&t.connection_id)).unwrap();
    assert_eq!(c.status, Some(ConnectionStatus::Active));
    env.client
        .send(&t.creds, ops::create_order(&order(&t, "br-c", Side::Buy, "1", OrderType::Mkt, None, None)))
        .unwrap();
}

#[test]
fn tokens_stand_in_for_credentials() {
    let env = world();
    let t = env.trader(OrderBroker::Tastytrade);
    let tok = env
        .client
        .send(
            &t.creds,
            ops::create_user_token(&UserAccessTokenRequest { scopes: vec![TokenScope::Read, TokenScope::Trade] }),
        )
        .unwrap();
    let bearer = Credentials::Bearer(tok.access_token.into());
    let accounts = env.client.send(&bearer, ops::list_accounts(&Default::default())).unwrap();
    assert_eq!(accounts[0].account_id, t.account_id);
}
