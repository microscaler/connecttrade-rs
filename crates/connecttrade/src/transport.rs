//! Moving bytes. One blocking trait, so the same client runs on an OS thread, a `may` coroutine (with a transport
//! built on `may` sockets) or behind `spawn_blocking`.

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

#[cfg(feature = "ureq")]
pub use self::ureq_transport::UreqTransport;

#[cfg(feature = "ureq")]
mod ureq_transport {
    use super::{HttpRequest, HttpResponse, Transport, TransportError};
    use std::time::Duration;

    /// Blocking HTTPS over `ureq` and rustls, with connection reuse.
    ///
    /// It blocks the calling OS thread for the whole round trip: call it from a thread of its own, never from a
    /// `may` coroutine (bring a transport on `may` sockets there).
    pub struct UreqTransport {
        agent: ureq::Agent,
    }

    impl std::fmt::Debug for UreqTransport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("UreqTransport").finish_non_exhaustive()
        }
    }

    impl UreqTransport {
        /// `connect` bounds resolve, connect and TLS (nothing sent yet); `total` bounds the whole call.
        #[must_use]
        pub fn new(connect: Duration, total: Duration) -> Self {
            let config = ureq::Agent::config_builder()
                .timeout_connect(Some(connect))
                .timeout_global(Some(total))
                .http_status_as_error(false)
                .user_agent(concat!("connecttrade-rs/", env!("CARGO_PKG_VERSION")))
                .build();
            UreqTransport { agent: config.into() }
        }
    }

    impl Default for UreqTransport {
        fn default() -> Self {
            UreqTransport::new(Duration::from_secs(3), Duration::from_secs(10))
        }
    }

    impl Transport for UreqTransport {
        fn send(&self, r: &HttpRequest) -> Result<HttpResponse, TransportError> {
            let mut b = ureq::http::Request::builder().method(r.method.as_str()).uri(&r.url);
            for (k, v) in &r.headers {
                b = b.header(*k, v);
            }
            let req = b.body(r.body.clone().unwrap_or_default()).map_err(|e| TransportError::NotSent(e.to_string()))?;
            let mut resp = self.agent.run(req).map_err(|e| classify(&e))?;
            let status = resp.status().as_u16();
            let body = resp
                .body_mut()
                .with_config()
                .limit(32 * 1024 * 1024)
                .read_to_vec()
                .map_err(|e| TransportError::Broken(e.to_string()))?;
            Ok(HttpResponse { status, body })
        }
    }

    fn classify(e: &ureq::Error) -> TransportError {
        use ureq::Error as E;
        use ureq::Timeout;
        match e {
            E::HostNotFound
            | E::ConnectionFailed
            | E::BadUri(_)
            | E::Tls(_)
            | E::Rustls(_)
            | E::Timeout(Timeout::Connect | Timeout::Resolve) => TransportError::NotSent(e.to_string()),
            E::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => TransportError::NotSent(e.to_string()),
            E::Timeout(_) => TransportError::Timeout(e.to_string()),
            _ => TransportError::Broken(e.to_string()),
        }
    }
}
