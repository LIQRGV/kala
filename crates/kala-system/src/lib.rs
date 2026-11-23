//! The reserved `system` database: accounts, privileges, password hashing
//! and verification, and database-name validation.
//!
//! Everything here works on `system.kala` and plain values; nothing knows
//! about a wire protocol. A server (kala-bolt) authenticates through
//! [`auth::AccountVerifier`]; the CLI administers accounts through
//! [`system`].
#![forbid(unsafe_code)]
pub mod auth;
pub mod system;

pub use system::{SYSTEM_MARKER, SystemError, is_system, validate_restore};

/// Whether a filename stem is a valid database name (server database
/// discovery and local backups share this rule).
pub fn valid_database_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
