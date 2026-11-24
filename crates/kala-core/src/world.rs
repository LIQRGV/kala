//! A Kala world: one story timeline, its schema and its entities.
//!
//! Facts live in the in-memory `E` index (layout in the private `key`
//! module). A world is either in memory only ([`World::in_memory`]) or backed
//! by a log file ([`World::create`], format in [`crate::log`]), which is then
//! the source of truth: a committed [`Txn`] is appended as its `DEFINE`
//! entries, its facts and a `COMMIT`, and fsynced, before the in-memory
//! tables change. Every operation first validates into a definition
//! (`EntityDef`, `FactDef`, `EdgeDef`) before applying it. Schema operations
//! temporarily update metadata under the transaction's exclusive borrow;
//! dropping or failing the transaction restores that metadata. No failed
//! operation becomes visible or changes the log.
//!
//! **Edges.** The first stored assert of a `ref` fact `(source, attr,
//! target)` interns the triple as an edge: it gets the next [`EdgeId`]
//! (one counter for the whole world) and a `DEFINE` entry in the same
//! transaction. The triple keeps that ID forever, through retracts and
//! re-asserts; nothing else (no-ops, retracts, failed or dropped
//! transactions) consumes an edge sequence. A defined edge is an ID like an
//! entity's: it can be a fact's subject and a `ref` value.
//!
//! **Bidirectional attributes.** A write of `(a, attr, b)` is normalised to
//! `(min(a, b), attr, max(a, b))` by entity ID before it is judged, interned
//! or stored, so both spellings are one edge and one history (`E` under the
//! smaller ID, `R` under the larger). Reads merge both sides. Directed
//! attributes are never normalised.
//!
//! **Corrections.** [`World::correct`] changes what canon says (author
//! time), never the story: a [`Correction::Delete`] appends a `DELETED`
//! marker at the corrected fact's key; a [`Correction::Replace`] appends a
//! `Revised` fact, after a `DELETED` marker at the old key if the key
//! moves. Corrections of one base fact form its revision chain; a
//! correction always acts on the chain's current fact. At canon N the *current entry* of a
//! key `(entity, attr, value, tick)` is its entry with the highest tx `<=`
//! N; if that is a `DELETED` marker the key holds nothing, and every read
//! and every write judgement skips that whole tick for that value. A
//! transaction stores at most one entry per key.
//!
//! Every write goes through a [`Txn`]: [`World::create_entity`],
//! [`World::assert`], [`World::retract`] and [`World::correct`] are
//! one-write transactions that commit at once. Every read goes through a [`View`] pinned to a canon tx;
//! the world's own reads are the view at its latest tx.
//!
//! **Layout.** This file holds the public value types, the [`World`] struct
//! and its facade. A world is composed of four components, each in a private
//! submodule: `catalog` (the `Catalog` struct: schema and ID tables), `index`
//! (the `Indexes` struct: the index maps and the seeks reads are built from),
//! `journal` (the `Journal` struct: kept log entries, the latest tx,
//! correction chains and the log file) and `write_gate` (the `WriteGate`
//! struct: read-only and recovery state). The rest of the module is split by
//! concern into submodules that share its scope: `ids` (node types, entities,
//! handles, edges), `open` (replaying a log), `txn` (the write path and
//! [`Txn`]), `sync` (catching up with other handles on the same log file and
//! committing under its lock), `meter` (bounded reads), `view` ([`View`]),
//! `schema_ops` and `migration`. The indexes themselves sit behind the `store` module's
//! ordered-map seam. Every hook of the `test-hooks` feature is in the
//! `test_hooks` submodule.

mod catalog;
mod concurrency;
mod ids;
mod index;
mod journal;
mod meter;
mod migration;
mod open;
mod schema_ops;
mod sync;
#[cfg(feature = "test-hooks")]
mod test_hooks;
mod txn;
mod view;
mod write_gate;

use crate::error::{Error, WorkInterruptReason};
use crate::id::{AttrId, EdgeId, EntityId, IdError, MAX_NODE_TYPE, NodeTypeId};
use crate::interval::IntervalSet;
use crate::key::{self, ValueRef};
use crate::log::{
    self, EncodedTxn, EntryBody, EntryId, LogEntry, LogWriter, NewEntry, SurveyedTxn, Tail,
    WriterLock,
};
use crate::overlap::{
    Interval, Overlap, Side, Warnings, find_overlaps, find_overlaps_touching, value_intervals,
};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection, Schema};
use crate::schema::{
    AttributeOptions, MigrationOptions, MigrationReport, SchemaChange, SchemaEdit,
};
use crate::store::{OrderedKv, OrderedMap};
use crate::tick::{Precision, Tick};
use crate::value::{Value, ValueType, fold_for_search};
use crate::value_table::{ContentHash, ValueTable, fnv1a64};
pub use concurrency::{Concurrency, Finalized, FinalizedOp, IdMap, MAX_READ_KEYS, TickMoved};
pub use index::IndexDump;
pub(crate) use index::{Index, decode_e_row, decode_v_row, decode_vfold_row};
pub use meter::{MeteredView, WorkMeter};
pub(crate) use open::ReplayFault;
use schema_ops::{OrderedEntry, SchemaSnapshot};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering as AtomicOrdering};
pub use txn::{CommitResult, HeldValues, TickOccupant, Txn};
pub use view::View;
use {catalog::*, ids::*, index::*, journal::*, meter::*, open::*, txn::*, write_gate::*};

/// A position on the author timeline: the number of the transaction that
/// wrote a fact. Each committed transaction advances it by one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u64);

