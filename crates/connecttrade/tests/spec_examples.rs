//! Every example in Connect Trade's own spec decodes into the generated types, and every request example
//! round-trips. When Connect Trade changes a payload, this is the first test to fail.

use connecttrade::models::*;
use connecttrade::ops::OPERATIONS;
use serde::de::DeserializeOwned;
use serde_json::Value;

fn spec() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../spec/openapi.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn decode<T: DeserializeOwned>(what: &str, v: &Value) {
    if let Err(e) = serde_json::from_value::<T>(v.clone()) {
        panic!("{what}: {e}\n{}", serde_json::to_string_pretty(v).unwrap());
    }
}

/// Decode a 2xx example into the operation's answer type.
fn decode_ok(op: &str, v: &Value) {
    let what = format!("{op} 2xx example");
    match op {
        "create_platform_token" => decode::<PlatformAccessTokenResponse>(&what, v),
        "create_user_token" => decode::<UserAccessTokenResponse>(&what, v),
        "list_users" => decode::<UserListResponse>(&what, v),
        "create_user" => decode::<UserCreateResponse>(&what, v),
        "delete_user" => decode::<UserDeleteResponse>(&what, v),
        "rotate_user_secret" => decode::<UserSecretRotateResponse>(&what, v),
        "list_connections" => decode::<Vec<ConnectionResponse>>(&what, v),
        "create_connection" | "repair_connection" => decode::<ConnectionCreateResponse>(&what, v),
        "get_connection" => decode::<ConnectionResponse>(&what, v),
        "delete_connection" => decode::<ConnectionDeleteResponse>(&what, v),
        "list_platform_connections" => decode::<ConnectionListResponse>(&what, v),
        "list_accounts" => decode::<Vec<AccountResponse>>(&what, v),
        "get_account" => decode::<AccountResponse>(&what, v),
        "list_balances" => decode::<Vec<BalanceResponse>>(&what, v),
        "create_order" | "replace_order" => decode::<OrderCreateResponse>(&what, v),
        "list_orders" => decode::<OrderListResponse>(&what, v),
        "get_order" => decode::<OrderResponse>(&what, v),
        "cancel_order" | "cancel_order_group" => decode::<OrderCancelResponse>(&what, v),
        "get_order_group" => decode::<OrderGroup>(&what, v),
        "list_positions" => decode::<Vec<PositionResponse>>(&what, v),
        "list_transactions" => decode::<TransactionListResponse>(&what, v),
        other => panic!("no decoder for {other}: add it here"),
    }
}

fn examples(content: &Value) -> Vec<Value> {
    let mut out = vec![];
    if let Some(e) = content.get("example") {
        out.push(e.clone());
    }
    if let Some(m) = content.get("examples").and_then(Value::as_object) {
        for e in m.values() {
            if let Some(v) = e.get("value") {
                out.push(v.clone());
            }
        }
    }
    out
}

#[test]
fn every_operation_in_the_spec_is_generated() {
    let spec = spec();
    let mut in_spec = 0;
    for (path, item) in spec["paths"].as_object().unwrap() {
        for m in ["get", "post", "put", "patch", "delete"] {
            if item.get(m).is_some() {
                in_spec += 1;
                assert!(
                    OPERATIONS.iter().any(|(_, method, p, _)| *p == path && method.as_str().eq_ignore_ascii_case(m)),
                    "{m} {path} is in the spec but not generated"
                );
            }
        }
    }
    assert_eq!(in_spec, OPERATIONS.len());
}

#[test]
fn every_response_example_decodes() {
    let spec = spec();
    let mut checked = 0;
    for (id, method, path, _) in OPERATIONS {
        let op = &spec["paths"][path][method.as_str().to_ascii_lowercase()];
        for (code, r) in op["responses"].as_object().unwrap() {
            let Some(content) = r.pointer("/content/application~1json") else { continue };
            for ex in examples(content) {
                if code.starts_with('2') {
                    decode_ok(id, &ex);
                } else {
                    decode::<ErrorResponse>(&format!("{id} {code} example"), &ex);
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 60, "only {checked} examples found: did the spec move?");
}

#[test]
fn every_request_example_round_trips() {
    let spec = spec();
    for (id, method, path, _) in OPERATIONS {
        let op = &spec["paths"][path][method.as_str().to_ascii_lowercase()];
        let Some(content) = op.pointer("/requestBody/content/application~1json") else { continue };
        for ex in examples(content) {
            let back = match *id {
                "create_order" => {
                    serde_json::to_value(serde_json::from_value::<OrderCreateRequest>(ex.clone()).unwrap())
                }
                "replace_order" => {
                    serde_json::to_value(serde_json::from_value::<OrderReplaceRequest>(ex.clone()).unwrap())
                }
                other => panic!("request example for {other}: add a decoder"),
            }
            .unwrap();
            assert_eq!(strip_nulls(&back), strip_nulls(&ex), "{id} request example does not round-trip");
        }
    }
}

/// The "Order examples" section of the docs carries the canonical order shapes, including brackets, OTO, OCO and
/// multi-leg orders. Each request block is an `OrderCreateRequest`; the one response block is an
/// `OrderCreateResponse` with its order group.
#[test]
fn the_order_examples_in_the_docs_decode() {
    let spec = spec();
    let tag =
        spec["tags"].as_array().unwrap().iter().find(|t| t["name"] == "Order examples").expect("Order examples tag");
    let md = tag["description"].as_str().unwrap();
    let mut requests = 0;
    let mut responses = 0;
    for block in md.split("```json").skip(1) {
        let json = block.split("```").next().unwrap();
        let v: Value = serde_json::from_str(json).unwrap_or_else(|e| panic!("bad json in docs: {e}\n{json}"));
        if v.get("order_id").is_some() {
            decode::<OrderCreateResponse>("docs response example", &v);
            let r: OrderCreateResponse = serde_json::from_value(v).unwrap();
            let group = r.order_group.expect("bracket response carries its group");
            assert_eq!(group.orders.as_ref().map(Vec::len), Some(3));
            responses += 1;
        } else {
            let req: OrderCreateRequest = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{e}\n{v}"));
            assert_eq!(strip_nulls(&serde_json::to_value(&req).unwrap()), strip_nulls(&v));
            requests += 1;
        }
    }
    assert!(requests >= 10 && responses == 1, "{requests} requests, {responses} responses");
}

#[test]
fn unknown_enum_values_decode_instead_of_failing() {
    let s: NormalizedStatus = serde_json::from_value(Value::String("held_by_broker".into())).unwrap();
    assert_eq!(s, NormalizedStatus::Unknown("held_by_broker".into()));
    let s: NormalizedStatus = serde_json::from_value(Value::String("partially_filled".into())).unwrap();
    assert_eq!(s, NormalizedStatus::PartiallyFilled);
    assert_eq!(serde_json::to_value(OrderTif::DayPlus).unwrap(), "DAY+");
}

fn strip_nulls(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            Value::Object(m.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.clone(), strip_nulls(v))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(strip_nulls).collect()),
        other => other.clone(),
    }
}
