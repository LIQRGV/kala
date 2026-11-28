//! The server's authentication policy: the [`Authenticator`] trait, the
//! development-only [`NoAuth`], and [`Accounts`], which checks tokens against
//! the `system` database (kala-system) read through the registry's snapshots.
use crate::registry::{Registry, SystemDb};
use kala_system::auth::AccountVerifier;
pub(crate) use kala_system::auth::Plugin;
pub use kala_system::auth::{
    AuthFailure, AuthToken, Credentials, Identity, Peer, VerificationStats,
};
use kala_system::system::{self, SystemError};
use std::{net::IpAddr, path::Path, sync::Arc};

/// Authentication policy for a server. Implementations must never log tokens.
pub trait Authenticator: Send + Sync {
    /// Authenticate a token without retaining its credentials.
    fn authenticate(&self, token: &AuthToken, peer: IpAddr) -> Result<Identity, AuthFailure>;
    /// Authenticate a transport origin. Existing IP-only policies fail closed
    /// on local sockets unless they explicitly implement this method.
    fn authenticate_peer(&self, token: &AuthToken, peer: Peer) -> Result<Identity, AuthFailure> {
        match peer {
            Peer::Ip(ip) => self.authenticate(token, ip),
            Peer::Local => Err(AuthFailure),
        }
    }
    /// The current form of an identity this policy issued earlier: its
    /// grants as they are stored now, or `None` if its account is gone.
    /// The server asks before a connection begins a transaction or writes,
    /// when the accounts changed since the connection's grants were read,
    /// so a revoked privilege doesn't survive on a pooled connection. A
    /// policy without stored accounts keeps the identity as it is (the
    /// default).
    fn refresh(&self, identity: &Identity) -> Result<Option<Identity>, AuthFailure> {
        Ok(Some(identity.clone()))
    }
}
/// Accept every well-formed token. Server listeners are restricted to loopback.
/// A development mode, not an administrator: the identity may read every
/// user database (`read:*`) but never the `system` database.
#[derive(Debug, Default)]
pub struct NoAuth;
impl Authenticator for NoAuth {
    fn authenticate(&self, _token: &AuthToken, _peer: IpAddr) -> Result<Identity, AuthFailure> {
        self.authenticate_peer(_token, Peer::Ip(_peer))
    }
    fn authenticate_peer(&self, _token: &AuthToken, _peer: Peer) -> Result<Identity, AuthFailure> {
        Ok(Identity {
            principal: String::new(),
            host: String::new(),
            grants: vec!["read:*".into()],
            generation: 0,
        })
    }
}
/// Accounts loaded through the registry's committed, stamp-checked snapshots.
pub struct Accounts {
    registry: Arc<Registry>,
    verifier: AccountVerifier,
}
impl Accounts {
    /// Initialize the system world and prepare one dummy hash for unknown users.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, SystemError> {
        system::ensure(directory.as_ref())?;
        let registry = Registry::open(directory.as_ref(), None, SystemDb::Serve)
            .map_err(|_| SystemError::Store)?;
        Self::with_registry(Arc::new(registry))
    }

    pub(crate) fn with_registry(registry: Arc<Registry>) -> Result<Self, SystemError> {
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
        self.authenticate_peer(token, Peer::Ip(peer))
    }
    fn authenticate_peer(&self, token: &AuthToken, peer: Peer) -> Result<Identity, AuthFailure> {
        self.verifier.authenticate_peer(token, peer, || {
            let (_, world) = self.registry.get(Some("system")).map_err(|_| AuthFailure)?;
            Ok(world)
        })
    }
    fn refresh(&self, identity: &Identity) -> Result<Option<Identity>, AuthFailure> {
        let (_, world) = self.registry.get(Some("system")).map_err(|_| AuthFailure)?;
        kala_system::auth::current_identity(&world, identity).map_err(|_| AuthFailure)
    }
}