/// What a write did: how many entries it stored (0 for a no-op), the tick
/// it used, the log entries it appended, the edges it interned and the
/// overlap warnings it introduced or resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteResult {
    /// Entries stored by this write: a fact, or a correction's `DELETED`
    /// marker and/or `Revised` fact (0 for a no-op).
    pub changed: usize,
    /// The tick the write landed on (its own, or the defaulted one).
    pub tick: Tick,
    /// IDs of the fact and correction (`DELETED`, `Revised`) entries the write's
    /// commit appended, in log order (`DEFINE`s and the `Commit` are not
    /// listed): what a later correction points at. Like `edges`, only an auto-committed write fills it; entry
    /// IDs are assigned at commit, so a [`Txn`] write leaves it empty and
    /// its [`CommitResult::entries`] lists them. Empty for a no-op.
    pub entries: Vec<EntryId>,
    /// Edges newly interned by the write's commit, in interning order. Only
    /// an auto-committed write ([`World::assert`], [`World::retract`],
    /// [`World::correct`]) fills it; a [`Txn`] write leaves it empty and its
    /// [`CommitResult::edges`] lists them.
    pub edges: Vec<EdgeId>,
    /// The cardinality overlaps the write's commit introduced and resolved
    /// (see [`Warnings`]). Like `entries`, only an auto-committed write
    /// fills it; a [`Txn`] write leaves it empty and its
    /// [`CommitResult::warnings`] carries the transaction's net diff.
    pub warnings: Warnings,
    /// Schema changes made by this operation.
    pub schema: Vec<SchemaChange>,
}

/// The op of a stored fact: whether it starts (`Assert`) or ends
/// (`Retract`) its value from its tick on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The value holds from the fact's tick on.
    Assert,
    /// The value stops holding from the fact's tick on.
    Retract,
}

/// What one stored index entry says: a fact's op, or a `DELETED` marker
/// (a correction: from its tx on, its key holds nothing). The value of every
/// `E` and `R` index row, as [`World::index_dump`] shows it.
///
/// The discriminants match [`Op`]'s (`Assert` 0, `Retract` 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// A stored assert.
    Assert,
    /// A stored retract.
    Retract,
    /// A `DELETED` marker.
    Deleted,
}

impl Mark {
    /// The fact's op, or `None` for a `DELETED` marker.
    fn op(self) -> Option<Op> {
        match self {
            Mark::Assert => Some(Op::Assert),
            Mark::Retract => Some(Op::Retract),
            Mark::Deleted => None,
        }
    }
}

impl From<Op> for Mark {
    fn from(op: Op) -> Mark {
        match op {
            Op::Assert => Mark::Assert,
            Op::Retract => Mark::Retract,
        }
    }
}

impl PartialEq<Op> for Mark {
    fn eq(&self, op: &Op) -> bool {
        *self == Mark::from(*op)
    }
}

/// A correction of a stored fact (author time, not story time), for
/// [`World::correct`] and [`Txn::correct`]. The corrected entry is never
/// touched: the correction is a new log entry pointing at it.
#[derive(Clone, Debug, PartialEq)]
pub enum Correction {
    /// The fact is not canon: a `DELETED` marker at its key, so readers at
    /// canon `>=` the correction see the key as empty.
    Delete,
    /// The fact (the chain's current revision) is replaced by one with the
    /// given fields changed (`None` keeps the field): a `Revised` entry, after
    /// a `DELETED` marker at the old key if the key (tick or value) moves.
    Replace {
        /// The new value.
        value: Option<Value>,
        /// The new tick.
        tick: Option<Tick>,
        /// The new op.
        op: Option<Op>,
        /// The new tick precision. A replace that changes only this is a
        /// `Revised` entry at the same key with the op unchanged; a moved
        /// fact keeps the current precision unless one is given.
        precision: Option<Precision>,
    },
}

/// Which side of a `ref` attribute [`View::neighbors`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// The entity is the source: its targets (`E|entity|attr|`).
    Out,
    /// The entity is the target: its sources (`R|entity|attr|`).
    In,
    /// Both sides: the union of `Out` and `In`.
    Both,
}

impl Direction {
    /// The same read seen from the other end: `Out` <-> `In`; `Both` stays.
    fn flipped(self) -> Direction {
        match self {
            Direction::Out => Direction::In,
            Direction::In => Direction::Out,
            Direction::Both => Direction::Both,
        }
    }

    fn reads_out(self) -> bool {
        matches!(self, Direction::Out | Direction::Both)
    }

    fn reads_in(self) -> bool {
        matches!(self, Direction::In | Direction::Both)
    }
}

/// One change point of an `(entity, attribute)`'s history, as returned by
/// [`View::history`] and [`View::as_of_detail`]: at story tick `tick`,
/// `value` was asserted or retracted (`op`), with the tick known to
/// `precision`.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Story tick of the change.
    pub tick: Tick,
    /// Assert or retract.
    pub op: Op,
    /// The value asserted or retracted.
    pub value: Value,
    /// How precisely the tick is known, as the entry stored it (a payload:
    /// reads treat the tick as exact).
    pub precision: Precision,
}

/// A validated fact, ready to apply. Later this is the payload of the log's
/// fact entry and the source of its index keys.
#[derive(Debug)]
pub(crate) struct FactDef {
    pub(crate) entity: EntityId,
    pub(crate) attr: AttrId,
    /// Order-preserving encoding of the full value: what the log stores,
    /// and what every rule and public order compares.
    pub(crate) value: Vec<u8>,
    pub(crate) tick: Tick,
    pub(crate) op: Op,
    /// The precision of the tick: carried by the entry, never part of a
    /// key, and ignored by every judgement except a correction's "nothing
    /// changed".
    pub(crate) precision: Precision,
    /// The target of a `ref` fact (its `R` key), `None` for other values.
    pub(crate) target: Option<EntityId>,
    /// For an unindexed string value, the value reference its `E` keys
    /// carry instead of `value` (see `Indexes::value_ref`); `None` for
    /// every other value, whose `E` keys carry `value` itself.
    pub(crate) vref: Option<[u8; key::VALUE_REF_LEN]>,
}

