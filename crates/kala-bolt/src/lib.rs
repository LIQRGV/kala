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
pub mod transport;
pub use server::{Server, ServerConfig};

// The `system` database lives in kala-system; `crate::system` names it here.
use kala_system::system;

mod metadata;
