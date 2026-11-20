//! The Streaming Broker API, as the docs describe it: `welcome`, `authenticate` within 30 seconds, `subscribe`,
//! `subscribe_user`, then incremental `orders`, `positions` and `balances` updates; `ping`/`pong`.

use crate::AppState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use connecttrade::stream::{ClientMessage, ServerMessage, Topic};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const MAX_PLATFORM_SESSIONS: usize = 10;
pub const MAX_USERS_PER_SESSION: usize = 500;

pub async fn upgrade(State(s): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| session(s, socket))
}

fn text(m: &ServerMessage) -> Message {
    Message::Text(serde_json::to_string(m).expect("server messages serialise").into())
}

fn topic_of(m: &ServerMessage) -> Option<Topic> {
    match m {
        ServerMessage::Orders { .. } => Some(Topic::Orders),
        ServerMessage::Positions { .. } => Some(Topic::Positions),
        ServerMessage::Balances { .. } => Some(Topic::Balances),
        _ => None,
    }
}

async fn session(s: Arc<AppState>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let session_id = uuid::Uuid::new_v4().to_string();
    let welcome = ServerMessage::Welcome {
        session_id,
        message: Some("Connected to the Connect Trade mock. Please authenticate within 30 seconds.".into()),
        timeout_seconds: Some(30),
    };
    if tx.send(text(&welcome)).await.is_err() {
        return;
    }

    // authenticate within the deadline; anything else before that is ignored
    let deadline = Instant::now() + s.config.auth_timeout;
    let mut platform_slot = false;
    let user_lock: Option<String>;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = match tokio::time::timeout(left, rx.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(_))) => continue,
            _ => {
                let _ = tx.send(Message::Close(None)).await;
                return;
            }
        };
        let Ok(ClientMessage::Authenticate { client_id, client_secret, access_token }) = serde_json::from_str(&msg)
        else {
            continue;
        };
        let refused = |m: &str| ServerMessage::AuthError { message: Some(m.to_string()) };
        match (client_id, client_secret, access_token) {
            (Some(id), Some(secret), None) => {
                if id != s.config.client_id || secret != s.config.client_secret {
                    let _ = tx.send(text(&refused("Invalid client credentials"))).await;
                    continue;
                }
                if s.platform_sessions.fetch_add(1, Ordering::SeqCst) >= MAX_PLATFORM_SESSIONS {
                    s.platform_sessions.fetch_sub(1, Ordering::SeqCst);
                    let m = format!("Concurrent session limit ({MAX_PLATFORM_SESSIONS}) reached for this client_id");
                    let _ = tx.send(text(&refused(&m))).await;
                    continue;
                }
                platform_slot = true;
                user_lock = None;
            }
            (None, None, Some(token)) => {
                let found = s.book.lock().unwrap().tokens.get(&token).cloned();
                match found {
                    Some((_, exp)) if Instant::now() > exp => {
                        let _ = tx.send(text(&refused("Access token has expired"))).await;
                        continue;
                    }
                    Some((user, _)) => {
                        if user.is_none() {
                            if s.platform_sessions.fetch_add(1, Ordering::SeqCst) >= MAX_PLATFORM_SESSIONS {
                                s.platform_sessions.fetch_sub(1, Ordering::SeqCst);
                                let _ = tx
                                    .send(text(&refused("Concurrent session limit (10) reached for this client_id")))
                                    .await;
                                continue;
                            }
                            platform_slot = true;
                        }
                        user_lock = user;
                    }
                    None => {
                        let _ = tx.send(text(&refused("Invalid access token"))).await;
                        continue;
                    }
                }
            }
            _ => {
                let _ = tx.send(text(&refused("Send client credentials or an access token, not both"))).await;
                continue;
            }
        }
        break;
    }
    let ok =
        ServerMessage::AuthSuccess { message: Some("Authentication successful".into()), user_id: user_lock.clone() };
    if tx.send(text(&ok)).await.is_err() {
        release(&s, platform_slot);
        return;
    }

    let mut topics: HashSet<Topic> = HashSet::new();
    let mut users: HashSet<String> = user_lock.iter().cloned().collect();
    let mut events = s.events.subscribe();
    loop {
        tokio::select! {
            incoming = rx.next() => {
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { break }
                    continue;
                };
                let reply = match serde_json::from_str::<ClientMessage>(&t) {
                    Ok(ClientMessage::Authenticate { .. }) => ServerMessage::AuthError { message: Some("Session is already authenticated".into()) },
                    Ok(ClientMessage::Subscribe { topics: ts }) => {
                        let unique: HashSet<_> = ts.iter().copied().collect();
                        if ts.is_empty() || unique.len() != ts.len() {
                            ServerMessage::SubscriptionFailed { message: Some("Topics must be listed once each".into()) }
                        } else {
                            topics.extend(unique);
                            ServerMessage::Subscribed {
                                subscription_id: Some(uuid::Uuid::new_v4().to_string()),
                                topics: ts.iter().map(|t| serde_json::to_value(t).unwrap().as_str().unwrap().to_string()).collect(),
                            }
                        }
                    }
                    Ok(ClientMessage::SubscribeUser { user_id }) => {
                        if user_lock.is_some() {
                            ServerMessage::SubscriptionFailed { message: Some("A user-token session cannot subscribe to another user".into()) }
                        } else if !users.contains(&user_id) && users.len() >= MAX_USERS_PER_SESSION {
                            ServerMessage::SubscriptionFailed { message: Some(format!("User subscription limit ({MAX_USERS_PER_SESSION}) reached")) }
                        } else if !s.book.lock().unwrap().users.contains_key(&user_id) {
                            ServerMessage::SubscriptionFailed { message: Some(format!("Unknown user {user_id}")) }
                        } else {
                            users.insert(user_id.clone());
                            ServerMessage::UserSubscribed { user_id }
                        }
                    }
                    Ok(ClientMessage::UnsubscribeUser { user_id }) => {
                        users.remove(&user_id);
                        ServerMessage::UserUnsubscribed { user_id }
                    }
                    Ok(ClientMessage::Ping { seq }) => ServerMessage::Pong { seq },
                    Err(e) => ServerMessage::Error { message: Some(format!("Bad message: {e}")) },
                };
                if tx.send(text(&reply)).await.is_err() { break }
            }
            event = events.recv() => {
                match event {
                    Ok((user, msg)) => {
                        let wanted = users.contains(&user) && topic_of(&msg).is_some_and(|t| topics.contains(&t));
                        if wanted && tx.send(text(&msg)).await.is_err() { break }
                    }
                    // a slow reader missed updates: tell it, as a real client must re-snapshot
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let m = ServerMessage::Error { message: Some(format!("{n} updates dropped; re-read state over REST")) };
                        if tx.send(text(&m)).await.is_err() { break }
                    }
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    }
    release(&s, platform_slot);
}

fn release(s: &AppState, platform_slot: bool) {
    if platform_slot {
        s.platform_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}
