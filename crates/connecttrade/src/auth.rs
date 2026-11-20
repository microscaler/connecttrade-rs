//! Credentials. Connect Trade takes either credential headers on every call or a bearer token.
//!
//! Secrets are never printed: `Debug` shows only the ids.

use crate::request::Scope;

/// The platform's credentials, from Connect Trade.
#[derive(Clone)]
pub struct PlatformCredentials {
    pub client_id: String,
    pub client_secret: String,
}

/// One end user's credentials, from `POST /users` (or a secret rotation).
#[derive(Clone)]
pub struct UserCredentials {
    pub user_id: String,
    pub user_secret: String,
}

/// What a call is made with.
#[derive(Clone)]
pub enum Credentials {
    /// Platform only: user management, platform connections, platform tokens.
    Platform(PlatformCredentials),
    /// Platform plus one user: everything about that user's connections, accounts and orders.
    User(PlatformCredentials, UserCredentials),
    /// A platform token (1 hour) or a user token (15 minutes) from the Access Tokens endpoints.
    Bearer(String),
}

impl Credentials {
    pub fn platform(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Credentials::Platform(PlatformCredentials { client_id: client_id.into(), client_secret: client_secret.into() })
    }

    /// This platform acting for one user.
    pub fn for_user(&self, user_id: impl Into<String>, user_secret: impl Into<String>) -> Option<Self> {
        let p = match self {
            Credentials::Platform(p) | Credentials::User(p, _) => p.clone(),
            Credentials::Bearer(_) => return None,
        };
        Some(Credentials::User(p, UserCredentials { user_id: user_id.into(), user_secret: user_secret.into() }))
    }

    /// The headers for an operation of this scope, or the name of what is missing.
    pub(crate) fn headers(&self, scope: Scope) -> Result<Vec<(&'static str, String)>, &'static str> {
        match (self, scope) {
            (Credentials::Bearer(t), _) => Ok(vec![("authorization", format!("Bearer {t}"))]),
            (Credentials::Platform(p) | Credentials::User(p, _), Scope::Platform) => {
                Ok(vec![("client-id", p.client_id.clone()), ("client-secret", p.client_secret.clone())])
            }
            (Credentials::User(p, u), Scope::User) => Ok(vec![
                ("client-id", p.client_id.clone()),
                ("client-secret", p.client_secret.clone()),
                ("user-id", u.user_id.clone()),
                ("user-secret", u.user_secret.clone()),
            ]),
            (Credentials::Platform(_), Scope::User) => Err("user credentials"),
        }
    }
}

impl std::fmt::Debug for PlatformCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformCredentials").field("client_id", &self.client_id).finish_non_exhaustive()
    }
}

impl std::fmt::Debug for UserCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserCredentials").field("user_id", &self.user_id).finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::Platform(p) => {
                f.debug_struct("Platform").field("client_id", &p.client_id).finish_non_exhaustive()
            }
            Credentials::User(p, u) => f
                .debug_struct("User")
                .field("client_id", &p.client_id)
                .field("user_id", &u.user_id)
                .finish_non_exhaustive(),
            Credentials::Bearer(_) => f.write_str("Bearer(..)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_a_secret() {
        let c = Credentials::platform("cid", "very-secret").for_user("u1", "also-secret").unwrap();
        let shown = format!("{c:?}");
        assert!(shown.contains("cid") && shown.contains("u1"));
        assert!(!shown.contains("secret\""), "{shown}");
        assert!(!shown.contains("very-secret") && !shown.contains("also-secret"));
    }

    #[test]
    fn platform_credentials_cannot_act_for_a_user() {
        assert_eq!(Credentials::platform("a", "b").headers(Scope::User).unwrap_err(), "user credentials");
    }
}
