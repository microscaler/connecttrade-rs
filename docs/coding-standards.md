# Coding standards

The Microscaler JSF-inspired profile, the same as BRRTRouter, lifeguard, sesame-idam and rerp: rules adapted from the [JSF AV C++ coding standards](https://www.stroustrup.com/JSF-AV-rules.pdf) for Rust, enforced by tools rather than review. Every rule below fails CI, and the same checks run in the pre-commit and pre-push hooks.

| Where | What |
| --- | --- |
| `Cargo.toml` `[workspace.lints]` | Lint levels. Every crate opts in with `[lints] workspace = true`. CI runs clippy with `-D warnings`, so `warn` means "fails CI". |
| `clippy.toml` | JSF thresholds, test allowances, and the `may`-runtime bans. |
| `rust-toolchain.toml` | The shared Microscaler toolchain (1.99.0), so CI, local builds and the other repos use the same compiler and clippy. |
| `rustfmt.toml` | 120 columns; the code generator formats with the same file. |
| `scripts/check-file-sizes.sh` | The large-file rules. |
| `scripts/check-http-clients.sh` | One HTTP client: `may_minihttp`. |
| `.pre-commit-config.yaml` | fmt, file sizes, the HTTP-client check and codegen drift on commit; clippy and tests on push; Conventional Commits. |

## The rules

**No panics in production code (JSF AV Rule 208).** `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, `unimplemented!` and slice indexing are lints. Recoverable failures are typed `Result`s. Tests may unwrap, expect, panic and index (`allow-*-in-tests`; integration test files say so at the top), because a failing test should fail loudly. A mutex poisoned by a panicking handler is recovered, not unwrapped (`AppState::book`).

**Bounded complexity (Rules 1 to 3).** Cognitive complexity at most 30, functions at most 200 lines, at most 8 arguments, type complexity at most 300. Pedantic clippy is on, with the same three style allows as rerp (`similar_names`, `items_after_statements`, `default_trait_access`) and `module_name_repetitions`.

**Errors are handled, not dropped.** `let _ = <must_use>` is a lint: handle the result, or bind it to a named variable that says why it does not matter (`let _no_listener = ...`). Public functions returning `Result` document `# Errors`; public pure functions are `#[must_use]`.

**Every exception says why.** `allow_attributes_without_reason` is on: an `#[allow]` without `reason = "..."` fails CI.

**No unsafe.** `unsafe_code = "forbid"`.

**No float arithmetic.** Prices, quantities and cash are decimal strings on the wire. The client never does arithmetic on them; the mock uses `rust_decimal`.

**No OS-blocking calls where a `may` runtime may run.** `PriceWhisperer` calls this client from `may` coroutines, where a blocking call stalls every coroutine on that worker. `clippy.toml` bans `std::net::TcpStream`, blocking `connect`, `to_socket_addrs`, `thread::sleep`, `JoinHandle::join`, `thread::scope`, `Condvar` and `mpsc::Receiver`, as in BRRTRouter and `PriceWhisperer`. Code that runs on its own OS thread opts out with an allow whose reason names that thread: `MockServer::start` and the tests. The client's own I/O is on `may` sockets (`MayTransport`, the stream session), so it parks a coroutine, never its worker; the one exception is name resolution through the operating system, which a strict caller replaces with a may-aware resolver (`MayTransport::with_client`).

**One HTTP client: `may_minihttp`.** Every HTTP request this repository makes goes through `may_minihttp`, the Microscaler fork that BRRTRouter and `PriceWhisperer` use: the REST transport (`MayTransport`), the stream's opening handshake (`HttpClient::upgrade`; `tungstenite` only frames messages on the upgraded connection, built without its `handshake` feature), and the tests' own calls to the mock. `scripts/check-http-clients.sh` fails when the client crate does not depend on `may_minihttp`, when its `tungstenite` has `handshake`, or when any crate in the workspace, tests and build scripts included, brings another HTTP client (`ureq`, `reqwest`, `isahc`, `curl`, hyper's client...). The mock is a server (axum) and is not covered. `may_minihttp` is pinned by release tag.

**Debuggable, never leaky.** Every public type implements `Debug`. Credential types print ids only, never secrets (tested).

**Generated code is generated.** `crates/connecttrade/src/generated/` comes from `cargo xtask codegen`. Its module carries the allows it needs, each with a reason; problems are fixed in the generator or the overlay, never by hand, and CI fails when the files drift from what the generator writes.

## Large files

- **No tracked file over 512 KiB.** Binaries, dumps and vendored blobs stay out of git. An exception needs a line in `.large-files-allow` (`path<TAB>reason`); there are none today. Connect Trade's spec, the largest file, is 243 KiB.
- **No hand-written Rust file over 500 lines.** A file that grows past it is split into modules by concern: the mock's REST layer is `rest/{platform,account,orders,checks,controls}.rs`, its book is `book/{mod,engine,render}.rs`, and the tests are one file per subject with a shared `tests/common`. Generated code is exempt; its shape is the generator's.

Both limits are variables at the top of `scripts/check-file-sizes.sh` (`MAX_BYTES`, `MAX_RUST_LINES`).

## Commits

[Conventional Commits](https://www.conventionalcommits.org): `feat(client): ...`, `fix(mock): ...`, `chore: ...`, checked by the `commit-msg` hook.

## Running the checks

```
pre-commit install                                   # once
pre-commit run --all-files --hook-stage pre-push     # everything CI runs, locally
```
