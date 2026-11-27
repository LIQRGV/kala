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
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub use server::OpenCounts;
pub use server::{Server, ServerConfig};

// The `system` database lives in kala-system; `crate::system` names it here.
use kala_system::system;

mod metadata;

mod transaction;
mod write;

mod reads;
