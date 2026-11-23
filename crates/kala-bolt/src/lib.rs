//! Read-only Bolt access to Kala worlds.
#![forbid(unsafe_code)]
pub mod auth;
mod chunk;
mod error;
mod handshake;
mod message;
pub mod packstream;
mod query;
mod registry;
mod server;
mod session;
pub mod system;
pub use server::{Server, ServerConfig};