impl FactDef {
    /// The value part of the fact's `E` keys: its value reference, or its
    /// value encoding.
    pub(crate) fn key_value(&self) -> &[u8] {
        match &self.vref {
            Some(r) => r,
            None => &self.value,
        }
    }

    /// The fact's value reference, for an unindexed string value.
    fn value_ref(&self) -> Option<ValueRef> {
        self.vref.as_ref().and_then(|r| ValueRef::from_key_bytes(r))
    }
}

/// Why [`World::open`] cut a tail off the log (recovery rules in
/// [`crate::log`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutCause {
    /// The last transaction was cut short (or is a zero-filled tail).
    Torn,
    /// The last transaction is all there but its body checksum fails, and
    /// no valid begin record follows it.
    Damaged,
}

/// What [`World::open`] did to recover the log: how many bytes of a torn or
/// damaged tail it cut off, why, and where it saved them. A clean open, a
/// new world ([`World::create`]) and an in-memory world report the default:
/// nothing cut, no quarantine file, no cause. A world from
/// [`World::open_read_only`] reports what `World::open` would cut, with no
/// quarantine file (nothing is written).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Bytes cut off the end of the log (0 if the log was clean).
    pub bytes_cut: u64,
    /// The quarantine file holding exactly the cut bytes, if any were cut
    /// (always `None` for [`World::open_read_only`]).
    pub quarantine: Option<PathBuf>,
    /// Why the bytes were cut; `None` if nothing was.
    pub cause: Option<CutCause>,
}

/// One committed log entry, from [`World::entry`]: its ID, the transaction
/// it belongs to and what it says.
#[derive(Clone, Debug, PartialEq)]
pub struct EntryInfo {
    /// The entry's position in the log.
    pub id: EntryId,
    /// The tx of the transaction holding it (the tx of its `Commit`).
    pub tx: TxId,
    /// What the entry says, exactly as [`crate::log::read_log`] returns it.
    pub body: EntryBody,
}

impl EntryInfo {
    /// `(of, rev)` if the entry is a correction (revision `rev` of base
    /// entry `of`: a `DELETED` marker or a `Revised` fact); `None` for any
    /// other entry.
    pub fn revision(&self) -> Option<(EntryId, u32)> {
        match self.body {
            EntryBody::Deleted { of, rev, .. } | EntryBody::Revised { of, rev, .. } => {
                Some((of, rev))
            }
            _ => None,
        }
    }
}

/// Size estimates of one attribute, for a query planner (see
/// [`World::attr_stats`]). Counted over every fact entry ever committed for
/// the attribute, so they never shrink.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AttrStats {
    /// Distinct entities (or edges) that have had a value.
    pub entities: usize,
    /// Distinct values (by encoding) any entity has had.
    pub values: usize,
    /// Distinct `(entity, value)` pairs: the number of `E` histories.
    pub pairs: usize,
}

/// The distinct entities, values and pairs behind one attribute's
/// [`AttrStats`]. Each distinct value is numbered once (by its `E`-key
/// value part, which is one-to-one with the value: a value reference for an
/// unindexed string, so long strings aren't copied), and pairs hold the
/// number, not the bytes.
#[derive(Debug, Default)]
struct AttrCounts {
    entities: HashSet<EntityId>,
    values: HashMap<Box<[u8]>, usize>,
    pairs: HashSet<(EntityId, usize)>,
}

impl AttrCounts {
    /// Counts one stored fact entry of `entity` with `E`-key value part
    /// `value`.
    fn add(&mut self, entity: EntityId, value: &[u8]) {
        let next = self.values.len();
        let n = match self.values.get(value) {
            Some(&n) => n,
            None => {
                self.values.insert(value.into(), next);
                next
            }
        };
        self.entities.insert(entity);
        self.pairs.insert((entity, n));
    }

    fn stats(&self) -> AttrStats {
        AttrStats {
            entities: self.entities.len(),
            values: self.values.len(),
            pairs: self.pairs.len(),
        }
    }
}

/// A world: schema, entities and (later) their facts over story time.
#[derive(Debug)]
pub struct World {
    /// The schema and ID tables: what names and IDs mean (see [`Catalog`]).
    catalog: Catalog,
    /// Every index and derived table over the stored facts (see [`Indexes`]).
    indexes: Indexes,
    /// What was committed, in author order: the kept log entries, the
    /// latest tx, the correction chains and the log file (see [`Journal`]).
    journal: Journal,
    /// Whether writes are accepted at all, and what opening the log
    /// recovered (see [`WriteGate`]).
    gate: WriteGate,
}

impl World {
    /// Creates an empty in-memory world (no file) with the given schema.
    ///
    /// Fails with [`Error::InvalidName`] for an empty node type, attribute
    /// or inverse name, [`Error::DuplicateName`] if a node type or an
    /// attribute is declared twice (the two are separate namespaces; inverse
    /// names share the attribute namespace), [`Error::InvalidDirection`] if
    /// an attribute's direction or inverse doesn't fit it (see
    /// [`AttrDef::direction`]), or [`Error::Id`] if there are more node types
    /// than fit in an ID.
    ///
    /// The world holds the genesis transaction tx#0 (the schema's `DEFINE`s
    /// and its `Commit`) as entries, numbered as [`World::create`] writes
    /// them, so the same writes get the same entry IDs in both kinds of world.
    /// A name too long for a log entry (one over
    /// [`MAX_ENTRY_BYTES`](log::MAX_ENTRY_BYTES)) fails with [`Error::Io`]
    /// of kind `InvalidInput`, as it would for a file.
    pub fn in_memory(schema: Schema) -> Result<World, Error> {
        World::with_genesis(schema).map(|(world, _)| world)
    }

