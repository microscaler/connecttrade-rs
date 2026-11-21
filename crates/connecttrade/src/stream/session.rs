//! The WebSocket client for the Streaming Broker API.
//!
//! HTTP is `may_minihttp`'s: it connects, does TLS and sends the opening handshake (an HTTP/1.1 upgrade), and hands
//! over the connection after `101 Switching Protocols`. `tungstenite` only frames messages on that connection; its
//! own HTTP client is not compiled in. Reads block until a message arrives (on `may` sockets, so a coroutine parks
//! rather than its worker): give a session a coroutine or thread of its own and hand updates on over a channel.

use super::{ClientMessage, ServerMessage, StreamAuth};
use may_minihttp::client::{HttpClient, Upgrade, Upgraded};
use std::time::{Duration, Instant};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

/// The GUID RFC 6455 appends to `Sec-WebSocket-Key` before hashing it into `Sec-WebSocket-Accept`.
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Why a stream session failed.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The WebSocket or the TLS under it failed.
    #[error("websocket: {0}")]
    Socket(Box<tungstenite::Error>),
    /// Connecting, TLS or the opening handshake failed, or the socket refused a setting (a read timeout).
    #[error("socket: {0}")]
    Io(#[from] std::io::Error),
    /// The server did not switch to WebSocket: a final HTTP status instead of 101, or a 101 that does not prove it
    /// read our key.
    #[error("websocket handshake refused: {0}")]
    Handshake(String),
    /// A message could not be encoded; nothing was sent.
    #[error("message did not encode: {0}")]
    Encode(String),
    #[error("authentication refused: {0}")]
    AuthRefused(String),
    #[error("subscription refused: {0}")]
    SubscriptionRefused(String),
    #[error("unexpected message during {stage}: {message}")]
    Protocol { stage: &'static str, message: String },
    #[error("no answer during {0}")]
    Timeout(&'static str),
    #[error("closed by the server: {0}")]
    Closed(String),
    #[error("bad message: {0}")]
    Decode(String),
}

impl From<tungstenite::Error> for StreamError {
    fn from(e: tungstenite::Error) -> Self {
        StreamError::Socket(Box::new(e))
    }
}

/// Everything needed to open a session, and to open it again after a drop.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// `wss://stream.connecttrade.com`, or a mock's `ws://` URL.
    pub url: String,
    pub auth: StreamAuth,
    pub topics: Vec<super::Topic>,
    /// Users to subscribe (platform sessions). Ignored for a user-token session.
    pub users: Vec<String>,
    /// Bound on each step of the handshake.
    pub handshake_timeout: Duration,
}

impl StreamConfig {
    pub fn new(url: impl Into<String>, auth: StreamAuth) -> Self {
        StreamConfig {
            url: url.into(),
            auth,
            topics: vec![super::Topic::Orders, super::Topic::Positions, super::Topic::Balances],
            users: vec![],
            handshake_timeout: Duration::from_secs(10),
        }
    }

    /// Connect, authenticate, subscribe to the topics and every user, and wait for each acknowledgement.
    ///
    /// # Errors
    ///
    /// [`StreamError::Io`] when the connection fails; [`StreamError::Handshake`] when the server does not switch to
    /// WebSocket; [`StreamError::AuthRefused`] or
    /// [`StreamError::SubscriptionRefused`] when Connect Trade says no; [`StreamError::Timeout`] when a step gets no
    /// answer within `handshake_timeout`; [`StreamError::Protocol`] for an unexpected message.
    pub fn connect(&self) -> Result<StreamSession, StreamError> {
        let ws = open(&self.url, self.handshake_timeout)?;
        let mut s = StreamSession { ws, session_id: None, user_id: None };

        match s.read_message("welcome")? {
            ServerMessage::Welcome { session_id, .. } => s.session_id = Some(session_id),
            other => return Err(protocol("welcome", &other)),
        }

        let auth = match &self.auth {
            StreamAuth::Client { client_id, client_secret } => ClientMessage::Authenticate {
                client_id: Some(client_id.clone()),
                client_secret: Some(client_secret.clone()),
                access_token: None,
            },
            StreamAuth::Token(t) => {
                ClientMessage::Authenticate { client_id: None, client_secret: None, access_token: Some(t.clone()) }
            }
        };
        s.send(&auth)?;
        match s.read_message("authenticate")? {
            ServerMessage::AuthSuccess { user_id, .. } => s.user_id = user_id,
            ServerMessage::AuthError { message } => return Err(StreamError::AuthRefused(message.unwrap_or_default())),
            other => return Err(protocol("authenticate", &other)),
        }

        s.send(&ClientMessage::Subscribe { topics: self.topics.clone() })?;
        match s.read_message("subscribe")? {
            ServerMessage::Subscribed { .. } => {}
            ServerMessage::SubscriptionFailed { message } => {
                return Err(StreamError::SubscriptionRefused(message.unwrap_or_default()))
            }
            other => return Err(protocol("subscribe", &other)),
        }

        if s.user_id.is_none() {
            for user in &self.users {
                s.send(&ClientMessage::SubscribeUser { user_id: user.clone() })?;
                // updates for users already subscribed may arrive before this acknowledgement; none are expected
                // this early, but anything else is a protocol error
                match s.read_message("subscribe_user")? {
                    ServerMessage::UserSubscribed { user_id } if &user_id == user => {}
                    ServerMessage::SubscriptionFailed { message } => {
                        return Err(StreamError::SubscriptionRefused(format!(
                            "{user}: {}",
                            message.unwrap_or_default()
                        )))
                    }
                    other => return Err(protocol("subscribe_user", &other)),
                }
            }
        }
        s.set_read_timeout(None)?;
        Ok(s)
    }
}

/// Connect to `url` (`ws://` or `wss://`) and do the WebSocket opening handshake (RFC 6455 §4.1) over
/// `may_minihttp`. The connection keeps `timeout` as its read and write timeout.
fn open(url: &str, timeout: Duration) -> Result<WebSocket<Upgraded>, StreamError> {
    let (http_url, path) = http_url(url)?;
    let mut client = HttpClient::from_url(&http_url)?;
    client.set_timeout(Some(timeout));
    let key = new_key()?;
    let mut request =
        client.new_request(http::Method::GET, path.parse().map_err(|e| StreamError::Handshake(format!("{url}: {e}")))?);
    let headers = request.headers_mut();
    headers.insert(http::header::UPGRADE, http::HeaderValue::from_static("websocket"));
    headers.insert(http::header::CONNECTION, http::HeaderValue::from_static("Upgrade"));
    headers.insert(http::header::SEC_WEBSOCKET_VERSION, http::HeaderValue::from_static("13"));
    headers.insert(
        http::header::SEC_WEBSOCKET_KEY,
        http::HeaderValue::from_str(&key).map_err(|e| StreamError::Handshake(e.to_string()))?,
    );
    match client.upgrade(request)? {
        Upgrade::Switched { response, stream } => {
            check_switched(response.headers(), &key)?;
            Ok(WebSocket::from_raw_socket(stream, Role::Client, None))
        }
        Upgrade::Refused(response) => Err(StreamError::Handshake(format!("HTTP {}", response.status().as_u16()))),
    }
}

/// `ws://host/path` → (`http://host/path`, `/path`); `wss://` → `https://`.
fn http_url(url: &str) -> Result<(String, String), StreamError> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("wss://") {
        ("https://", rest)
    } else if let Some(rest) = url.strip_prefix("ws://") {
        ("http://", rest)
    } else {
        return Err(StreamError::Handshake(format!("{url}: not a ws:// or wss:// URL")));
    };
    let path = rest.find(['/', '?']).and_then(|i| rest.get(i..)).unwrap_or("/");
    let path = if path.starts_with('?') { format!("/{path}") } else { path.to_string() };
    Ok((format!("{scheme}{rest}"), path))
}

