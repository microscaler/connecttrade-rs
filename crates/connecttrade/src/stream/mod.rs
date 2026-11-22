//! The Streaming Broker API: order, position and balance updates over WebSocket (`wss://stream.connecttrade.com`).
//!
//! Not in the `OpenAPI` spec; written from Connect Trade's docs ("Streaming Broker API"). The flow:
//! 1. connect; the server sends `welcome` with a `session_id` (quote it to Connect Trade support);
//! 2. `authenticate` within 30 seconds, with client credentials or an access token (never both);
//! 3. `subscribe` to topics; 4. `subscribe_user` per user (platform sessions only; at most 500 per session);
//! 5. read updates. Updates are **incremental**: one order, one account's positions, one balance. Take a REST
//!    snapshot after every (re)connect and apply updates on top.
//!
//! Limits from the docs: 10 platform sessions per `client_id`; 5 user-token sessions per user. The server never
//! pings; [`StreamSession::ping`] is how to check the connection is alive.
//!
//! The message types are always available (a mock server uses them); the blocking client, [`StreamSession`],
//! needs the `stream` feature. It is blocking. Run it on its own thread and hand updates to the rest of the program over a
//! channel; [`StreamConfig::connect`] again after any error, which restores the topics and users.

use crate::models::{BalanceResponse, OrderResponse, PositionResponse};
use serde::{Deserialize, Serialize};

#[cfg(feature = "stream")]
mod session;
#[cfg(feature = "stream")]
pub use session::{StreamConfig, StreamError, StreamSession};

/// What a session receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Topic {
    Orders,
    Positions,
    Balances,
}

/// How a session authenticates.
#[derive(Clone)]
pub enum StreamAuth {
    /// The platform's client credentials (server side). Needs `subscribe_user` per user.
    Client { client_id: String, client_secret: crate::auth::Secret },
    /// A platform token (acts like client credentials) or a user token (locked to its user, subscribed already).
    Token(crate::auth::Secret),
}

impl std::fmt::Debug for StreamAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamAuth::Client { client_id, .. } => {
                f.debug_struct("Client").field("client_id", client_id).finish_non_exhaustive()
            }
            StreamAuth::Token(_) => f.write_str("Token(..)"),
        }
    }
}

/// Messages a client sends. Its `Debug` never shows a credential (#9).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ClientMessage {
    Authenticate {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        client_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        client_secret: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        access_token: Option<String>,
    },
    Subscribe {
        topics: Vec<Topic>,
    },
    SubscribeUser {
        user_id: String,
    },
    UnsubscribeUser {
        user_id: String,
    },
    Ping {
        #[serde(skip_serializing_if = "Option::is_none", default)]
        seq: Option<u64>,
    },
}

impl std::fmt::Debug for ClientMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientMessage::Authenticate { client_id, client_secret, access_token } => f
                .debug_struct("Authenticate")
                .field("client_id", client_id)
                .field("client_secret", &client_secret.as_ref().map(|_| "<redacted>"))
                .field("access_token", &access_token.as_ref().map(|_| "<redacted>"))
                .finish(),
            ClientMessage::Subscribe { topics } => f.debug_struct("Subscribe").field("topics", topics).finish(),
            ClientMessage::SubscribeUser { user_id } => {
                f.debug_struct("SubscribeUser").field("user_id", user_id).finish()
            }
            ClientMessage::UnsubscribeUser { user_id } => {
                f.debug_struct("UnsubscribeUser").field("user_id", user_id).finish()
            }
            ClientMessage::Ping { seq } => f.debug_struct("Ping").field("seq", seq).finish(),
        }
    }
}

