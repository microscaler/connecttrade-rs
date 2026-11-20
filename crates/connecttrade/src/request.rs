//! Sans-IO requests: what to send and how to read the answer, with no opinion on how bytes move.
//!
//! Every generated operation returns a [`Request<T>`]. [`Request::to_http`] turns it into an [`HttpRequest`] for a
//! base URL and a set of credentials; [`Request::parse`] turns the status and body that came back into `T` or an
//! [`Error`]. The [`Client`](crate::Client) does both around a [`Transport`](crate::Transport), but a caller with its
//! own I/O (PriceWhisperer's `may` sockets, an async runtime) can use these two halves directly.

use crate::auth::Credentials;
use crate::error::{ApiError, Error};
use serde::de::DeserializeOwned;
use std::marker::PhantomData;

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }

    /// True for calls that change something at the broker or at Connect Trade. A change whose answer was lost
    /// may have happened; it must never be retried blindly.
    pub fn is_change(self) -> bool {
        !matches!(self, Method::Get)
    }
}

/// Whose credentials an operation needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The platform's `client-id` and `client-secret` (or a platform token).
    Platform,
    /// The platform's credentials plus the end user's `user-id` and `user-secret` (or a user token).
    User,
}

/// Query string parameters, in order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query(Vec<(&'static str, String)>);

impl Query {
    pub fn push(&mut self, name: &'static str, value: String) {
        self.0.push((name, value));
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn pairs(&self) -> &[(&'static str, String)] {
        &self.0
    }

    fn render(&self) -> String {
        let mut s = String::new();
        for (i, (k, v)) in self.0.iter().enumerate() {
            s.push(if i == 0 { '?' } else { '&' });
            s.push_str(k);
            s.push('=');
            s.push_str(&encode_query(v));
        }
        s
    }
}

/// One call, ready to send. `T` is what a 2xx answer decodes to.
#[derive(Debug, Clone)]
pub struct Request<T> {
    /// The operation's id (`create_order`, `replace_order`, …): for logs and metrics.
    pub operation: &'static str,
    pub method: Method,
    /// Path without the query, with path parameters already encoded.
    pub path: String,
    pub query: Query,
    /// JSON body, when the operation has one.
    pub body: Option<Vec<u8>>,
    pub scope: Scope,
    _answer: PhantomData<fn() -> T>,
}

/// A request as bytes, for a transport.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub operation: &'static str,
    pub method: Method,
    /// Full URL, base plus path and query.
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Vec<u8>>,
}

/// What came back.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl<T: DeserializeOwned> Request<T> {
    #[doc(hidden)]
    pub fn new(
        operation: &'static str,
        method: Method,
        path: String,
        query: Query,
        body: Option<Vec<u8>>,
        scope: Scope,
    ) -> Self {
        Request { operation, method, path, query, body, scope, _answer: PhantomData }
    }

    /// Path and query, as sent.
    pub fn path_and_query(&self) -> String {
        format!("{}{}", self.path, self.query.render())
    }

    /// The bytes to send. Fails before anything is sent when the credentials do not fit the operation's scope.
    pub fn to_http(&self, base_url: &str, credentials: &Credentials) -> Result<HttpRequest, Error> {
        let mut headers = credentials
            .headers(self.scope)
            .map_err(|missing| Error::Credentials { operation: self.operation, missing })?;
        headers.push(("accept", "application/json".into()));
        if self.body.is_some() {
            headers.push(("content-type", "application/json".into()));
        }
        Ok(HttpRequest {
            operation: self.operation,
            method: self.method,
            url: format!("{}{}", base_url.trim_end_matches('/'), self.path_and_query()),
            headers,
            body: self.body.clone(),
        })
    }

    /// Read an answer: a 2xx body decodes to `T`; anything else is an [`Error::Api`] carrying Connect Trade's
    /// message and, when the message starts with one, its error code (`UNSUPPORTED_ORDER_CLASS`, …).
    ///
    /// A 2xx body that does not decode is [`Error::Decode`], which keeps the raw body: after a create or a replace
    /// the order exists even though we could not read the answer, and the caller must look it up, not retry.
    pub fn parse(&self, response: &HttpResponse) -> Result<T, Error> {
        if (200..300).contains(&response.status) {
            let body = if response.body.is_empty() { b"null".as_slice() } else { &response.body };
            serde_json::from_slice(body).map_err(|e| Error::Decode {
                operation: self.operation,
                status: response.status,
                reason: e.to_string(),
                body: String::from_utf8_lossy(&response.body).into_owned(),
            })
        } else {
            Err(Error::Api(ApiError::from_response(self.operation, response.status, &response.body)))
        }
    }
}

/// Percent-encode one path segment.
pub fn encode_path(s: &str) -> String {
    encode(s, |c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
}

fn encode_query(s: &str) -> String {
    // `,` stays: the API takes comma-separated symbol and type lists
    encode(s, |c| c.is_ascii_alphanumeric() || b"-._~,:".contains(&c))
}

fn encode(s: &str, keep: impl Fn(u8) -> bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if keep(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crypto_symbols_are_escaped_in_paths_and_queries() {
        assert_eq!(encode_path("BTC/USD"), "BTC%2FUSD");
        let mut q = Query::default();
        q.push("symbols", "AAPL,BTC/USD".into());
        q.push("sort", "entry_time:desc".into());
        assert_eq!(q.render(), "?symbols=AAPL,BTC%2FUSD&sort=entry_time:desc");
    }
}
