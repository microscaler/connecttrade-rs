//! Client for [Connect Trade](https://connecttrade.com), the broker connectivity API: one normalised API in front of
//! `TradeZero`, tastytrade, Tradier, Lightspeed and other brokers.
//!
//! - [`models`] and [`ops`] are generated from Connect Trade's `OpenAPI` spec (`spec/openapi.json`) and our overlay
//!   (`spec/overlay.json`) by `cargo xtask codegen`. Never edit them by hand.
//! - Every operation returns a sans-IO [`Request`]: build it, send it with any [`Transport`], parse the answer.
//!   [`Client`] does the three steps with the built-in [`MayTransport`] (`may_minihttp`) or your own.
//! - [`stream`] is the Streaming Broker API (orders, positions, balances over WebSocket), which the spec does not
//!   describe; it is written by hand from Connect Trade's docs.
//!
//! Errors keep the one distinction an order path needs: whether a failed change may still have happened
//! ([`Error::may_have_happened`]).

#![forbid(unsafe_code)]

pub mod auth;
mod client;
pub mod error;
pub mod request;
pub mod stream;
pub mod transport;

#[allow(
    clippy::all,
    clippy::pedantic,
    missing_docs,
    reason = "generated from the spec by `cargo xtask codegen`; fixed in the generator, never by hand"
)]
mod generated {
    pub mod models;
    pub mod operations;
}

/// Request and response types, generated from the spec.
pub mod models {
    pub use crate::generated::models::*;
}

/// One function per endpoint, generated from the spec.
pub mod ops {
    pub use crate::generated::operations::*;
}

pub use auth::{Credentials, PlatformCredentials, UserCredentials};
pub use client::{Client, PRODUCTION_URL, STREAM_URL};
pub use error::{ApiError, Error, ErrorCode, TransportError};
pub use request::{HttpRequest, HttpResponse, Method, Request, Scope};
#[cfg(feature = "transport")]
pub use transport::MayTransport;
pub use transport::Transport;
