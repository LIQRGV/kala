// The crate docs are the README, so its examples run as doctests and
// can't drift from the API.
#![doc = include_str!("../README.md")]

pub mod doctor;
mod echo;
pub mod error;
pub mod fixture;
pub mod id;
pub mod interval;
mod key;
pub mod log;
pub mod overlap;
pub mod schema;
pub mod tick;
pub mod value;
mod value_table;
pub mod world;

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub use doctor::Tamper;
pub use doctor::{DoctorOptions, Finding, IndexName, Report, Severity, doctor, doctor_log};
pub use echo::must_escape;
pub use error::{Error, WorkInterruptReason};
pub use fixture::{FixtureError, FixtureErrorKind, OpRef, load_fixture, load_fixture_to};
pub use id::{AttrId, EdgeId, EntityId, IdError, NodeTypeId};
pub use interval::IntervalSet;
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub use log::LogFault;
pub use log::{EntryId, MAX_ENTRY_BYTES};
pub use overlap::{Interval, Overlap, Side, Warnings, find_overlaps, value_intervals};
pub use schema::{AttrDef, Cardinality, Kind, RefDirection, Schema};
pub use tick::{Precision, Tick};
pub use value::{MAX_INDEXED_STRING_BYTES, Value, ValueError, ValueType, fold_for_search};
pub use world::{
    AttrStats, Change, CommitResult, Correction, CutCause, Direction, EntryInfo, IndexDump, Mark,
    MeteredView, Op, RecoveryReport, TxId, Txn, View, WorkMeter, World, WriteResult,
};
