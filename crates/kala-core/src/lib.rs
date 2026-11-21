//! Kala core: the temporal fact store behind Kala, and its native Rust API.

pub mod error;
pub mod id;
mod key;
pub mod log;
pub mod schema;
pub mod tick;
pub mod value;
pub mod world;

pub use error::Error;
pub use id::{AttrId, EntityId, IdError, NodeTypeId};
pub use schema::{AttrDef, Cardinality, Kind, Schema};
pub use tick::Tick;
pub use value::{MAX_INDEXED_STRING_BYTES, Value, ValueError, ValueType};
pub use world::{Change, CommitResult, Op, TxId, Txn, View, World, WriteResult};
