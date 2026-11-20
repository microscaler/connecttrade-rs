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

impl<T: Transport> Client<T> {
    pub fn new(base_url: impl Into<String>, transport: T) -> Self {
        Client { base_url: base_url.into(), transport }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Send one request with these credentials and decode the answer.
    pub fn send<R: DeserializeOwned>(&self, credentials: &Credentials, request: Request<R>) -> Result<R, Error> {
        let http = request.to_http(&self.base_url, credentials)?;
        let response = self.transport.send(&http)?;
        request.parse(&response)
    }
}
