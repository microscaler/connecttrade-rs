//! Regression tests for the adversarial audit of 22 Nov 2026 (issues #9 to #22). Each failed on v0.2.1.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "integration tests: a failing test should fail loudly; the fake server runs on a plain OS thread"
)]

use connecttrade::models::*;
use connecttrade::request::encode_path;
use connecttrade::stream::{ClientMessage, ServerMessage};
use connecttrade::{ops, Credentials, Error, HttpResponse, MayTransport, Transport};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PLATFORM_SECRET: &str = "pl4tform-s3cret";
const USER_SECRET: &str = "us3r-s3cret";

fn creds() -> Credentials {
    Credentials::platform("client-1", PLATFORM_SECRET).for_user("user-1", USER_SECRET).unwrap()
}

// #1 secrets in Debug
#[test]
fn audit_01_debug_of_a_request_never_shows_a_secret() {
    let http = ops::list_accounts(&Default::default()).to_http("https://api.connecttrade.com", &creds()).unwrap();
    let shown = format!("{http:?}");
    assert!(!shown.contains(PLATFORM_SECRET) && !shown.contains(USER_SECRET), "HttpRequest Debug: {shown}");
}

#[test]
fn audit_01_debug_of_a_stream_authenticate_never_shows_a_secret() {
    let m = ClientMessage::Authenticate {
        client_id: Some("client-1".into()),
        client_secret: Some(PLATFORM_SECRET.into()),
        access_token: Some("t0ken-s3cret".into()),
    };
    let shown = format!("{m:?}");
    assert!(!shown.contains(PLATFORM_SECRET) && !shown.contains("t0ken-s3cret"), "ClientMessage Debug: {shown}");
}

// #2 an order with no client order id
const ORDER_WITHOUT_CL_ID: &str = r#"{"user_id":"u","connection_id":"c","account_id":"a","order_id":"o-1",
  "broker":"TRADEZERO","normalized_status":"working","side":"SELL","order_qty":"10","order_type":"STP",
  "order_tif":"DAY","stop_price":"185","symbol":"AAPL","fills":[],"legs":null,"cl_order_id":null}"#;

#[test]
fn audit_02_an_order_placed_outside_the_platform_decodes() {
    let page = format!(r#"{{"data":[{ORDER_WITHOUT_CL_ID}],"next_cursor":null}}"#);
    let r = ops::list_orders(&Default::default()).parse(&HttpResponse::new(200, page.into_bytes()));
    assert!(r.is_ok(), "one order without cl_order_id fails the whole page: {:?}", r.err());
}

// #4 a DELETE resent on its own after a stale connection
#[test]
fn audit_04_a_cancel_is_never_sent_twice() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let deletes = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&deletes);
    std::thread::spawn(move || {
        let mut n = 0;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            n += 1;
            let first_connection = n == 1;
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let method = line.split(' ').next().unwrap_or_default().to_string();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                if method == "DELETE" {
                    let k = seen.fetch_add(1, Ordering::SeqCst);
                    if first_connection && k == 0 {
                        // the cancel is taken, and the connection dies before the answer
                        break;
                    }
                    let b = r#"{"message":"Order o-1 is already pending cancel"}"#;
                    write!(
                        stream,
                        "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{b}",
                        b.len()
                    )
                    .unwrap();
                } else {
                    let b = r#"{"order_id":"o-1"}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{b}", b.len()).unwrap();
                }
                stream.flush().unwrap();
            }
        }
    });
    let t = MayTransport::new(Duration::from_secs(2), Duration::from_secs(3)).unwrap();
    let base = format!("http://{addr}");
    // a read leaves a pooled connection to reuse
    let get = ops::get_order("a", "o-1").to_http(&base, &creds()).unwrap();
    t.send(&get).unwrap();
    let del = ops::cancel_order("a", "o-1");
    let http = del.to_http(&base, &creds()).unwrap();
    let r = t.send(&http).map_err(Error::from).and_then(|resp| del.parse(&resp).map(|_| ()));
    let sent = deletes.load(Ordering::SeqCst);
    assert_eq!(sent, 1, "the cancel reached the server {sent} times; the caller got {r:?}");
}

// #6 broken connection told by a word
#[test]
fn audit_06_a_duplicate_is_not_a_broken_connection() {
    let r = ops::cancel_order("a", "o")
        .parse(&HttpResponse::new(409, br#"{"message":"Duplicate cl_order_id broken-arrow-7"}"#.to_vec()));
    let e = r.unwrap_err();
    let a = e.api().unwrap();
    assert!(!a.is_broken_connection(), "a duplicate client order id read as a broken connection: {a:?}");
}

// #7 one odd update ends the stream session
#[test]
fn audit_07_an_odd_order_update_does_not_end_the_stream() {
    let msg = format!(r#"{{"type":"orders","data":{ORDER_WITHOUT_CL_ID}}}"#);
    assert!(serde_json::from_str::<ServerMessage>(&msg).is_ok(), "an order without cl_order_id is a valid update");
    // and an update this version cannot read is handed over, not an error that ends the session
    let odd = r#"{"type":"orders","data":{"order_id":7,"user_secret":"x"}}"#;
    match connecttrade::stream::decode(odd.as_bytes()) {
        Some(ServerMessage::Undecoded { type_, raw, .. }) => {
            assert_eq!(type_.as_deref(), Some("orders"));
            assert!(!raw.contains("\"x\""), "the raw frame keeps a secret: {raw}");
        }
        other => panic!("expected Undecoded, got {other:?}"),
    }
}

// #8 credentials in clear
#[test]
fn audit_08_credentials_are_never_sent_in_clear() {
    let r = ops::list_accounts(&Default::default()).to_http("http://api.connecttrade.com", &creds());
    assert!(r.is_err(), "both secrets would go over plain HTTP: {:?}", r.map(|h| h.url.clone()));
}

// #9 a secret kept in an error
#[test]
fn audit_09_an_error_never_keeps_a_secret() {
    let body = format!(r#"{{"user_id":"user-1","user_secret":"{USER_SECRET}","message":7}}"#);
    let e = ops::create_user(&UserCreateRequest { user_id: "user-1".into() })
        .parse(&HttpResponse::new(201, body.into_bytes()))
        .unwrap_err();
    let shown = format!("{e:?} / {e}");
    assert!(!shown.contains(USER_SECRET), "create_user decode error: {shown}");
}

// #10 dot segments
#[test]
fn audit_10_an_id_is_never_a_dot_segment() {
    for id in [".", ".."] {
        assert_ne!(encode_path(id), id, "{id:?} goes into the path as a dot segment");
    }
}

// #14 log injection
#[test]
fn audit_14_an_error_is_one_line() {
    let e = ops::get_order("a", "o")
        .parse(&HttpResponse::new(400, br#"{"message":"bad\n2026-11-22T10:00:00Z INFO order o-1 filled"}"#.to_vec()));
    let shown = e.unwrap_err().to_string();
    assert!(!shown.contains('\n'), "a server message forges a log line: {shown:?}");
}
