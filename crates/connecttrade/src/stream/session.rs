//! The blocking WebSocket client for the Streaming Broker API.
//!
//! It holds a `std::net::TcpStream` and blocks its thread on every read, by design: run a session on an OS thread
//! of its own (never a `may` coroutine) and hand updates to the rest of the program over a channel.
#![allow(
    clippy::disallowed_types,
    reason = "a stream session runs on its own OS thread, never on a may worker; see the module docs"
)]

use super::{ClientMessage, ServerMessage, StreamAuth};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Why a stream session failed.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The WebSocket or the TLS under it failed.
    #[error("websocket: {0}")]
    Socket(Box<tungstenite::Error>),
    /// The socket refused a setting (a read timeout).
    #[error("socket: {0}")]
    Io(#[from] std::io::Error),
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
    /// [`StreamError::Socket`] when the connection fails; [`StreamError::AuthRefused`] or
    /// [`StreamError::SubscriptionRefused`] when Connect Trade says no; [`StreamError::Timeout`] when a step gets no
    /// answer within `handshake_timeout`; [`StreamError::Protocol`] for an unexpected message.
    pub fn connect(&self) -> Result<StreamSession, StreamError> {
        let (ws, _) = tungstenite::connect(self.url.as_str())?;
        let mut s = StreamSession { ws, session_id: None, user_id: None };
        s.set_read_timeout(Some(self.handshake_timeout))?;

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

fn protocol(stage: &'static str, m: &ServerMessage) -> StreamError {
    StreamError::Protocol { stage, message: format!("{m:?}") }
}

/// An open, authenticated, subscribed session.
pub struct StreamSession {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
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

    /// Bound how long [`next_message`](Self::next_message) waits. `None` waits for ever.
    ///
    /// # Errors
    ///
    /// [`StreamError::Io`] when the socket refuses the setting.
    pub fn set_read_timeout(&mut self, t: Option<Duration>) -> Result<(), StreamError> {
        match self.ws.get_mut() {
            MaybeTlsStream::Plain(s) => s.set_read_timeout(t)?,
            MaybeTlsStream::Rustls(s) => s.get_mut().set_read_timeout(t)?,
            _ => {}
        }
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
    #[test]
    fn a_tls_config_builds_with_one_crypto_provider() {
        // fails if two rustls providers end up compiled in and none is chosen (a panic in production on first wss)
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        assert_eq!(config.alpn_protocols, Vec::<Vec<u8>>::new());
    }
}
