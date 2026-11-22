# What Connect Trade has confirmed

Connect Trade's published documents (the spec in `spec/`, `docs.connecttrade.com`) leave some behaviour open. This page
records what Connect Trade has confirmed to us beyond them, when, and what the client and the mock do about it. The
mock (`crates/ct-mock`) is changed to match each answer; anything still unconfirmed is marked `ASSUMPTION` in
[`brokers.rs`](../crates/ct-mock/src/brokers.rs).

## 22 November 2026

| Topic | Confirmed | In the client and the mock |
| --- | --- | --- |
| Routing | Every API call goes through Connect Trade's servers to the broker; nothing is handed to the broker directly. | Connect Trade is in the path of every create, replace and cancel. |
| Latency | Connect Trade's own processing is 1-2 ms; the rest is the distance to its servers and the broker's own (retail) API. | Measure per broker on a paper account; the mock's timings are not a latency model. |
| Order and fill events | Pushed over websockets from brokers that have them, polled at the broker's maximum rate from those that do not; every update goes out on Connect Trade's websocket API, which gives the lowest latency. | Use the stream (`connecttrade::stream`) rather than polling `GET /orders`. |
| Rate limits | REST limits are configured per platform. | Over a limit the mock answers `429` (`X-Rate-Limit`, or a broker's trading-call limit); the actual numbers are agreed with Connect Trade per platform. |
| A broker that is down | The call times out; the order's state is known as soon as the broker knows it. | `Error::may_have_happened` is true for a timeout: look the order up by its `cl_order_id` before sending anything again. |
| `cl_order_id` | Required, unique per account. A `POST` that reuses one already recorded on the account returns `409`. Clients should reconcile before sending after a timeout. | The mock answers `409` to a reused `cl_order_id` on a create or a replace. A retry under the same id is safe (it cannot place twice) but is a `409`, not the order: reconcile by lookup. |
| Capability list per broker | Not published yet; in progress. | The mock's per-broker profiles are ours, from each broker's own documents and paper runs. |
| Modifying an order | Where the broker can modify, `PATCH` sends one modify request to the broker, which amends the order in place or replaces it; a replacement comes back with its new `order_id`. **Where the broker cannot, `PATCH` returns `501`**, and the order is changed with `DELETE` and a new `POST`. | `ApiError::is_not_supported()` reads the `501`, and `may_have_happened` is false for it (nothing changed). The mock answers `501 NOT_SUPPORTED` for TradeZero (`ReplaceMode::Unsupported`); it no longer pretends Connect Trade cancels and places behind one `PATCH`. |
| Partial fills | Each fill carries its own `fill_id`. | As modelled. |
| Fees | Not every broker reports fees through its API. | TradeZero and Lightspeed have no transactions in the mock; fees there come from the broker's schedule. |
| Paper trading | Available for brokers that have a paper environment (connecttrade.com/integrations); it may differ slightly from production but is generally consistent in latency. | |

### Moving a stop on a broker that answers `501`

`DELETE` then `POST` leaves the position with no stop between the two. A client that must never leave a position
unprotected places the new (tighter) stop first and cancels the old one after: for a moment two stops cover the same
shares, and if both fill the extra close is undone at once. A broker that counts working sells against the shares held
refuses the second stop; then the client falls back to cancel, confirm, place, and should record how long the position
went without a stop. `MockConfig { reserve_shares: true, .. }` models such a broker; which real brokers do it is not
confirmed yet.

### Still open

- Whether the `STOP_LOSS` and `TAKE_PROFIT` legs of an active `BRACKET` or `OCO` can be modified where the broker
  supports it (today: `GROUP_LEG_NOT_REPLACEABLE`), or a group's legs replaced in one call.
- Which brokers reserve shares for working sells, and the error a refused second sell returns.
- The exact per-broker list of `501` on `PATCH`, and whether a broker's own replace ever leaves a moment with no
  working order.
