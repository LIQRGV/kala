//! Kala core: the temporal fact store behind Kala, and its native Rust API.

pub mod doctor;
pub mod error;
pub mod id;
mod key;
pub mod log;
pub mod overlap;
pub mod schema;
pub mod tick;
pub mod value;
pub mod world;

#[doc(hidden)]
pub use doctor::Tamper;
pub use doctor::{DoctorOptions, Finding, IndexName, Report, Severity, doctor, doctor_log};
pub use error::Error;
pub use id::{AttrId, EdgeId, EntityId, IdError, NodeTypeId};
pub use log::EntryId;
#[doc(hidden)]
pub use log::LogFault;
pub use overlap::{Interval, Overlap, Side, Warnings, find_overlaps, value_intervals};
pub use schema::{AttrDef, Cardinality, Kind, RefDirection, Schema};
pub use tick::Tick;
pub use value::{MAX_INDEXED_STRING_BYTES, Value, ValueError, ValueType};
pub use world::{
    Change, CommitResult, Correction, CutCause, Direction, EntryInfo, IndexDump, Mark, Op,
    RecoveryReport, TxId, Txn, View, World, WriteResult,
};