    /// Validates the schema into a new world holding just the genesis
    /// transaction, and returns it with that transaction encoded (what
    /// [`World::create`] writes after the file header).
    fn with_genesis(schema: Schema) -> Result<(World, EncodedTxn), Error> {
        let mut world = World {
            catalog: Catalog::from_schema(&schema)?,
            indexes: Indexes::new(fnv1a64),
            journal: Journal::in_memory(),
            gate: WriteGate::open(),
        };
        let settings: Vec<_> = schema
            .settings()
            .iter()
            .map(|(key, value)| SchemaEdit::Setting {
                key: key.clone(),
                value: value.clone(),
            })
            .collect();
        for edit in &settings {
            world.apply_schema_edit(edit, TxId(0), false)?;
        }
        let genesis = world.encode_genesis(&settings)?;
        world
            .journal
            .record(genesis.entries.iter().cloned(), TxId(0));
        Ok((world, genesis))
    }

    /// Encodes the genesis transaction tx#0 of this new world: a `DEFINE`
    /// per node type and per attribute, in ID order, then the world
    /// `settings`, numbered from entry 1.
    fn encode_genesis(&self, settings: &[SchemaEdit]) -> Result<EncodedTxn, Error> {
        let node_types =
            self.catalog
                .node_types
                .iter()
                .enumerate()
                .map(|(i, nt)| NewEntry::NodeType {
                    // In range: refused by `Catalog::from_schema` if more
                    // than MAX_NODE_TYPE + 1 types.
                    id: NodeTypeId(i as u16),
                    name: &nt.name,
                });
        let attrs = self
            .catalog
            .attributes
            .iter()
            .enumerate()
            .map(|(i, def)| NewEntry::Attr {
                // In range: refused by `Catalog::from_schema` if more than
                // u32::MAX + 1 attributes.
                id: AttrId(i as u32),
                def,
            });
        log::encode_txn(
            EntryId(1),
            TxId(0),
            log::header_checksum(),
            node_types
                .chain(attrs)
                .chain(settings.iter().map(NewEntry::Schema)),
        )
    }

    /// The committed entry with ID `id`: its tx and what it says, for every
    /// kind of entry (`DEFINE`s and `Commit`s included), exactly as
    /// [`crate::log::read_log`] returns it. `None` for an ID no committed
    /// entry has (0, past the end, or an entry of a transaction that was
    /// dropped or failed to commit). Not limited by canon: every committed
    /// entry is visible.
    pub fn entry(&self, id: EntryId) -> Option<EntryInfo> {
        self.journal.kept_entry(id).cloned()
    }

    /// Entry IDs generated by migration markers, which a script dumper must
    /// omit: replaying the marker regenerates these exact entries. Uses the
    /// ordinary replay validator, including intra-transaction schema changes.
    pub fn migration_copy_entries(&self) -> Result<std::collections::HashSet<EntryId>, Error> {
        let mut copies = std::collections::HashSet::new();
        let kept = self.journal.entries();
        if !kept
            .iter()
            .any(|e| matches!(e.body, EntryBody::Schema(SchemaEdit::Migration { .. })))
        {
            return Ok(copies);
        }
        let genesis: Vec<_> = kept
            .iter()
            .take_while(|e| e.tx == TxId(0))
            .map(as_log_entry)
            .collect();
        let mut rebuilt = World::from_genesis(&genesis)?;
        // Same value interning as this world, as the doctor rebuild does.
        rebuilt.set_rebuild_hash(self.indexes.content_hash);
        let mut txn = ReplayTxn::default();
        for entry in kept.iter().filter(|e| e.tx != TxId(0)) {
            if !txn.migration_expected.is_empty() {
                copies.insert(entry.id);
            }
            rebuilt.replay_entry(&as_log_entry(entry), &mut txn)?;
        }
        Ok(copies)
    }

    /// The path of a file-backed world's log; `None` in memory.
    pub(crate) fn log_path(&self) -> Option<&Path> {
        self.journal.log_path()
    }

    /// `Err` if the world refuses writes: a read-only world or an
    /// unacknowledged damaged-tail recovery (checked first, see
    /// [`World::accepts_writes`]), or a log append that failed earlier.
    fn writable(&self) -> Result<(), Error> {
        self.accepts_writes()?;
        self.journal.check_log()
    }

    /// `Err(ReadOnly)` for a world from [`World::open_read_only`], else
    /// `Err(RecoveryUnacknowledged)` until a damaged-tail cut is
    /// acknowledged (see [`World::acknowledge_recovery`]).
    fn accepts_writes(&self) -> Result<(), Error> {
        self.gate.accepts_writes()
    }

    /// Acknowledges what [`World::open`] cut off the log, allowing writes
    /// again.
    ///
    /// After an open that cut a damaged tail ([`CutCause::Damaged`]: bytes
    /// that look committed but fail their checksum, saved to the quarantine
    /// file), every write fails with [`Error::RecoveryUnacknowledged`]
    /// until this is called, so the author decides what the lost canon
    /// means before writing on top of it. [`World::recovery`] keeps
    /// reporting the cut afterwards. On any other world (torn cut, clean
    /// open, [`World::create`], [`World::in_memory`]) and when called again
    /// this does nothing. On a world from [`World::open_read_only`] it does
    /// nothing either: writes stay refused with [`Error::ReadOnly`].
    pub fn acknowledge_recovery(&mut self) {
        self.gate.acknowledge_recovery();
    }