/// Messages the server sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServerMessage {
    Welcome {
        session_id: String,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        timeout_seconds: Option<u64>,
    },
    AuthSuccess {
        #[serde(default)]
        message: Option<String>,
        /// Set for a user-token session.
        #[serde(default)]
        user_id: Option<String>,
    },
    AuthError {
        #[serde(default)]
        message: Option<String>,
    },
    Subscribed {
        #[serde(default)]
        subscription_id: Option<String>,
        #[serde(default)]
        topics: Vec<String>,
    },
    SubscriptionFailed {
        #[serde(default)]
        message: Option<String>,
    },
    UserSubscribed {
        user_id: String,
    },
    UserUnsubscribed {
        user_id: String,
    },
    Pong {
        #[serde(default)]
        seq: Option<u64>,
    },
    /// One order that changed.
    Orders {
        data: Box<OrderResponse>,
    },
    /// All positions of the account that changed.
    Positions {
        data: Vec<PositionResponse>,
    },
    /// One account's balance.
    Balances {
        data: BalanceResponse,
    },
    Error {
        #[serde(default)]
        message: Option<String>,
    },
    /// A message that is JSON but did not decode as its type (#15). The session goes on; `raw` is the message with
    /// any credential redacted, at most 4 KiB.
    #[serde(skip)]
    Undecoded {
        /// Its `type`, when it has one.
        type_: Option<String>,
        raw: String,
        error: String,
    },
    /// A message type this crate does not know yet.
    #[serde(other)]
    Unknown,
}

/// A message's text as a [`ServerMessage`]: one that is JSON but not of its type's shape is
/// [`ServerMessage::Undecoded`], so one odd update never ends a session (#15). `None`: not JSON at all.
#[must_use]
pub fn decode(text: &[u8]) -> Option<ServerMessage> {
    match serde_json::from_slice::<ServerMessage>(text) {
        Ok(m) => Some(m),
        Err(e) => {
            let v = serde_json::from_slice::<serde_json::Value>(text).ok()?;
            let mut raw = crate::redact::body("stream", text).text;
            if raw.len() > 4096 {
                let mut end = 4096;
                while !raw.is_char_boundary(end) {
                    end -= 1;
                }
                raw.truncate(end);
                raw.push('…');
            }
            Some(ServerMessage::Undecoded {
                type_: v.get("type").and_then(|t| t.as_str()).map(str::to_string),
                raw,
                error: e.to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_docs_messages_decode() {
        let welcome = r#"{"type":"welcome","session_id":"1e04d508","message":"Connected","timeout_seconds":30,"required_steps":["1"]}"#;
        assert!(matches!(serde_json::from_str(welcome).unwrap(), ServerMessage::Welcome { .. }));
        let order = r#"{"type":"orders","data":{"user_id":"u","connection_id":"c","account_id":"a","order_id":"o","cl_order_id":"x",
            "broker":"alpaca","normalized_status":"filled","side":"BUY","order_qty":"2","order_type":"MKT","order_tif":"DAY",
            "fills":[{"time":"2025-06-30T16:16:17.805369Z","quantity":"1","price":"200.37","fill_id":"f"}],"legs":null,"stop_price":null}}"#;
        match serde_json::from_str(order).unwrap() {
            ServerMessage::Orders { data } => assert_eq!(data.fills.unwrap()[0].price.as_deref(), Some("200.37")),
            other => panic!("{other:?}"),
        }
        let pos = r#"{"type":"positions","data":[{"symbol":"AAPL","quantity":"624","account_id":"a"}]}"#;
        assert!(matches!(serde_json::from_str(pos).unwrap(), ServerMessage::Positions { data } if data.len() == 1));
        let pong = r#"{"type":"pong","seq":1}"#;
        assert_eq!(serde_json::from_str::<ServerMessage>(pong).unwrap(), ServerMessage::Pong { seq: Some(1) });
        let odd = r#"{"type":"orders","data":{"order_id":7}}"#;
        assert!(
            matches!(decode(odd.as_bytes()), Some(ServerMessage::Undecoded { type_: Some(t), .. }) if t == "orders")
        );
        assert!(decode(b"not json").is_none());
        let new = r#"{"type":"heartbeat_v2","x":1}"#;
        assert_eq!(serde_json::from_str::<ServerMessage>(new).unwrap(), ServerMessage::Unknown);
    }

    #[test]
    fn client_messages_match_the_docs() {
        let a = ClientMessage::Authenticate {
            client_id: Some("id".into()),
            client_secret: Some("s".into()),
            access_token: None,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"action":"authenticate","client_id":"id","client_secret":"s"}"#
        );
        let s = ClientMessage::Subscribe { topics: vec![Topic::Orders, Topic::Balances] };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"action":"subscribe","topics":["orders","balances"]}"#);
        let u = ClientMessage::SubscribeUser { user_id: "u1".into() };
        assert_eq!(serde_json::to_string(&u).unwrap(), r#"{"action":"subscribe_user","user_id":"u1"}"#);
    }
}
