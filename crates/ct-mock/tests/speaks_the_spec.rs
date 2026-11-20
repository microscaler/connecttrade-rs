//! The mock's answers checked against Connect Trade's own schemas, not against our generated types (which the
//! mock and the client share, so a mistake there would pass both). Every response body of a scripted session is
//! validated against the spec's schema for that operation and status.
//!
//! `required` lists are not enforced: Connect Trade's own examples break them (`price_effect`, `legs` are null or
//! absent), so the check is on types and enum values of what is present.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_methods,
    reason = "integration test: a failing test should fail loudly; it runs on a plain OS thread, never a may worker"
)]

use connecttrade::models::*;
use connecttrade::{ops, Client, Credentials, HttpRequest, HttpResponse, Transport, TransportError, UreqTransport};
use ct_mock::{MockConfig, MockServer};
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Operation, status, body.
type Seen = Arc<Mutex<Vec<(&'static str, u16, Vec<u8>)>>>;

struct Recorder {
    inner: UreqTransport,
    seen: Seen,
}

impl Transport for Recorder {
    fn send(&self, r: &HttpRequest) -> Result<HttpResponse, TransportError> {
        let resp = self.inner.send(r)?;
        self.seen.lock().unwrap().push((r.operation, resp.status, resp.body.clone()));
        Ok(resp)
    }
}

/// `OpenAPI` 3.0 schema → JSON Schema: refs to `#/definitions`, `nullable` to a null type, `required` dropped.
fn convert(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                match k.as_str() {
                    "$ref" => {
                        out.insert(
                            k.clone(),
                            Value::String(x.as_str().unwrap().replace("#/components/schemas/", "#/definitions/")),
                        );
                    }
                    "nullable" | "required" | "example" | "examples" | "description" | "default" => {}
                    _ => {
                        out.insert(k.clone(), convert(x));
                    }
                }
            }
            if m.get("nullable") == Some(&Value::Bool(true)) {
                if let Some(Value::String(t)) = out.get("type").cloned() {
                    out.insert("type".into(), json!([t, "null"]));
                    if let Some(Value::Array(e)) = out.get_mut("enum") {
                        e.push(Value::Null);
                    }
                } else {
                    return json!({ "anyOf": [Value::Object(out), { "type": "null" }] });
                }
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(convert).collect()),
        other => other.clone(),
    }
}