    /// What opening the log did to recover it (see [`RecoveryReport`]).
    pub fn recovery(&self) -> &RecoveryReport {
        self.gate.recovery()
    }

    /// Where this handle's log stands in the file: the last committed
    /// transaction it has applied, with its byte range and its chained
    /// checksum. [`log::chain_tip_holds`] re-reads just that transaction to
    /// tell whether everything this handle applied is still what the file
    /// holds (another process may have cut and rewritten it). `None` for an
    /// in-memory world. A failed commit doesn't move it.
    pub fn chain_tip(&self) -> Option<log::ChainTip> {
        self.journal.chain_tip()
    }

    /// The last committed transaction. Every write that stores a fact
    /// advances it by one; a failed or 0-changed write leaves it unchanged.
    pub fn latest_tx(&self) -> TxId {
        self.journal.latest_tx()
    }

    /// The world's latest tick: the highest tick of any fact (assert or
    /// retract) at the latest canon, or `Tick(0)` if there is none. A fact
    /// deleted by a correction doesn't count.
    ///
    /// A tick-less write lands here (see [`World::begin`]). A prequel fact, a
    /// no-op or a failed write leaves it as it is; deleting the fact(s) at
    /// the highest tick lowers it to the next highest.
    pub fn latest_tick(&self) -> Tick {
        self.indexes.latest_tick()
    }

