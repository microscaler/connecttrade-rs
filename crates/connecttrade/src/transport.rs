//! Moving bytes. One blocking trait, so the same client runs on a `may` coroutine, an OS thread or behind
//! `spawn_blocking`.

use crate::error::TransportError;
use crate::request::{HttpRequest, HttpResponse};

/// Sends one request and returns the status and body. Implementations must report failures with the right
/// [`TransportError`] variant: whether the request may have been sent decides whether an order path can retry.
pub trait Transport: Send + Sync {
    /// Send `request` and return the answer, whatever its status.
    ///
    /// # Errors
    ///
    /// [`TransportError::NotSent`] when nothing reached the server; [`TransportError::Timeout`] or
    /// [`TransportError::Broken`] when the request may have been received.
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError>;
}

impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, TransportError> {
        (**self).send(request)
    }
}

#[cfg(feature = "transport")]
pub use self::may_transport::MayTransport;

#[cfg(feature = "transport")]
mod may_transport {
    use super::{HttpRequest, HttpResponse, Transport, TransportError};
    use may_minihttp::client::{Client, ClientError, ClientErrorKind, RedirectPolicy};
    use std::io;
    use std::time::Duration;

    /// The largest answer read (a long order history); anything bigger is a broken answer, not a body.
    const MAX_RESPONSE_BODY: usize = 32 * 1024 * 1024;

    /// HTTPS over the `may_minihttp` client (rustls, pooled connections, `may` sockets).
    ///
    /// Sockets park the calling coroutine, not the worker thread, so it can be called from a `may` coroutine as well
    /// as from an OS thread. Name resolution is the client's resolver: the operating system's by default, which can
    /// block a worker on a cache miss; a strict `may` caller builds its own client with a may-aware resolver and
    /// passes it to [`MayTransport::with_client`].
    pub struct MayTransport {
        client: Client,
    }

    impl std::fmt::Debug for MayTransport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("MayTransport").finish_non_exhaustive()
        }
    }

    impl MayTransport {
        /// `connect` bounds resolve, connect and TLS (nothing sent yet); `total` bounds the whole call. Redirects are
        /// never followed: Connect Trade does not send them, and a followed redirect would resend a change.
        ///
        /// # Errors
        ///
        /// When the TLS configuration (the platform's certificate verifier) cannot be built.
        pub fn new(connect: Duration, total: Duration) -> io::Result<Self> {
            let client = Client::builder()
                .connect_timeout(connect)
                .io_timeout(total)
                .request_timeout(total)
                .max_response_body(MAX_RESPONSE_BODY)
                .redirect_policy(RedirectPolicy::None)
                .build()?;
            Ok(MayTransport { client })
        }

        /// Use a client configured by the caller (resolver, TLS roots, pool sizes, observers).
        #[must_use]
        pub fn with_client(client: Client) -> Self {
            MayTransport { client }
        }

        /// Three seconds to connect, ten for the whole call.
        ///
        /// # Errors
        ///
        /// As [`MayTransport::new`].
        pub fn with_default_timeouts() -> io::Result<Self> {
            MayTransport::new(Duration::from_secs(3), Duration::from_secs(10))
        }
    }

    impl Transport for MayTransport {
        fn send(&self, r: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let method = http::Method::from_bytes(r.method.as_str().as_bytes())
                .map_err(|e| TransportError::NotSent(e.to_string()))?;
            let mut b = self.client.request(method, &r.url).map_err(|e| not_sent(&e))?;
            b = b
                .header_str("user-agent", concat!("connecttrade-rs/", env!("CARGO_PKG_VERSION")))
                .map_err(|e| not_sent(&e))?;
            for (k, v) in &r.headers {
                b = b.header_str(k, v).map_err(|e| not_sent(&e))?;
            }
            if let Some(body) = &r.body {
                b = b.body(body.clone());
            }
            if r.method.is_change() {
                // a change reaches the server at most once: the pool never resends it on its own after a stale
                // connection, even a DELETE (#12)
                b = b.stale_retry(false);
            }
            let resp = b.send_typed().map_err(|e| classify(&e, r.method.is_change()))?;
            let headers = resp
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
                .collect();
            Ok(HttpResponse { status: resp.status().as_u16(), headers, body: resp.into_body() })
        }
    }

    fn not_sent(e: &io::Error) -> TransportError {
        TransportError::NotSent(e.to_string())
    }

    /// Conservative: only failures that end before the request could be written are `NotSent`. Everything else may
    /// have reached Connect Trade. A TLS failure cannot be placed before or after the request was written (the
    /// client labels it by its text), so for a change it is `Broken` - may have happened (#13); a read keeps
    /// `NotSent`.
    fn classify(e: &ClientError, change: bool) -> TransportError {
        let refused = std::error::Error::source(e)
            .and_then(|s| s.downcast_ref::<io::Error>())
            .is_some_and(|io| io.kind() == io::ErrorKind::ConnectionRefused);
        match e.kind() {
            // the request was refused before it was built, or the server could not be found or reached
            ClientErrorKind::Tls if change => TransportError::Broken(e.to_string()),
            ClientErrorKind::InvalidRequest
            | ClientErrorKind::Metadata
            | ClientErrorKind::Dns
            | ClientErrorKind::Tls => TransportError::NotSent(e.to_string()),
            ClientErrorKind::Connection if refused => TransportError::NotSent(e.to_string()),
            ClientErrorKind::Timeout => TransportError::Timeout(e.to_string()),
            _ => TransportError::Broken(e.to_string()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn classify_change(e: &ClientError) -> TransportError {
            classify(e, true)
        }

        #[test]
        fn a_tls_failure_on_a_change_may_have_happened_and_on_a_read_was_not_sent() {
            let tls = || ClientError::from(io::Error::other("tls: received fatal alert: InternalError"));
            assert_eq!(tls().kind(), ClientErrorKind::Tls);
            assert!(matches!(classify(&tls(), true), TransportError::Broken(_)));
            assert!(matches!(classify(&tls(), false), TransportError::NotSent(_)));
        }

        fn err(kind: io::ErrorKind, message: &str) -> ClientError {
            ClientError::from(io::Error::new(kind, message.to_string()))
        }

        #[test]
        fn only_failures_before_sending_are_not_sent() {
            assert!(matches!(
                classify_change(&err(io::ErrorKind::ConnectionRefused, "refused")),
                TransportError::NotSent(_)
            ));
            assert!(matches!(
                classify_change(&err(io::ErrorKind::AddrNotAvailable, "no address")),
                TransportError::NotSent(_)
            ));
            assert!(matches!(
                classify_change(&err(io::ErrorKind::InvalidInput, "bad url")),
                TransportError::NotSent(_)
            ));
            assert!(matches!(classify_change(&err(io::ErrorKind::TimedOut, "read")), TransportError::Timeout(_)));
            assert!(matches!(
                classify_change(&err(io::ErrorKind::ConnectionReset, "reset")),
                TransportError::Broken(_)
            ));
            assert!(matches!(classify_change(&err(io::ErrorKind::UnexpectedEof, "eof")), TransportError::Broken(_)));
            assert!(matches!(classify_change(&err(io::ErrorKind::BrokenPipe, "pipe")), TransportError::Broken(_)));
        }
    }
}
