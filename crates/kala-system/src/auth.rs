//! Account selection by user and peer IP, followed by bounded password verification.
use crate::system::{self, SystemError};
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{
        SaltString,
        rand_core::{OsRng, RngCore},
    },
};
use kala_core::World;
use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::{Condvar, Mutex},
};

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
    /// Authentication mechanism (`none` or `basic`).
    pub scheme: String,
    /// User name, if supplied.
    pub principal: String,
    /// Secret, never echoed.
    pub credentials: Credentials,
}
/// An authenticated identity containing no credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Authenticated user.
    pub principal: String,
    /// The host selector that won account selection.
    pub host: String,
    /// Captured grants; changes apply on the next login.
    pub grants: Vec<String>,
}
impl Identity {
    /// Whether this identity may write the named database (separate from read).
    pub fn permits_write(&self, db: &str) -> bool {
        self.grants.iter().any(|grant| {
            grant.split_once(':').is_some_and(|(privilege, target)| {
                (target == "*" || target == db)
                    && (privilege == "admin" || (db != "system" && privilege == "write"))
            })
        })
    }

    /// Whether this identity has the stored backup privilege. Local file
    /// backups use filesystem permissions; a future server dump will call this.
    /// Like reads, the accounts database (password hashes) is covered only by
    /// `admin`: neither `backup:*` nor `backup:system` reaches it.
    pub fn permits_backup(&self, db: &str) -> bool {
        self.grants.iter().any(|grant| {
            grant.split_once(':').is_some_and(|(privilege, target)| {
                (target == "*" || target == db)
                    && (privilege == "admin" || (db != "system" && privilege == "backup"))
            })
        })
    }
    /// Whether this identity may read the named database.
    pub fn permits_read(&self, db: &str) -> bool {
        self.grants.iter().any(|grant| {
            let Some((privilege, target)) = grant.split_once(':') else {
                return false;
            };
            (target == "*" || target == db)
                && (privilege == "admin" || (db != "system" && privilege == "read"))
        })
    }
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
/// Supported account authentication plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plugin {
    /// Passwordless login, restricted to explicit loopback selectors.
    Trust,
    /// Argon2id with the RustCrypto recommended default parameters.
    KalaPassword,
}
impl fmt::Display for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Trust => "trust",
            Self::KalaPassword => "kala_password",
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum HostKind {
    Any,
    Localhost,
    Exact(IpAddr),
    Cidr(IpAddr, u8),
    Wildcard(Vec<u8>),
}
/// A validated, DNS-free host selector with a canonical representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    text: String,
    kind: HostKind,
}
impl HostPattern {
    /// Parse an exact address, CIDR, trailing IPv4 wildcard, localhost, or `%`.
    pub fn parse(s: &str) -> Result<Self, SystemError> {
        if s.len() > 64 {
            return Err(SystemError::InvalidHost);
        }
        let (text, kind) = if s == "%" {
            (s.into(), HostKind::Any)
        } else if s == "localhost" {
            (s.into(), HostKind::Localhost)
        } else if let Ok(ip) = s.parse::<IpAddr>() {
            let ip = normalize_ip(ip);
            (ip.to_string(), HostKind::Exact(ip))
        } else if let Some((ip, prefix)) = s.split_once('/') {
            let mut ip = ip.parse::<IpAddr>().map_err(|_| SystemError::InvalidHost)?;
            if prefix.is_empty() || !prefix.bytes().all(|c| c.is_ascii_digit()) {
                return Err(SystemError::InvalidHost);
            }
            let mut prefix = prefix.parse::<u8>().map_err(|_| SystemError::InvalidHost)?;
            if let IpAddr::V6(v6) = ip
                && let Some(v4) = v6.to_ipv4_mapped()
            {
                prefix = prefix.checked_sub(96).ok_or(SystemError::InvalidHost)?;
                ip = IpAddr::V4(v4);
            }
            let network = match ip {
                IpAddr::V4(v4) if prefix <= 32 => {
                    IpAddr::V4(Ipv4Addr::from(u32::from(v4) & mask4(prefix)))
                }
                IpAddr::V6(v6) if prefix <= 128 => {
                    IpAddr::V6(Ipv6Addr::from(u128::from(v6) & mask6(prefix)))
                }
                _ => return Err(SystemError::InvalidHost),
            };
            (
                format!("{network}/{prefix}"),
                HostKind::Cidr(network, prefix),
            )
        } else if s.contains('%') {
            let parts: Vec<_> = s.split('.').collect();
            if !(2..=4).contains(&parts.len()) {
                return Err(SystemError::InvalidHost);
            }
            let mut literals = Vec::new();
            let mut wildcard = false;
            for part in parts {
                if part == "%" {
                    wildcard = true;
                } else {
                    if wildcard || part.is_empty() || !part.bytes().all(|c| c.is_ascii_digit()) {
                        return Err(SystemError::InvalidHost);
                    }
                    literals.push(part.parse::<u8>().map_err(|_| SystemError::InvalidHost)?);
                }
            }
            if !wildcard || literals.is_empty() {
                return Err(SystemError::InvalidHost);
            }
            let text = format!(
                "{}.%",
                literals
                    .iter()
                    .map(u8::to_string)
                    .collect::<Vec<_>>()
                    .join(".")
            );
            (text, HostKind::Wildcard(literals))
        } else {
            return Err(SystemError::InvalidHost);
        };
        Ok(Self { text, kind })
    }
    /// Whether this selector includes a peer; IPv4-mapped IPv6 is normalized.
    pub fn matches(&self, peer: IpAddr) -> bool {
        let peer = normalize_ip(peer);
        match (&self.kind, peer) {
            (HostKind::Any, _) => true,
            (HostKind::Localhost, ip) => {
                ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
            }
            (HostKind::Exact(expected), ip) => *expected == ip,
            (HostKind::Cidr(IpAddr::V4(network), prefix), IpAddr::V4(ip)) => {
                u32::from(ip) & mask4(*prefix) == u32::from(*network)
            }
            (HostKind::Cidr(IpAddr::V6(network), prefix), IpAddr::V6(ip)) => {
                u128::from(ip) & mask6(*prefix) == u128::from(*network)
            }
            (HostKind::Wildcard(prefix), IpAddr::V4(ip)) => ip.octets().starts_with(prefix),
            _ => false,
        }
    }
    /// Larger ranks win: exact, localhost, CIDR prefix, wildcard literals, any.
    pub fn specificity(&self) -> (u8, u8) {
        match &self.kind {
            HostKind::Exact(_) => (4, 0),
            HostKind::Localhost => (3, 0),
            HostKind::Cidr(_, p) => (2, *p),
            HostKind::Wildcard(p) => (1, p.len() as u8),
            HostKind::Any => (0, 0),
        }
    }
    /// Canonical selector spelling, used for account identity and duplicate checks.
    pub fn as_str(&self) -> &str {
        &self.text
    }
    pub(crate) fn permits_trust(&self) -> bool {
        matches!(self.kind, HostKind::Localhost)
            || matches!(&self.kind,HostKind::Exact(ip) if *ip==IpAddr::V4(Ipv4Addr::LOCALHOST)||*ip==IpAddr::V6(Ipv6Addr::LOCALHOST))
    }
}
fn mask4(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}
fn mask6(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    }
}
/// Normalize IPv4-mapped peers before any access decision.
pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}
/// A validated account identifier. Host addresses and networks are canonicalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountName {
    /// Case-sensitive ASCII user name.
    pub user: String,
    /// Canonical host selector.
    pub host: String,
}
impl AccountName {
    /// Parse `USER@HOST` with a user of at most 64 ASCII identifier bytes.
    pub fn parse(s: &str) -> Result<Self, SystemError> {
        let (user, host) = s.split_once('@').ok_or(SystemError::InvalidAccount)?;
        if user.is_empty()
            || user.len() > 64
            || !user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        {
            return Err(SystemError::InvalidAccount);
        }
        Ok(Self {
            user: user.into(),
            host: HostPattern::parse(host)?.text,
        })
    }
    pub(crate) fn key(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Password verification counters, containing no account or credential data.
#[derive(Debug, Default, Clone, Copy)]
pub struct VerificationStats {
    /// Verification attempts admitted to the bound.
    pub total: usize,
    /// Dummy verifications for accounts that cannot authenticate.
    pub dummy: usize,
    /// Currently running verifications.
    pub active: usize,
    /// Maximum observed concurrent verifications.
    pub peak: usize,
}
#[derive(Default)]
struct VerificationGate {
    stats: Mutex<VerificationStats>,
    available: Condvar,
}
struct Permit<'a>(&'a VerificationGate);
impl VerificationGate {
    fn enter(&self, dummy: bool) -> Result<Permit<'_>, AuthFailure> {
        let mut stats = self.stats.lock().map_err(|_| AuthFailure)?;
        while stats.active >= 4 {
            stats = self.available.wait(stats).map_err(|_| AuthFailure)?;
        }
        stats.active += 1;
        stats.total = stats.total.saturating_add(1);
        stats.dummy = stats.dummy.saturating_add(usize::from(dummy));
        stats.peak = stats.peak.max(stats.active);
        Ok(Permit(self))
    }
}
impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if let Ok(mut stats) = self.0.stats.lock() {
            stats.active -= 1;
            self.0.available.notify_one();
        }
    }
}
/// Account selection and bounded password verification over snapshots of
/// the system world. The caller supplies each snapshot (a server reads it
/// through its own cache), so this type never opens a file.
pub struct AccountVerifier {
    dummy: Credentials,
    gate: VerificationGate,
}
impl AccountVerifier {
    /// Prepare one dummy hash for unknown users.
    pub fn new() -> Result<Self, SystemError> {
        let mut random = [0u8; 32];
        OsRng
            .try_fill_bytes(&mut random)
            .map_err(|_| SystemError::Password)?;
        let dummy = hash_bytes(&random)?;
        Ok(Self {
            dummy,
            gate: VerificationGate::default(),
        })
    }
    /// Read aggregate password-work counters for diagnostics.
    pub fn verification_stats(&self) -> VerificationStats {
        self.gate.stats.lock().map(|s| *s).unwrap_or_default()
    }
}
impl AccountVerifier {
    /// Authenticate a token against the accounts in a system-world snapshot,
    /// without retaining its credentials. `system_world` is called only
    /// once the token's scheme is known to be supported.
    pub fn authenticate<W: AsRef<World>>(
        &self,
        token: &AuthToken,
        peer: IpAddr,
        system_world: impl FnOnce() -> Result<W, AuthFailure>,
    ) -> Result<Identity, AuthFailure> {
        let user = match token.scheme.as_str() {
            "none" => "root",
            "basic" => &token.principal,
            _ => return Err(AuthFailure),
        };
        let world = system_world()?;
        let accounts = system::read_accounts(world.as_ref()).map_err(|_| AuthFailure)?;
        let account = accounts
            .iter()
            .filter(|a| a.summary.user == user && a.host.matches(peer))
            .max_by_key(|a| a.host.specificity());
        if let Some(account) = account
            && account.summary.plugin == Plugin::Trust
            && account.host.permits_trust()
            && normalize_ip(peer).is_loopback()
        {
            return Ok(account.identity());
        }
        let password_account =
            account.filter(|a| a.summary.plugin == Plugin::KalaPassword && token.scheme == "basic");
        let hash = password_account
            .and_then(|a| a.password.as_ref())
            .unwrap_or(&self.dummy);
        let _permit = self.gate.enter(password_account.is_none())?;
        let verified = verify_password(hash, token.credentials.expose());
        if verified && let Some(account) = password_account {
            Ok(account.identity())
        } else {
            Err(AuthFailure)
        }
    }
}
pub(crate) fn hash_password(password: &str) -> Result<Credentials, SystemError> {
    if password.is_empty() {
        return Err(SystemError::EmptyPassword);
    }
    hash_bytes(password.as_bytes())
}
fn hash_bytes(password: &[u8]) -> Result<Credentials, SystemError> {
    let mut random = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| SystemError::Password)?;
    let salt = SaltString::encode_b64(&random).map_err(|_| SystemError::Password)?;
    let hash = Argon2::default()
        .hash_password(password, &salt)
        .map_err(|_| SystemError::Password)?
        .to_string();
    Ok(Credentials::new(hash))
}
fn verify_password(hash: &Credentials, password: &str) -> bool {
    // PasswordVerifier otherwise takes costs from the PHC string, not self's
    // parameters. Refuse unexpected costs before allocating Argon2 memory.
    if hash.expose().len() > 256 {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(hash.expose()) else {
        return false;
    };
    if parsed.algorithm.as_str() != "argon2id"
        || parsed.version != Some(19)
        || parsed.params.iter().count() != 3
        || parsed.params.get_decimal("m") != Some(19456)
        || parsed.params.get_decimal("t") != Some(2)
        || parsed.params.get_decimal("p") != Some(1)
        || parsed.hash.is_none_or(|h| h.len() != 32)
    {
        return false;
    }
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}