#[test]
fn every_answer_of_a_session_matches_the_spec() {
    let spec: Value = serde_json::from_str(
        &std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/openapi.json")).unwrap(),
    )
    .unwrap();
    let definitions = convert(&spec["components"]["schemas"]);

    let mock = MockServer::start(MockConfig::default()).expect("the mock starts");
    let seen = Arc::new(Mutex::new(vec![]));
    let client = Client::new(mock.url(), Recorder { inner: UreqTransport::default(), seen: Arc::clone(&seen) });
    let platform = Credentials::platform("mock-client-id", "mock-client-secret");

    // a session touching every operation
    client.send(&platform, ops::create_platform_token()).unwrap();
    let u = client.send(&platform, ops::create_user(&UserCreateRequest { user_id: "spec-user".into() })).unwrap();
    client.send(&platform, ops::create_user(&UserCreateRequest { user_id: "spec-user".into() })).unwrap_err();
    client.send(&platform, ops::list_users(&Default::default())).unwrap();
    let u2 = client.send(&platform, ops::rotate_user_secret("spec-user")).unwrap();
    let creds = platform.for_user("spec-user", &u2.user_secret).unwrap();
    let _ = u;
    client.send(&creds, ops::create_user_token(&UserAccessTokenRequest { scopes: vec![TokenScope::Read] })).unwrap();
    let mut accounts = vec![];
    for broker in ["TASTYTRADE", "TRADEZERO"] {
        let link = client
            .send(
                &creds,
                ops::create_connection(&ConnectionCreateRequest {
                    broker: Some(serde_json::from_value(json!(broker)).unwrap()),
                    connection_type: Some(ConnectionType::Trade),
                    redirect_url: None,
                    redirect_on_error_url: None,
                }),
            )
            .unwrap();
        ureq::get(&link.connection_request_url).call().unwrap();
    }
    for a in client.send(&creds, ops::list_accounts(&Default::default())).unwrap() {
        client.send(&creds, ops::get_account(&a.account_id)).unwrap();
        client.send(&creds, ops::get_connection(&a.connection_id)).unwrap();
        accounts.push(a);
    }
    client.send(&creds, ops::list_connections(&Default::default())).unwrap();
    client.send(&platform, ops::list_platform_connections(&Default::default())).unwrap();
    let tasty = accounts.iter().find(|a| a.institution_name == Some(BrokerEnum::Tastytrade)).unwrap();
    let mut req = OrderCreateRequest {
        account_id: tasty.account_id.clone(),
        broker: OrderBroker::Tastytrade,
        cl_order_id: "spec-1".into(),
        side: Some(Side::Buy),
        order_qty: Some("10".into()),
        order_type: Some(OrderType::Lmt),
        order_tif: OrderTif::Day,
        order_limit_price: Some("100".into()),
        stop_price: None,
        symbol: Some("AAPL".into()),
        instrument: Some(Instrument::Stk),
        complex_order_parameters: Some(ComplexOrderParameters {
            order_class: ComplexOrderClass::Bracket,
            take_profit: Some(ComplexOrderTakeProfit { order_limit_price: "110".into() }),
            stop_loss: Some(ComplexOrderStopLoss { stop_price: "95".into(), order_limit_price: Some("94.5".into()) }),
        }),
        exec_dest: None,
        legs: None,
        order_notional: None,
        price_effect: None,
    };
    let bracket = client.send(&creds, ops::create_order(&req)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let gid = bracket.order_group_id.clone().unwrap();
    client.send(&creds, ops::get_order_group(&tasty.account_id, &gid)).unwrap();
    client.send(&creds, ops::cancel_order(&tasty.account_id, &bracket.order_id)).unwrap_err(); // 400 GROUP_LEG_NOT_CANCELLABLE
    req.cl_order_id = "spec-2".into();
    req.complex_order_parameters = None;
    req.order_limit_price = Some("1".into());
    let simple = client.send(&creds, ops::create_order(&req)).unwrap();
    std::thread::sleep(Duration::from_millis(60));
    let mut r = OrderReplaceRequest {
        new_cl_order_id: "spec-2r".into(),
        broker: OrderBroker::Tastytrade,
        order_qty: None,
        order_notional: None,
        order_limit_price: Some("2".into()),
        stop_price: None,
    };
    let replaced = client.send(&creds, ops::replace_order(&tasty.account_id, &simple.order_id, &r)).unwrap();
    r.new_cl_order_id = "spec-2rr".into();
    client.send(&creds, ops::replace_order(&tasty.account_id, &bracket.order_id, &r)).unwrap_err(); // 400 GROUP_LEG_NOT_REPLACEABLE
    std::thread::sleep(Duration::from_millis(60));
    client.send(&creds, ops::cancel_order(&tasty.account_id, &replaced.order_id)).unwrap();
    client.send(&creds, ops::get_order(&tasty.account_id, &simple.order_id)).unwrap();
    client.send(&creds, ops::cancel_order_group(&tasty.account_id, &gid)).unwrap();
    client.send(&creds, ops::get_order(&tasty.account_id, "missing")).unwrap_err(); // 404
    client.send(&creds, ops::list_orders(&Default::default())).unwrap();
    client.send(&creds, ops::list_orders(&ops::ListOrdersQuery { limit: Some(0), ..Default::default() })).unwrap_err(); // 422
    client.send(&creds, ops::list_positions(&Default::default())).unwrap();
    client.send(&creds, ops::list_balances(&Default::default())).unwrap();
    client.send(&creds, ops::list_transactions(&Default::default())).unwrap();
    let repair =
        client.send(&creds, ops::repair_connection(&tasty.connection_id, &ConnectionRepairRequest::default())).unwrap();
    let _ = repair;
    client.send(&creds, ops::delete_connection(&accounts[1].connection_id)).unwrap();
    client.send(&platform, ops::delete_user(&ops::DeleteUserQuery { user_id: Some("spec-user".into()) })).unwrap();

    // validate
    let seen = seen.lock().unwrap();
    let mut covered = std::collections::BTreeSet::new();
    let mut failures = vec![];
    for (op, status, body) in seen.iter() {
        let (_, method, path, _) = ops::OPERATIONS.iter().find(|(id, ..)| id == op).unwrap();
        let o = &spec["paths"][path][method.as_str().to_ascii_lowercase()];
        let Some(schema) = o["responses"][status.to_string()].pointer("/content/application~1json/schema") else {
            failures.push(format!("{op}: status {status} is not in the spec"));
            continue;
        };
        let mut root = convert(schema).as_object().cloned().unwrap_or_default();
        root.insert("definitions".into(), definitions.clone());
        let validator = jsonschema::validator_for(&Value::Object(root)).unwrap();
        let instance: Value = serde_json::from_slice(body).unwrap();
        for e in validator.iter_errors(&instance) {
            failures.push(format!("{op} {status}: {e} at {}", e.instance_path));
        }
        covered.insert(*op);
    }
    assert!(failures.is_empty(), "the mock does not speak the spec:\n{}", failures.join("\n"));
    let missing: Vec<_> = ops::OPERATIONS.iter().map(|(id, ..)| *id).filter(|id| !covered.contains(id)).collect();
    assert!(missing.is_empty(), "operations not exercised: {missing:?}");
}