    /// Asserts that `entity.attr` has value `v` from `tick` on.
    ///
    /// One write is one auto-committed transaction (see [`Txn::assert`]):
    /// with no tick it lands on the world's latest tick. If `v` already
    /// holds at the write's tick, nothing is stored and the result reports
    /// `changed: 0`. Input is validated first either way. Fails, changing
    /// nothing, if the attribute is unknown ([`Error::UnknownAttribute`]),
    /// the entity or a `ref` value isn't defined ([`Error::UnknownEntity`]),
    /// the value has the wrong type ([`Error::WrongValueType`]), or the
    /// value can't be stored ([`Error::Value`]: NaN, indexed string over
    /// 256 bytes; [`Error::EntryTooLarge`]: a value whose `Revised` entry
    /// would exceed [`MAX_ENTRY_BYTES`](log::MAX_ENTRY_BYTES), which only an
    /// unindexed string can reach), or the opposite op (a retract) is already stored for
    /// this value at exactly this tick ([`Error::SameTickConflict`]; a
    /// repeated assert at that tick is a no-op instead).
    pub fn assert(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert, Precision::Exact)
    }

    /// [`World::assert`] with the tick's precision (vague time): the stored
    /// fact carries `p`, which [`View::history`] and [`View::as_of_detail`]
    /// report. Precision is never part of a key: the write is validated and
    /// judged exactly like [`World::assert`] (asserting a value that holds
    /// at the write's tick is "0 changed" whatever its precision, and
    /// records nothing).
    pub fn assert_with_precision(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
        p: Precision,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert, p)
    }

    /// [`World::retract`] with the tick's precision; judged exactly like
    /// [`World::retract`] (see [`World::assert_with_precision`]).
    pub fn retract_with_precision(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
        p: Precision,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Retract, p)
    }

    /// Retracts value `v` of `entity.attr` from `tick` on.
    ///
    /// One auto-committed transaction, defaulting the tick like
    /// [`World::assert`]. If `v` doesn't hold at the write's tick (never
    /// asserted, asserted only later, or already retracted), nothing is
    /// stored and the result reports `changed: 0`. Validated exactly like
    /// [`World::assert`], and additionally fails with
    /// [`Error::RetractEvent`] if the attribute is an event (whatever is
    /// stored; events are un-canoned by a correction), or with
    /// [`Error::SameTickConflict`] if an assert of this value is stored at
    /// exactly this tick.
    pub fn retract(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Retract, Precision::Exact)
    }

    /// Corrects the committed fact entry `entry` (author time): see
    /// [`Correction`]. One auto-committed transaction (see
    /// [`Txn::correct`]); the result lists the appended correction entries.
    ///
    /// `entry` may be a base fact or any `Revised` entry of its revision
    /// chain: it is resolved to the chain's base, every correction entry
    /// points at that base (`of`) with the chain's next revision (rev 1 for
    /// the first correction), and the correction acts on the chain's current
    /// fact. Readers at canon `>=` the correction's tx see the corrected
    /// canon; older canon still sees the old one.
    ///
    /// [`Correction::Delete`] appends a `DELETED` marker at the current
    /// fact's key: `changed: 1`, and `tick` is the key's tick. Events may be
    /// deleted (retracting one stays an error).
    ///
    /// [`Correction::Replace`] builds the new fact from the current one with
    /// the given fields replaced, validated like a write (a bidirectional
    /// pair renormalised, which may swap entity and value), and `tick` is its
    /// tick. Same key, only the op and/or the precision changed: one
    /// `Revised` entry (`changed: 1`). A moved key (tick or value): a
    /// `DELETED` marker at the old key, then the `Revised` at the new one,
    /// sharing one revision and keeping the precision unless one is given
    /// (`changed: 2`); if the same op is already current at the new key only the marker
    /// is written (`changed: 1`, and the chain counts as deleted). Nothing
    /// changed: a no-op (`changed: 0`, nothing appended). A new `Revised`
    /// assert of a `ref` triple with no edge interns one (listed in
    /// `edges`). The hold-based no-op rules of writes don't apply.
    ///
    /// Fails, changing nothing, with [`Error::UnknownEntry`] (no committed
    /// entry has that ID), [`Error::NotAFact`] (a `DEFINE`, `Commit` or
    /// `DELETED` entry), [`Error::AlreadyDeleted`] (the chain is currently
    /// deleted), [`Error::TooManyRevisions`], for a replace any error of
    /// [`World::assert`]'s validation (including [`Error::RetractEvent`] for
    /// a new retract of an event), [`Error::SameTickConflict`] if the
    /// opposite op is current at a moved fact's new key, or
    /// [`Error::EntryTooLarge`] if a correction entry carrying the current
    /// or the new value would exceed [`MAX_ENTRY_BYTES`](log::MAX_ENTRY_BYTES)
    /// (judged as for a write; for the current value only reachable
    /// through a crafted log).
    pub fn correct(&mut self, entry: EntryId, c: Correction) -> Result<WriteResult, Error> {
        let mut txn = self.begin_locked()?;
        let mut res = txn.correct(entry, c)?;
        let commit = txn.commit()?;
        res.entries = commit.entries;
        res.edges = commit.edges;
        res.warnings = commit.warnings;
        Ok(res)
    }

    /// The transaction number the next commit gets.
    fn next_tx(&self) -> TxId {
        self.journal.next_tx()
    }

    /// A read-only view of the world as of canon `tx`: only transactions
    /// `<= tx` are visible, so later commits (story changes, prequels, new
    /// values) don't change its answers. A `tx` past the latest is the
    /// latest; `TxId(0)` sees no facts.
    ///
    /// At the latest canon (`tx >= latest_tx()`) the view reads the
    /// latest-revision indexes (one seek per value, however many
    /// corrections there were); at an older canon it reads the main
    /// indexes, skipping entries newer than `tx`.
    pub fn at_canon(&self, tx: TxId) -> View<'_> {
        let canon = tx.min(self.latest_tx());
        View {
            world: self,
            canon,
            latest: canon == self.latest_tx(),
            work: Work::NONE,
        }
    }

    /// Whether value `v` of `entity.attr` holds at tick `t` (latest canon).
    /// Same as [`View::as_of_value`] on `at_canon(latest_tx())`.
    pub fn as_of_value(
        &self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        t: Tick,
    ) -> Result<bool, Error> {
        self.at_canon(self.latest_tx())
            .as_of_value(entity, attr, v, t)
    }

    /// Every entity (or edge) for which `attr = v` holds at tick `t`
    /// (latest canon). Same as [`View::lookup`] on `at_canon(latest_tx())`.
    pub fn lookup(&self, attr: &str, v: &Value, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.at_canon(self.latest_tx()).lookup(attr, v, t)
    }

    /// Every entity of node type `node_type` (latest canon). Same as
    /// [`View::entities_of_type`] on `at_canon(latest_tx())`.
    pub fn entities_of_type<'w>(
        &'w self,
        node_type: &str,
    ) -> Result<impl Iterator<Item = EntityId> + use<'w>, Error> {
        self.at_canon(self.latest_tx()).entities_of_type(node_type)
    }

    /// Every entity (or edge) with a value of the string attribute `attr`
    /// holding at tick `t` whose [`fold_for_search`] equals `text`'s
    /// (latest canon). Same as [`View::lookup_ci`] on
    /// `at_canon(latest_tx())`.
    pub fn lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.at_canon(self.latest_tx()).lookup_ci(attr, text, t)
    }

    /// Every value of `entity.attr` that holds at tick `t` (latest canon).
    /// Same as [`View::as_of`] on `at_canon(latest_tx())`.
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        self.at_canon(self.latest_tx()).as_of(entity, attr, t)
    }

    /// Every value of `entity.attr` holding at tick `t` (latest canon) with
    /// the change that makes it hold, its tick and precision. Same as
    /// [`View::as_of_detail`] on `at_canon(latest_tx())`.
    pub fn as_of_detail(
        &self,
        entity: EntityId,
        attr: &str,
        t: Tick,
    ) -> Result<Vec<Change>, Error> {
        self.at_canon(self.latest_tx())
            .as_of_detail(entity, attr, t)
    }

    /// Every `(entity, value)` pair of `attr` holding at tick `t` (latest
    /// canon). Same as [`View::attr_scan`] on `at_canon(latest_tx())`.
    pub fn attr_scan(&self, attr: &str, t: Tick) -> Result<Vec<(EntityId, Value)>, Error> {
        self.at_canon(self.latest_tx()).attr_scan(attr, t)
    }

    /// Size estimates of attribute `attr` for a query planner, over every
    /// fact entry ever committed for it (base facts and revised entries, any
    /// op, every canon; `DELETED` markers ignored), counted in stored
    /// orientation (a bidirectional pair once, under its smaller ID). They
    /// never shrink: a retract or a correction can only add. An inverse
    /// name reports its base attribute's stats.
    ///
    /// Cost: one hash lookup. Fails with [`Error::UnknownAttribute`] for a
    /// name that is neither an attribute nor an inverse name.
    pub fn attr_stats(&self, attr: &str) -> Result<AttrStats, Error> {
        let attr_id = self.catalog.attr_or_inverse_id(attr)?;
        Ok(self.indexes.attr_stats(attr_id))
    }

    /// The change points of `entity.attr` (latest canon).
    /// Same as [`View::history`] on `at_canon(latest_tx())`.
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        self.at_canon(self.latest_tx()).history(entity, attr)
    }

    /// The change points of `attr` across every entity and edge (latest
    /// canon). Same as [`View::history_scan`] on `at_canon(latest_tx())`.
    pub fn history_scan(&self, attr: &str) -> Result<Vec<(EntityId, Change)>, Error> {
        self.at_canon(self.latest_tx()).history_scan(attr)
    }

    /// The ticks where `entity.attr` holds `value` (latest canon).
    /// Same as [`View::holds_during`] on `at_canon(latest_tx())`.
    pub fn holds_during(
        &self,
        entity: EntityId,
        attr: &str,
        value: &Value,
    ) -> Result<IntervalSet, Error> {
        self.at_canon(self.latest_tx())
            .holds_during(entity, attr, value)
    }

    /// Every cardinality overlap of `attr` (latest canon).
    /// Same as [`View::overlaps`] on `at_canon(latest_tx())`.
    pub fn overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error> {
        self.at_canon(self.latest_tx()).overlaps(attr)
    }

    /// The neighbors of `entity` through `attr` at tick `t` (latest canon).
    /// Same as [`View::neighbors`] on `at_canon(latest_tx())`.
    pub fn neighbors(
        &self,
        entity: EntityId,
        attr: &str,
        dir: Direction,
        t: Tick,
    ) -> Result<Vec<(EntityId, EdgeId)>, Error> {
        self.at_canon(self.latest_tx())
            .neighbors(entity, attr, dir, t)
    }

    /// Every declared attribute, in ID (= declaration) order. Inverse names
    /// aren't attributes and aren't listed (they are on their attribute's
    /// [`AttrDef::inverse`]).
    pub fn attributes(&self) -> impl Iterator<Item = &AttrDef> {
        self.catalog.attributes.iter()
    }

    /// The declaration of attribute `name`, matched exactly. `None` for an
    /// unknown name and for an inverse name (see [`World::attributes`]).
    pub fn attribute(&self, name: &str) -> Option<&AttrDef> {
        self.catalog.attribute(name)
    }

    /// The name of attribute `id` (as stored in a log entry), or `None` if
    /// the schema declares no attribute with that ID.
    pub fn attr_name(&self, id: AttrId) -> Option<&str> {
        self.catalog.attr_name(id)
    }

    /// Whether this is Kala's own `system` (accounts) world: its schema
    /// declares the [`SYSTEM_NODE_TYPE`](crate::SYSTEM_NODE_TYPE) marker.
    /// Recognized by content, never by file name, so a renamed copy is still
    /// a system world. Shared write appliers refuse such a world unless
    /// their caller explicitly allows it (account management and restores).
    pub fn is_system(&self) -> bool {
        self.catalog
            .node_type_by_name
            .contains_key(crate::SYSTEM_NODE_TYPE)
    }

    /// Every declared node type name, in ID (= declaration) order.
    pub fn node_types(&self) -> impl Iterator<Item = &str> {
        self.catalog.node_types.iter().map(|nt| nt.name.as_str())
    }

    /// Every committed log entry, in entry-ID order: the same items
    /// [`World::entry`] returns for each ID, genesis included, `DEFINE`s
    /// and `Commit`s included. Entries of a transaction that was dropped or
    /// failed to commit are never listed. Not limited by canon (filter on
    /// [`EntryInfo::tx`] for that). Each item is cloned as it is yielded.
    pub fn entries(&self) -> impl Iterator<Item = EntryInfo> + '_ {
        self.journal.entries().iter().cloned()
    }
}