/// Sixteen random bytes, base64: the handshake's nonce.
fn new_key() -> Result<String, StreamError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| StreamError::Handshake(format!("no randomness for the key: {e}")))?;
    Ok(data_encoding::BASE64.encode(&nonce))
}

/// What the client must check of a 101 (RFC 6455 §4.1): the upgrade is to `websocket`, and `Sec-WebSocket-Accept`
/// is base64(SHA-1(key + GUID)).
fn check_switched(headers: &http::HeaderMap, key: &str) -> Result<(), StreamError> {
    let upgrade = headers.get(http::header::UPGRADE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    if !upgrade.eq_ignore_ascii_case("websocket") {
        return Err(StreamError::Handshake(format!("upgraded to {upgrade:?}, not websocket")));
    }
    let accept = headers.get(http::header::SEC_WEBSOCKET_ACCEPT).map(http::HeaderValue::as_bytes);
    if accept != Some(accept_for(key).as_bytes()) {
        return Err(StreamError::Handshake("Sec-WebSocket-Accept does not match our key".into()));
    }
    Ok(())
}

fn accept_for(key: &str) -> String {
    use sha1::Digest;
    let mut sha = sha1::Sha1::new();
    sha.update(key.as_bytes());
    sha.update(WEBSOCKET_GUID.as_bytes());
    data_encoding::BASE64.encode(&sha.finalize())
}

fn protocol(stage: &'static str, m: &ServerMessage) -> StreamError {
    StreamError::Protocol { stage, message: format!("{m:?}") }
}

/// An open, authenticated, subscribed session.
pub struct StreamSession {
    ws: WebSocket<Upgraded>,
    session_id: Option<String>,
    user_id: Option<String>,
}

impl std::fmt::Debug for StreamSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSession")
            .field("session_id", &self.session_id)
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

impl StreamSession {
    /// Connect Trade's id for this session; quote it to their support.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The user a user-token session is locked to.
    #[must_use]
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Bound how long [`next_message`](Self::next_message) waits (and a send, which only waits when the socket is
    /// full). `None` waits for ever.
    ///
    /// # Errors
    ///
    /// [`StreamError::Io`] when the socket refuses the setting.
    pub fn set_read_timeout(&mut self, t: Option<Duration>) -> Result<(), StreamError> {
        self.ws.get_ref().set_timeout(t)?;
        Ok(())
    }

    /// Send one message.
    ///
    /// # Errors
    ///
    /// [`StreamError::Encode`] when the message does not encode; [`StreamError::Socket`] when sending fails.
    pub fn send(&mut self, m: &ClientMessage) -> Result<(), StreamError> {
        let text = serde_json::to_string(m).map_err(|e| StreamError::Encode(e.to_string()))?;
        self.ws.send(Message::text(text))?;
        Ok(())
    }

    /// Send a `ping`; the `pong` comes back through [`next_message`](Self::next_message).
    ///
    /// # Errors
    ///
    /// As [`StreamSession::send`].
    pub fn ping(&mut self, seq: u64) -> Result<(), StreamError> {
        self.send(&ClientMessage::Ping { seq: Some(seq) })
    }

    /// Ask for one more user's updates (platform sessions); the answer arrives as a message.
    ///
    /// # Errors
    ///
    /// As [`StreamSession::send`].
    pub fn subscribe_user(&mut self, user_id: &str) -> Result<(), StreamError> {
        self.send(&ClientMessage::SubscribeUser { user_id: user_id.into() })
    }

    /// Stop one user's updates; the answer arrives as a message.
    ///
    /// # Errors
    ///
    /// As [`StreamSession::send`].
    pub fn unsubscribe_user(&mut self, user_id: &str) -> Result<(), StreamError> {
        self.send(&ClientMessage::UnsubscribeUser { user_id: user_id.into() })
    }

    /// The next server message. A read timeout (see [`set_read_timeout`](Self::set_read_timeout)) returns
    /// [`StreamError::Timeout`] and leaves the session usable.
    ///
    /// # Errors
    ///
    /// [`StreamError::Timeout`], [`StreamError::Closed`], [`StreamError::Socket`] or [`StreamError::Decode`].
    pub fn next_message(&mut self) -> Result<ServerMessage, StreamError> {
        self.read_message("read")
    }

    /// Send a ping and wait for its pong, passing any updates that arrive first to `on_update`. Measures the
    /// round trip.
    ///
    /// # Errors
    ///
    /// [`StreamError::Timeout`] when no pong comes back in time; otherwise as [`StreamSession::next_message`].
    pub fn round_trip(
        &mut self,
        seq: u64,
        timeout: Duration,
        mut on_update: impl FnMut(ServerMessage),
    ) -> Result<Duration, StreamError> {
        let start = Instant::now();
        self.ping(seq)?;
        self.set_read_timeout(Some(timeout))?;
        let result = loop {
            if start.elapsed() > timeout {
                break Err(StreamError::Timeout("ping"));
            }
            match self.read_message("ping") {
                Ok(ServerMessage::Pong { seq: Some(s) }) if s == seq => break Ok(start.elapsed()),
                Ok(other) => on_update(other),
                Err(e) => break Err(e),
            }
        };
        self.set_read_timeout(None)?;
        result
    }

    /// Close the session politely.
    ///
    /// # Errors
    ///
    /// [`StreamError::Socket`] when the close frame cannot be sent (the socket is dropped either way).
    pub fn close(mut self) -> Result<(), StreamError> {
        self.ws.close(None)?;
        self.ws.flush()?;
        Ok(())
    }

    fn read_message(&mut self, stage: &'static str) -> Result<ServerMessage, StreamError> {
        loop {
            let msg = match self.ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    return Err(StreamError::Timeout(stage))
                }
                Err(e) => return Err(e.into()),
            };
            match msg {
                Message::Text(t) => {
                    return serde_json::from_str(t.as_str()).map_err(|e| StreamError::Decode(format!("{e}: {t}")))
                }
                Message::Binary(b) => {
                    return serde_json::from_slice(&b).map_err(|e| StreamError::Decode(e.to_string()))
                }
                Message::Close(c) => {
                    return Err(StreamError::Closed(
                        c.map(|c| format!("{} {}", u16::from(c.code), c.reason)).unwrap_or_default(),
                    ))
                }
                // tungstenite answers protocol pings itself
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_matches_the_rfc_example() {
        // RFC 6455 §1.3
        assert_eq!(accept_for("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn ws_urls_map_to_http() {
        assert_eq!(
            http_url("wss://stream.connecttrade.com").unwrap(),
            ("https://stream.connecttrade.com".into(), "/".into())
        );
        assert_eq!(
            http_url("ws://127.0.0.1:8098/stream?x=1").unwrap(),
            ("http://127.0.0.1:8098/stream?x=1".into(), "/stream?x=1".into())
        );
        assert_eq!(http_url("ws://h:1?x=1").unwrap().1, "/?x=1");
        assert!(matches!(http_url("https://h/"), Err(StreamError::Handshake(_))));
    }

    #[test]
    fn keys_are_fresh_sixteen_byte_nonces() {
        let (a, b) = (new_key().unwrap(), new_key().unwrap());
        assert_ne!(a, b);
        assert_eq!(data_encoding::BASE64.decode(a.as_bytes()).unwrap().len(), 16);
    }

    #[test]
    fn a_101_must_prove_it_read_the_key() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut h = http::HeaderMap::new();
        h.insert(http::header::UPGRADE, http::HeaderValue::from_static("WebSocket"));
        assert!(check_switched(&h, key).is_err());
        h.insert(http::header::SEC_WEBSOCKET_ACCEPT, http::HeaderValue::from_static("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
        check_switched(&h, key).unwrap();
        h.insert(http::header::UPGRADE, http::HeaderValue::from_static("h2c"));
        assert!(check_switched(&h, key).is_err());
    }
}
