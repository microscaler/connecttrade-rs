# connecttrade-rs

A Rust client for [Connect Trade](https://connecttrade.com), the broker connectivity API (one normalised API in front of TradeZero, tastytrade, Tradier, Lightspeed, Alpaca and other brokers), and **ct-mock**, a stateful mock of it to exercise a client before pointing it at a real account.

| Crate | What it is |
| --- | --- |
| [`connecttrade`](crates/connecttrade) | Models and requests generated from Connect Trade's OpenAPI spec; a blocking HTTPS transport; the order stream (WebSocket) |
| [`ct-mock`](crates/ct-mock) | Connect Trade as documented, REST and stream, with each broker's rules: `cargo run -p ct-mock` serves it on port 8098 |
| `xtask` | `cargo xtask codegen [--check]`: the code generator |

## Using the client

```rust,ignore
use connecttrade::{models::*, ops, Client, Credentials, UreqTransport, PRODUCTION_URL};

let platform = Credentials::platform(client_id, client_secret);
let client = Client::new(PRODUCTION_URL, UreqTransport::default());
let user = platform.for_user(user_id, user_secret).unwrap();

let accounts = client.send(&user, ops::list_accounts(&Default::default()))?;
let order = client.send(&user, ops::create_order(&OrderCreateRequest {
    account_id: accounts[0].account_id.clone(),
    broker: OrderBroker::Tradier,
    cl_order_id: "stop-aapl-1".into(),
    side: Some(Side::Sell),
    order_qty: Some("10".into()),
    order_type: Some(OrderType::Stp),
    stop_price: Some("185.00".into()),
    order_tif: OrderTif::Day,
    symbol: Some("AAPL".into()),
    instrument: Some(Instrument::Stk),
    complex_order_parameters: None,
    exec_dest: None,
    legs: None,
    order_limit_price: None,
    order_notional: None,
    price_effect: None,
}))?;
```

### Design

- **Generated, never hand-edited.** `crates/connecttrade/src/generated/` comes from `spec/openapi.json` (Connect Trade's, as published) and `spec/overlay.json` (ours: operation names and the corrections the spec needs, each with its reason; see [spec/README.md](spec/README.md)). CI fails when the two drift apart, and checks every Monday whether Connect Trade published a new spec.
- **Sans-IO.** Every operation returns a `Request<T>`: method, path, query, body, the credentials it needs and the type a 2xx decodes to. `Request::to_http` and `Request::parse` are the two halves around any I/O; `Client` joins them with a `Transport`. The built-in `UreqTransport` is blocking HTTPS over rustls; a caller with its own runtime (coroutines, async) brings its own transport.
- **Did it happen?** Every error answers the one question an order path has after a failed change: `Error::may_have_happened()`. A request that was never sent (connect or TLS failed) is safe to retry; one that timed out, broke, got a 5xx or a 2xx we could not decode must be looked up by its `cl_order_id`, never sent again.
- **Lenient where the wire is.** Response fields are `Option` except identifiers, because Connect Trade's examples leave out or null fields its spec calls required; decimals and timestamps stay strings, as sent; every enum has an `Unknown(String)` catch-all. A broker quirk never turns a placed order into a decode error.
- **Connect Trade's error codes** (`UNSUPPORTED_ORDER_CLASS`, `GROUP_LEG_NOT_REPLACEABLE`, `GROUP_LEG_NOT_CANCELLABLE`) are parsed into `ErrorCode`.
- **The stream** (`connecttrade::stream`, not in the spec) follows Connect Trade's docs: welcome, authenticate within 30 seconds, subscribe, subscribe per user, incremental updates, ping. It is blocking: run it on its own thread, and reconnect with the same `StreamConfig`, which restores topics and users. Updates are incremental, so re-read state over REST after every reconnect.

Features: `ureq` (the transport) and `stream` (the WebSocket client), both default. Without them the crate is models and sans-IO requests only.

## The mock

`ct-mock` is a model of Connect Trade as documented, plus our assumptions where the documents are silent. Every assumption is marked `ASSUMPTION` in [`brokers.rs`](crates/ct-mock/src/brokers.rs); when a paper account disagrees, the mock is corrected.

- Users, connections (the OAuth link is a URL on the mock; fetching it completes the login), accounts, balances, positions, transactions.
- Orders through their life as prices move (`POST /mock/prices`): partial fills, stops and limits, brackets, OTO and OCO groups with contingent exits; replace in place, or cancel then place with a gap (TradeZero, which has no replace); the group-leg rules; asynchronous session rejections (TradeZero's `R79` for a plain stop outside regular hours).
- Per-broker profiles: which brokers take groups, which cannot change quantity on a replace (Tradier), call limits (Tradier: 60 trading calls a minute), which brokers have no transactions (TradeZero, Lightspeed).
- The stream at `/stream`, with the 10-session and 500-user limits.
- Faults per request: `X-Auth-Failure`, `X-Service-Unavailable`, `X-Rate-Limit`, and `X-Mock-Delay-Before-Ms` / `X-Mock-Delay-After-Ms` for a timeout before or after the change was made.
- Controls: `POST /mock/prices`, `POST /mock/session` (`pre`, `regular`, `post`), `POST /mock/connections/{id}/break`, `POST /mock/reset`.

It accepts the platform credentials `mock-client-id` / `mock-client-secret` (or `CT_MOCK_CLIENT_ID` / `CT_MOCK_CLIENT_SECRET`). An image: `docker build -f crates/ct-mock/Dockerfile -t ct-mock .`

## Tests

`cargo test --workspace` runs:

- the client against the mock: every behaviour above, end to end over HTTP and WebSocket;
- every example in Connect Trade's spec and docs, decoded into the generated types (and request examples round-tripped);
- every answer of a scripted mock session validated against **the spec's own JSON schemas**, so the mock cannot drift from the spec through types it shares with the client.

## Licence

MIT OR Apache-2.0, for the code in this repository. `spec/openapi.json`, `spec/llms.txt` and `spec/llms-full.txt` are Connect Trade's, included as published.
