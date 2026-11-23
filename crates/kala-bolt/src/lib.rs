//! Read-only Bolt access to Kala worlds.
#![forbid(unsafe_code)]
pub mod auth;
mod chunk;
mod error;
mod handshake;
mod language;
mod message;
pub mod packstream;
mod query;
mod registry;
mod server;
mod session;
pub mod system;
pub use server::{Server, ServerConfig};

/// Shared database filename validation for server discovery and local backups.
pub use registry::valid_name as valid_database_name;
