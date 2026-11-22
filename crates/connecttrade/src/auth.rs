//! Credentials. Connect Trade takes either credential headers on every call or a bearer token.
//!
//! Secrets are never printed: `Debug` shows only the ids.

use crate::request::Scope;

/// A credential: zeroed in memory when dropped, `<redacted>` in `Debug`, read only through [`Secret::expose`] (#21).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(zeroize::Zeroizing<String>);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(zeroize::Zeroizing::new(value.into()))
    }

    /// The value, for the one place that must send it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret::new(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Secret::new(value)
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The platform's credentials, from Connect Trade.
#[derive(Clone)]
pub struct PlatformCredentials {
    pub client_id: String,
    pub client_secret: Secret,
}

/// One end user's credentials, from `POST /users` (or a secret rotation).
#[derive(Clone)]
pub struct UserCredentials {
    pub user_id: String,
    pub user_secret: Secret,
}

/// What a call is made with.
#[derive(Clone)]
pub enum Credentials {
    /// Platform only: user management, platform connections, platform tokens.
    Platform(PlatformCredentials),
    /// Platform plus one user: everything about that user's connections, accounts and orders.
    User(PlatformCredentials, UserCredentials),
    /// A platform token (1 hour) or a user token (15 minutes) from the Access Tokens endpoints.
    Bearer(Secret),
}

impl Credentials {
    pub fn platform(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Credentials::Platform(PlatformCredentials {
            client_id: client_id.into(),
            client_secret: Secret::new(client_secret),
        })
    }

    /// This platform acting for one user.
    pub fn for_user(&self, user_id: impl Into<String>, user_secret: impl Into<String>) -> Option<Self> {
        let p = match self {
            Credentials::Platform(p) | Credentials::User(p, _) => p.clone(),
            Credentials::Bearer(_) => return None,
        };
        Some(Credentials::User(p, UserCredentials { user_id: user_id.into(), user_secret: Secret::new(user_secret) }))
    }

    /// The headers for an operation of this scope, or the name of what is missing.
    pub(crate) fn headers(&self, scope: Scope) -> Result<Vec<(&'static str, String)>, &'static str> {
        match (self, scope) {
            (Credentials::Bearer(t), _) => Ok(vec![("authorization", format!("Bearer {}", t.expose()))]),
            (Credentials::Platform(p) | Credentials::User(p, _), Scope::Platform) => {
                Ok(vec![("client-id", p.client_id.clone()), ("client-secret", p.client_secret.expose().to_string())])
            }
            (Credentials::User(p, u), Scope::User) => Ok(vec![
                ("client-id", p.client_id.clone()),
                ("client-secret", p.client_secret.expose().to_string()),
                ("user-id", u.user_id.clone()),
                ("user-secret", u.user_secret.expose().to_string()),
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
    fn a_secret_is_shown_redacted_and_read_only_through_expose() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
        let bearer = Credentials::Bearer("t0k".into());
        assert_eq!(bearer.headers(Scope::User).unwrap(), vec![("authorization", "Bearer t0k".to_string())]);
    }

    #[test]
    fn platform_credentials_cannot_act_for_a_user() {
        assert_eq!(Credentials::platform("a", "b").headers(Scope::User).unwrap_err(), "user credentials");
    }
}
