//! The Streaming Broker API, as the docs describe it: `welcome`, `authenticate` within 30 seconds, `subscribe`,
//! `subscribe_user`, then incremental `orders`, `positions` and `balances` updates; `ping`/`pong`.

use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use connecttrade::stream::{ClientMessage, ServerMessage, Topic};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::broadcast::error::RecvError;

/// Platform sessions per `client_id`.
pub const MAX_PLATFORM_SESSIONS: usize = 10;
/// Users one session may subscribe to.
pub const MAX_USERS_PER_SESSION: usize = 500;

type Tx = SplitSink<WebSocket, Message>;
type Rx = SplitStream<WebSocket>;

pub(crate) async fn upgrade(State(s): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| session(s, socket))
}

/// Send one message; false when the client has gone.
async fn send(tx: &mut Tx, m: &ServerMessage) -> bool {
    match serde_json::to_string(m) {
        Ok(text) => tx.send(Message::Text(text.into())).await.is_ok(),
        Err(e) => {
            tracing::error!(error = %e, "a server message did not encode");
            false
        }
    }
}

fn topic_of(m: &ServerMessage) -> Option<Topic> {
    match m {
        ServerMessage::Orders { .. } => Some(Topic::Orders),
        ServerMessage::Positions { .. } => Some(Topic::Positions),
        ServerMessage::Balances { .. } => Some(Topic::Balances),
        _ => None,
    }
}

fn refused(m: &str) -> ServerMessage {
    ServerMessage::AuthError { message: Some(m.to_string()) }
}

/// Who a session authenticated as.
struct Authenticated {
    /// The user a user-token session is locked to.
    user: Option<String>,
    /// Holds one of the platform's session slots.
    platform_slot: bool,
}

fn take_platform_slot(s: &AppState) -> bool {
    if s.platform_sessions.fetch_add(1, Ordering::SeqCst) >= MAX_PLATFORM_SESSIONS {
        s.platform_sessions.fetch_sub(1, Ordering::SeqCst);
        return false;
    }
    true
}

/// Check one `authenticate` message: the session's identity, or the refusal to send back.
fn check_auth(s: &AppState, m: ClientMessage) -> Option<Result<Authenticated, ServerMessage>> {
    let ClientMessage::Authenticate { client_id, client_secret, access_token } = m else { return None };
    let limit = || refused(&format!("Concurrent session limit ({MAX_PLATFORM_SESSIONS}) reached for this client_id"));
    Some(match (client_id, client_secret, access_token) {
        (Some(id), Some(secret), None) => {
            if id != s.config.client_id || secret != s.config.client_secret {
                Err(refused("Invalid client credentials"))
            } else if take_platform_slot(s) {
                Ok(Authenticated { user: None, platform_slot: true })
            } else {
                Err(limit())
            }
        }
        (None, None, Some(token)) => match s.book().tokens.get(&token).cloned() {
            Some((_, exp)) if Instant::now() > exp => Err(refused("Access token has expired")),
            Some((Some(user), _)) => Ok(Authenticated { user: Some(user), platform_slot: false }),
            Some((None, _)) if take_platform_slot(s) => Ok(Authenticated { user: None, platform_slot: true }),
            Some((None, _)) => Err(limit()),
            None => Err(refused("Invalid access token")),
        },
        _ => Err(refused("Send client credentials or an access token, not both")),
    })
}

/// Wait for a good `authenticate` before the deadline; anything else before it is ignored.
async fn authenticate(s: &AppState, tx: &mut Tx, rx: &mut Rx) -> Option<Authenticated> {
    let deadline = Instant::now() + s.config.auth_timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let text = match tokio::time::timeout(left, rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        };
        let Ok(m) = serde_json::from_str::<ClientMessage>(&text) else { continue };
        match check_auth(s, m) {
            Some(Ok(a)) => return Some(a),
            Some(Err(no)) if !send(tx, &no).await => return None,
            _ => {}
        }
    }
}

/// A subscribed session's state.
struct Subscriptions {
    topics: HashSet<Topic>,
    users: HashSet<String>,
    locked_to: Option<String>,
}

