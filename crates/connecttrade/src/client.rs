//! A client: a base URL, credentials and a transport around the generated operations.

use crate::auth::Credentials;
use crate::error::Error;
use crate::request::Request;
use crate::transport::Transport;
use serde::de::DeserializeOwned;

/// Connect Trade's REST API.
pub const PRODUCTION_URL: &str = "https://api.connecttrade.com";
/// Connect Trade's order, position and balance stream.
pub const STREAM_URL: &str = "wss://stream.connecttrade.com";

/// Sends generated [`Request`]s.
///
/// ```no_run
/// use connecttrade::{Client, Credentials, ops, models::*};
/// let platform = Credentials::platform("client-id", "client-secret");
/// let client = Client::new(connecttrade::PRODUCTION_URL, connecttrade::UreqTransport::default());
/// let user = platform.for_user("user-1", "user-secret").unwrap();
/// let accounts = client.send(&user, ops::list_accounts(&Default::default()))?;
/// # Ok::<(), connecttrade::Error>(())
/// ```
pub struct Client<T: Transport> {
    base_url: String,
    transport: T,
}

impl<T: Transport> std::fmt::Debug for Client<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

impl<T: Transport> Client<T> {
    /// A client for `base_url` (`PRODUCTION_URL`, or a mock's address) sending through `transport`.
    pub fn new(base_url: impl Into<String>, transport: T) -> Self {
        Client { base_url: base_url.into(), transport }
    }

    /// Where requests go.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Send one request with these credentials and decode the answer.
    ///
    /// # Errors
    ///
    /// [`Error::Credentials`] and [`Error::Encode`] before anything is sent; [`Error::Transport`] when the request
    /// did not complete; [`Error::Api`] for a non-2xx answer; [`Error::Decode`] for a 2xx answer that does not
    /// decode. [`Error::may_have_happened`] says whether a failed change may still have taken effect.
    pub fn send<R: DeserializeOwned>(&self, credentials: &Credentials, request: Request<R>) -> Result<R, Error> {
        let (http, decoder) = request.into_http(&self.base_url, credentials)?;
        let response = self.transport.send(&http)?;
        decoder.parse(&response)
    }
}
