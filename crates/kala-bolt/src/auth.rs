//! The server's authentication policy: the [`Authenticator`] trait, the
//! development-only [`NoAuth`], and [`Accounts`], which checks tokens against
//! the `system` database (kala-system) read through the registry's snapshots.
use crate::registry::{Registry, SystemDb};
use kala_system::auth::AccountVerifier;
pub(crate) use kala_system::auth::Plugin;
pub use kala_system::auth::{AuthFailure, AuthToken, Credentials, Identity, VerificationStats};
use kala_system::system::{self, SystemError};
use std::{net::IpAddr, path::Path};

/// Authentication policy for a server. Implementations must never log tokens.
pub trait Authenticator: Send + Sync {
    /// Authenticate a token without retaining its credentials.
    fn authenticate(&self, token: &AuthToken, peer: IpAddr) -> Result<Identity, AuthFailure>;
}
/// Accept every well-formed token. Server listeners are restricted to loopback.
/// A development mode, not an administrator: the identity may read every
/// user database (`read:*`) but never the `system` database.
#[derive(Debug, Default)]
pub struct NoAuth;
impl Authenticator for NoAuth {
    fn authenticate(&self, _token: &AuthToken, _peer: IpAddr) -> Result<Identity, AuthFailure> {
        Ok(Identity {
            principal: String::new(),
            host: String::new(),
            grants: vec!["read:*".into()],
        })
    }
}
/// Accounts loaded through the registry's committed, stamp-checked snapshots.
pub struct Accounts {
    registry: Registry,
    verifier: AccountVerifier,
}
impl Accounts {
    /// Initialize the system world and prepare one dummy hash for unknown users.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, SystemError> {
        system::ensure(directory.as_ref())?;
        let registry = Registry::open(directory.as_ref(), None, SystemDb::Serve)
            .map_err(|_| SystemError::Store)?;
        let verifier = AccountVerifier::new()?;
        Ok(Self { registry, verifier })
    }
    /// Read aggregate password-work counters for diagnostics.
    pub fn verification_stats(&self) -> VerificationStats {
        self.verifier.verification_stats()
    }
}
impl Authenticator for Accounts {
    fn authenticate(&self, token: &AuthToken, peer: IpAddr) -> Result<Identity, AuthFailure> {
        self.verifier.authenticate(token, peer, || {
            let (_, world) = self.registry.get(Some("system")).map_err(|_| AuthFailure)?;
            Ok(world)
        })
    }
}