impl Subscriptions {
    fn answer(&mut self, s: &AppState, m: ClientMessage) -> ServerMessage {
        match m {
            ClientMessage::Authenticate { .. } => refused("Session is already authenticated"),
            ClientMessage::Subscribe { topics } => self.subscribe(&topics),
            ClientMessage::SubscribeUser { user_id } => self.subscribe_user(s, user_id),
            ClientMessage::UnsubscribeUser { user_id } => {
                self.users.remove(&user_id);
                ServerMessage::UserUnsubscribed { user_id }
            }
            ClientMessage::Ping { seq } => ServerMessage::Pong { seq },
        }
    }

    fn subscribe(&mut self, topics: &[Topic]) -> ServerMessage {
        let unique: HashSet<_> = topics.iter().copied().collect();
        if topics.is_empty() || unique.len() != topics.len() {
            return ServerMessage::SubscriptionFailed { message: Some("Topics must be listed once each".into()) };
        }
        self.topics.extend(unique);
        let names = topics
            .iter()
            .filter_map(|t| serde_json::to_value(t).ok().and_then(|v| v.as_str().map(String::from)))
            .collect();
        ServerMessage::Subscribed { subscription_id: Some(uuid::Uuid::new_v4().to_string()), topics: names }
    }

    fn subscribe_user(&mut self, s: &AppState, user_id: String) -> ServerMessage {
        let failed = |m: String| ServerMessage::SubscriptionFailed { message: Some(m) };
        if self.locked_to.is_some() {
            failed("A user-token session cannot subscribe to another user".into())
        } else if !self.users.contains(&user_id) && self.users.len() >= MAX_USERS_PER_SESSION {
            failed(format!("User subscription limit ({MAX_USERS_PER_SESSION}) reached"))
        } else if !s.book().users.contains_key(&user_id) {
            failed(format!("Unknown user {user_id}"))
        } else {
            self.users.insert(user_id.clone());
            ServerMessage::UserSubscribed { user_id }
        }
    }

    fn wants(&self, user: &str, m: &ServerMessage) -> bool {
        self.users.contains(user) && topic_of(m).is_some_and(|t| self.topics.contains(&t))
    }
}

async fn session(s: Arc<AppState>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let welcome = ServerMessage::Welcome {
        session_id: uuid::Uuid::new_v4().to_string(),
        message: Some("Connected to the Connect Trade mock. Please authenticate within 30 seconds.".into()),
        timeout_seconds: Some(30),
    };
    if !send(&mut tx, &welcome).await {
        return;
    }
    let Some(auth) = authenticate(&s, &mut tx, &mut rx).await else {
        if tx.send(Message::Close(None)).await.is_err() {
            tracing::debug!("stream client left before authenticating");
        }
        return;
    };
    let ok =
        ServerMessage::AuthSuccess { message: Some("Authentication successful".into()), user_id: auth.user.clone() };
    if send(&mut tx, &ok).await {
        let subs =
            Subscriptions { topics: HashSet::new(), users: auth.user.iter().cloned().collect(), locked_to: auth.user };
        serve_session(&s, &mut tx, &mut rx, subs).await;
    }
    if auth.platform_slot {
        s.platform_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Answer the client and forward updates until either side goes.
async fn serve_session(s: &AppState, tx: &mut Tx, rx: &mut Rx, mut subs: Subscriptions) {
    let mut events = s.events.subscribe();
    loop {
        tokio::select! {
            incoming = rx.next() => {
                let reply = match incoming {
                    Some(Ok(Message::Text(t))) => match serde_json::from_str::<ClientMessage>(&t) {
                        Ok(m) => subs.answer(s, m),
                        Err(e) => ServerMessage::Error { message: Some(format!("Bad message: {e}")) },
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                    Some(Ok(_)) => continue,
                };
                if !send(tx, &reply).await { return }
            }
            event = events.recv() => {
                let out = match event {
                    Ok((user, msg)) if subs.wants(&user, &msg) => msg,
                    Ok(_) => continue,
                    // a slow reader missed updates: tell it, as a real client must re-read state over REST
                    Err(RecvError::Lagged(n)) => {
                        ServerMessage::Error { message: Some(format!("{n} updates dropped; re-read state over REST")) }
                    }
                    Err(RecvError::Closed) => return,
                };
                if !send(tx, &out).await { return }
            }
        }
    }
}