/// Whether a prepared fact may target a deprecated attribute. New asserts
/// and retracts refuse; corrections may revise existing history. Only the
/// internal, verified copy of a migration may append plain facts for a
/// deprecated edge property onto new edges. Private to the world module,
/// so no public write flag can bypass deprecation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeprecatedWrites {
    Refuse,
    Correction,
    MigrationCopy,
}

/// Checks an attribute's direction, inverse and reverse cardinality fit its
/// value type: a `ref` attribute has a direction, nothing else has one, an
/// inverse or a reverse cardinality, and a bidirectional attribute has
/// neither an inverse (its reverse is itself) nor a reverse cardinality (its
/// cardinality already covers both sides).
fn check_direction(def: &AttrDef) -> Result<(), Error> {
    let ok = match (def.value_type, def.direction) {
        (ValueType::Ref, Some(RefDirection::Directed)) => true,
        (ValueType::Ref, Some(RefDirection::Bidirectional)) => {
            def.inverse.is_none() && def.reverse_cardinality.is_none()
        }
        (ValueType::Ref, None) => false,
        (_, direction) => {
            direction.is_none() && def.inverse.is_none() && def.reverse_cardinality.is_none()
        }
    };
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidDirection(def.name.clone()))
    }
}

/// Checks that `v` has the attribute's declared type.
fn check_type(attr: &str, def: &AttrDef, v: &Value) -> Result<(), Error> {
    let got = v.value_type();
    if got != def.value_type {
        return Err(Error::WrongValueType {
            attr: attr.to_owned(),
            expected: def.value_type,
            got,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! White-box checks of the latest-revision indexes: the integration
    //! tests can only compare read paths, so these prove which index each
    //! path actually reads and that the indexes hold what they should.
    use super::*;
    use crate::schema::Cardinality;

    fn world() -> (World, EntityId, EntityId, EntityId) {
        let schema = Schema::new()
            .node_type("CHARACTER")
            .node_type("PLACE")
            .attribute(AttrDef::new(
                "located_in",
                ValueType::Ref,
                Kind::State,
                Cardinality::One,
            ))
            .attribute(
                AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many)
                    .bidirectional(),
            );
        let mut w = World::in_memory(schema).unwrap();
        let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
        let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
        let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
        (w, greg, mara, varrow)
    }

    /// `E_latest` / `R_latest` rebuilt from the main indexes: each key's
    /// current entry at the latest canon, `DELETED` keys left out.
    fn rebuilt(index: &Index, latest: TxId) -> LatestIndex {
        current_entries(index, &[], latest)
            .map(|(k, op)| (key::without_tx(k).to_vec(), op))
            .collect()
    }

    #[test]
    fn latest_indexes_match_a_rebuild_after_corrections() {
        let (mut w, greg, mara, varrow) = world();
        let v = Value::Ref(varrow);
        w.assert(greg, "located_in", v.clone(), Some(Tick(-300)))
            .unwrap();
        let r = w
            .retract(greg, "located_in", v.clone(), Some(Tick(-250)))
            .unwrap();
        w.correct(
            r.entries[0],
            Correction::Replace {
                value: None,
                tick: Some(Tick(-280)),
                op: None,
                precision: None,
            },
        )
        .unwrap();
        let a = w
            .assert(mara, "ally_of", Value::Ref(greg), Some(Tick(5)))
            .unwrap();
        w.correct(a.entries[0], Correction::Delete).unwrap();
        assert_eq!(
            w.indexes.e_latest,
            rebuilt(&w.indexes.e_index, w.latest_tx())
        );
        assert_eq!(
            w.indexes.r_latest,
            rebuilt(&w.indexes.r_index, w.latest_tx())
        );
        // the deleted -250 key and the deleted pair are absent, the main
        // indexes still hold their entries
        assert_eq!(w.indexes.e_latest.len(), 2);
        assert_eq!(w.indexes.r_latest.len(), 2);
        assert!(w.indexes.e_index.len() > w.indexes.e_latest.len());
    }

    #[test]
    fn world_reads_go_through_e_latest_and_main_view_does_not() {
        let (mut w, greg, _, varrow) = world();
        let v = Value::Ref(varrow);
        w.assert(greg, "located_in", v.clone(), Some(Tick(-300)))
            .unwrap();
        // Tamper with E_latest only: the latest path now disagrees with the
        // main indexes, which proves which one each route reads.
        for op in w.indexes.e_latest.values_mut() {
            *op = Op::Retract;
        }
        let at = Tick(0);
        assert!(!w.as_of_value(greg, "located_in", &v, at).unwrap());
        assert!(w.as_of(greg, "located_in", at).unwrap().is_empty());
        assert_eq!(w.history(greg, "located_in").unwrap()[0].op, Op::Retract);
        assert!(
            !w.at_canon(w.latest_tx())
                .as_of_value(greg, "located_in", &v, at)
                .unwrap()
        );
        let main = w.at_canon_main(w.latest_tx());
        assert!(main.as_of_value(greg, "located_in", &v, at).unwrap());
        assert_eq!(main.as_of(greg, "located_in", at).unwrap(), vec![v.clone()]);
        assert_eq!(main.history(greg, "located_in").unwrap()[0].op, Op::Assert);
    }

    #[test]
    fn world_neighbors_in_go_through_r_latest() {
        let (mut w, greg, _, varrow) = world();
        w.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(1)))
            .unwrap();
        w.indexes.r_latest.clear();
        let at = Tick(1);
        assert!(
            w.neighbors(varrow, "located_in", Direction::In, at)
                .unwrap()
                .is_empty()
        );
        let main = w.at_canon_main(w.latest_tx());
        assert_eq!(
            main.neighbors(varrow, "located_in", Direction::In, at)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn an_older_canon_reads_the_main_indexes() {
        let (mut w, greg, _, varrow) = world();
        let v = Value::Ref(varrow);
        w.assert(greg, "located_in", v.clone(), Some(Tick(1)))
            .unwrap();
        let first = w.latest_tx();
        w.assert(greg, "located_in", Value::Ref(greg), Some(Tick(2)))
            .unwrap();
        w.indexes.e_latest.clear();
        assert!(
            w.at_canon(first)
                .as_of_value(greg, "located_in", &v, Tick(1))
                .unwrap()
        );
        assert!(!w.as_of_value(greg, "located_in", &v, Tick(1)).unwrap());
    }
    /// The precisions are the payload half of `E` / `E_latest` rows, kept
    /// beside them: doctor's rebuild-and-compare must catch a wrong one in
    /// either map, as an `IndexMismatch` of that index.
    #[test]
    fn doctor_reports_a_tampered_precision_as_an_index_mismatch() {
        use crate::doctor::{DoctorOptions, Finding, IndexName, doctor};
        let (mut w, greg, _, varrow) = world();
        w.assert_with_precision(
            greg,
            "located_in",
            Value::Ref(varrow),
            Some(Tick(1)),
            Precision::Year,
        )
        .unwrap();
        assert!(doctor(&w, &DoctorOptions::default()).is_clean());
        let mismatch_in = |w: &World, want: IndexName| {
            doctor(w, &DoctorOptions::default())
                .findings
                .iter()
                .any(|f| matches!(f, Finding::IndexMismatch { index, .. } if *index == want))
        };
        let mut main = w.indexes.e_precision.clone();
        for p in w.indexes.e_precision.values_mut() {
            *p = Precision::Day;
        }
        assert!(mismatch_in(&w, IndexName::E));
        w.indexes.e_precision.clear();
        assert!(mismatch_in(&w, IndexName::E));
        std::mem::swap(&mut w.indexes.e_precision, &mut main);
        for p in w.indexes.e_latest_precision.values_mut() {
            *p = Precision::Century;
        }
        assert!(mismatch_in(&w, IndexName::ELatest));
        assert!(!mismatch_in(&w, IndexName::E));
    }
}
