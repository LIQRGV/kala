//! Pluggable authentication. NoAuth is safe only on loopback listeners.
use std::fmt;
/// Credentials are accessible to authenticators but always redacted when formatted.
#[derive(Clone)]
pub struct Credentials(String);
impl Credentials {
    /// Wrap a credential without logging it.
    pub fn new(value: String) -> Self {
        Self(value)
    }
    /// Access the secret for authentication only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
impl fmt::Display for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}
/// An authentication token. Dropped immediately after authentication.
#[derive(Debug)]
pub struct AuthToken {
    /// Authentication mechanism (`none`, `basic`, …).
    pub scheme: String,
    /// User name, if supplied.
    pub principal: String,
    /// Secret, never echoed.
    pub credentials: Credentials,
}
/// An authenticated identity (no credentials).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub principal: String,
}
/// Authentication refusal. Its wire message is deliberately generic.
#[derive(Debug)]
pub struct AuthFailure;
impl fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("authentication failed")
    }
}
impl std::error::Error for AuthFailure {}
/// Authentication policy for a server. Implementations must never log tokens.
pub trait Authenticator: Send + Sync {
    /// Authenticate a token without retaining its credentials.
    fn authenticate(&self, token: &AuthToken) -> Result<Identity, AuthFailure>;
}
/// Accept every well-formed token. Server listeners are restricted to loopback.
#[derive(Debug, Default)]
pub struct NoAuth;
impl Authenticator for NoAuth {
    fn authenticate(&self, _token: &AuthToken) -> Result<Identity, AuthFailure> {
        Ok(Identity {
            principal: String::new(),
        })
    }
}
