//! A Kala world: one story timeline, its schema and its entities.
//!
//! Facts live in the in-memory `E` index (layout in the private `key`
//! module). A world is either in memory only ([`World::in_memory`]) or backed
//! by a log file ([`World::create`], format in [`crate::log`]), which is then
//! the source of truth: a committed [`Txn`] is appended as its `DEFINE`
//! entries, its facts and a `COMMIT`, and fsynced, before the in-memory
//! tables change. Every operation first validates into a definition
//! (`EntityDef`, `FactDef`, `EdgeDef`) without touching the world, then
//! applies it, so a failed operation changes nothing.
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

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

#[cfg(feature = "test-hooks")]
use crate::doctor::{IndexName, Tamper};
use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, IdError, MAX_NODE_TYPE, NodeTypeId};
use crate::key::{self, ValueRef};
use crate::log::{
    self, EncodedTxn, EntryBody, EntryId, LogEntry, LogWriter, NewEntry, SurveyedTxn,
};
use crate::overlap::{
    Overlap, Side, Warnings, find_overlaps, find_overlaps_touching, value_intervals,
};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection, Schema};
use crate::tick::{Precision, Tick};
use crate::value::{Value, ValueType, fold_for_search};
use crate::value_table::{ContentHash, ValueTable, fnv1a64};

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
    /// carry instead of `value` (see [`World::value_ref`]); `None` for
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

/// An `E`-layout index with its value references resolved, and the stored
/// keys that couldn't be (see [`World::resolved_e_index`]).
pub(crate) type ResolvedIndex<'i, V> = (Cow<'i, BTreeMap<Vec<u8>, V>>, Vec<Vec<u8>>);

/// An `E`- or `R`-layout index: full key -> what the entry says. Used for
/// the committed indexes and for a transaction's staged entries, so one seek
/// routine serves both.
pub(crate) type Index = BTreeMap<Vec<u8>, Mark>;

/// A latest-revision index (`E_latest` / `R_latest`): an `E`- or `R`-layout
/// key without its `^tx` (`E|e|a|v|^tick`, `R|t|a|s|^tick`) -> the op of
/// that key's current entry at the latest canon. A key whose current entry
/// is a `DELETED` marker is absent, so the first key at or after `^T` in a
/// history is the answer at `T`: one seek, nothing to skip.
pub(crate) type LatestIndex = BTreeMap<Vec<u8>, Op>;

/// The tick precisions of an `E`-layout index (main keys with `^tx`, or
/// latest-revision keys without), by the same key as the entry they belong
/// to. Sparse: only facts whose precision isn't [`Precision::Exact`] have a
/// row, so a world that never writes a vague tick holds none. Precision is
/// a payload, never part of a key: the indexes themselves are the same
/// whatever the precisions are.
pub(crate) type Precisions = BTreeMap<Vec<u8>, Precision>;

/// One entry a transaction staged, in write order: the order of its log
/// entries.
#[derive(Debug)]
enum Staged {
    /// A fact (assert or retract).
    Fact(FactDef),
    /// A `DELETED` marker: revision `rev` of the chain of base entry `of`,
    /// at the key of `fact`, the chain's current fact.
    Deleted {
        of: EntryId,
        rev: u32,
        fact: FactDef,
    },
    /// A revised fact: revision `rev` of the chain of base entry `of`, its
    /// new current fact.
    Revised {
        of: EntryId,
        rev: u32,
        fact: FactDef,
    },
}

impl Staged {
    /// The key the entry is stored at and what it says there.
    fn key_and_mark(&self) -> (&FactDef, Mark) {
        match self {
            Staged::Fact(fact) | Staged::Revised { fact, .. } => (fact, Mark::from(fact.op)),
            Staged::Deleted { fact, .. } => (fact, Mark::Deleted),
        }
    }
}

/// The correction state of one revision chain (keyed by its base entry in
/// [`World::chains`]). A chain no correction has touched has none: see
/// [`Chain::untouched`].
#[derive(Clone, Copy, Debug)]
struct Chain {
    /// The chain's last revision (0 = only the base entry).
    rev: u32,
    /// The entry holding the chain's current fact (the base or its last
    /// `Revised`), or `None` if the last correction deleted it.
    current: Option<EntryId>,
}

/// What a correction acts on (from [`World::correction_target`]).
#[derive(Debug)]
struct Target {
    /// The chain's base entry.
    base: EntryId,
    chain: Chain,
    /// The chain's current fact, as stored.
    fact: FactDef,
    /// Its value, decoded.
    value: Value,
}

impl Chain {
    /// The state of the chain of base entry `base` before any correction.
    fn untouched(base: EntryId) -> Chain {
        Chain {
            rev: 0,
            current: Some(base),
        }
    }
}

/// Outcome of judging a valid write against the stored facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// The write changes something: store it.
    Store,
    /// The write changes nothing ("0 changed").
    Noop,
}

/// One declared node type and its sequence counter.
#[derive(Debug)]
struct NodeType {
    name: String,
    /// The tx that defined each entity of this type: entry `i` is the
    /// entity with sequence `i + 1` (sequences are assigned without gaps,
    /// so the length is the last sequence, 0 = none). Non-decreasing,
    /// because entities are defined in tx order: the entities visible at a
    /// canon are a prefix.
    defined_in: Vec<TxId>,
}

impl NodeType {
    /// Sequence number of the last entity created with this type (0 = none).
    fn last_seq(&self) -> u64 {
        // A Vec's length always fits a u64.
        self.defined_in.len() as u64
    }
}

/// A validated entity definition, ready to apply. Later this is the payload
/// of the log's `DEFINE` entry.
#[derive(Debug)]
struct EntityDef {
    id: EntityId,
    handle: Option<String>,
}

/// The `(source, attr, target)` triple an edge is interned under.
type Triple = (EntityId, AttrId, EntityId);

/// A validated edge definition, ready to apply: the payload of the log's
/// `DefineEdge` entry.
#[derive(Debug)]
struct EdgeDef {
    id: EdgeId,
    source: EntityId,
    attr: AttrId,
    target: EntityId,
}

impl EdgeDef {
    fn triple(&self) -> Triple {
        (self.source, self.attr, self.target)
    }
}

/// IDs (entities and edges) a transaction has defined but not committed.
/// Defining one only stages it; the world's ID tables change on commit.
#[derive(Debug, Default)]
struct StagedIds {
    /// In creation order: the order of their `DEFINE` entries.
    defs: Vec<EntityDef>,
    /// Last staged sequence number per node type. Staged sequences of a type
    /// continue the world's without gaps, so `1..=last` are all defined.
    last_seq: HashMap<NodeTypeId, u64>,
    handles: HashSet<String>,
    /// Edges interned by this transaction, in interning order: their
    /// sequences continue the world's without gaps.
    edges: Vec<EdgeDef>,
    edge_by_triple: HashMap<Triple, EdgeId>,
    /// Unindexed string values this transaction's staged entries use that
    /// the world's value table doesn't hold yet, with the references they
    /// will get: they enter the world's table only when the transaction
    /// commits, so a dropped or poisoned one leaves nothing behind.
    values: ValueTable,
}

impl StagedIds {
    fn is_empty(&self) -> bool {
        self.defs.is_empty() && self.edges.is_empty()
    }

    fn push(&mut self, def: EntityDef) {
        self.last_seq.insert(def.id.node_type(), def.id.seq());
        if let Some(h) = &def.handle {
            self.handles.insert(h.clone());
        }
        self.defs.push(def);
    }

    fn push_edge(&mut self, def: EdgeDef) {
        self.edge_by_triple.insert(def.triple(), def.id);
        self.edges.push(def);
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

/// A decoded copy of the stored indexes, from [`World::index_dump`].
/// Rows are in key order (within one key, newest tx first), `DELETED`
/// markers included.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDump {
    /// `E` rows: `(entity, attr, value, tick, tx, mark)`.
    pub e: Vec<(EntityId, AttrId, Value, Tick, TxId, Mark)>,
    /// `R` rows: `(target, attr, source, tick, tx, mark)`.
    pub r: Vec<(EntityId, AttrId, EntityId, Tick, TxId, Mark)>,
    /// `V` rows: `(attr, value, entity, tick, tx, mark)`.
    pub v: Vec<(AttrId, Value, EntityId, Tick, TxId, Mark)>,
    /// `VFold` rows: `(attr, folded, original, entity, tick, tx, mark)`.
    pub v_fold: Vec<(AttrId, String, Value, EntityId, Tick, TxId, Mark)>,
}

/// A world: schema, entities and (later) their facts over story time.
#[derive(Debug)]
pub struct World {
    /// Indexed by `NodeTypeId.0`.
    node_types: Vec<NodeType>,
    node_type_by_name: HashMap<String, NodeTypeId>,
    entity_by_handle: HashMap<String, EntityId>,
    /// The reverse of `entity_by_handle`, for [`World::handle_of`].
    handle_by_entity: HashMap<EntityId, String>,
    /// Indexed by `AttrId.0`.
    attributes: Vec<AttrDef>,
    attr_by_name: HashMap<String, AttrId>,
    /// Inverse name -> the directed attribute it reverses. Not attribute
    /// names: every API except [`View::neighbors`] treats them as unknown
    /// attributes.
    inverse_by_name: HashMap<String, AttrId>,
    /// Interned edges: entry `i` is the triple of the edge with seq `i + 1`
    /// (edge sequences are assigned without gaps).
    edges: Vec<Triple>,
    /// The transaction that defined each edge, parallel to `edges` (so
    /// ascending): what [`View::edge`] checks against its canon.
    edges_defined_in: Vec<TxId>,
    edge_by_triple: HashMap<Triple, EdgeId>,
    /// The last committed transaction (0 = none yet).
    latest_tx: TxId,
    /// Forward index `E|entity|attr|value|^tick|^tx -> mark` (layout in
    /// [`crate::key`]): every stored fact and `DELETED` marker. With
    /// `r_index`, the in-memory copy of the log's entries (milestone 0);
    /// both are rebuilt from the log on open.
    e_index: Index,
    /// Reverse index `R|target|attr|source|^tick|^tx -> mark`: one entry
    /// per stored entry at a `ref` key, written by [`World::index_entry`]
    /// together with its `E` entry. Read by [`View::neighbors`] for
    /// [`Direction::In`].
    r_index: Index,
    /// `E_latest`: every `E` key's current entry at the latest canon, keys
    /// whose current entry is a `DELETED` marker left out (see
    /// [`LatestIndex`]). Derived from `e_index`; written only by
    /// [`World::index_entry`]. Serves every read at the latest canon.
    /// Crate-visible so doctor can compare it against a rebuild.
    pub(crate) e_latest: LatestIndex,
    /// `R_latest`: the same for `r_index`.
    pub(crate) r_latest: LatestIndex,
    /// The precision of every `E` entry that is a fact with a vague tick,
    /// by its `E` key (see [`Precisions`]): the payload half of an `E` row.
    /// Written only by [`World::index_entry`], derived and rebuilt with `E`;
    /// doctor compares it as part of `E`.
    e_precision: Precisions,
    /// The same for `E_latest`: the precision of each key's current fact at
    /// the latest canon, if vague. Compared by doctor as part of `E_latest`.
    e_latest_precision: Precisions,
    /// Value index `V|attr|value|entity|^tick|^tx -> mark`: a copy of every
    /// `E` entry of an indexed, non-ref attribute (see
    /// [`World::value_indexed`]), keyed by value so "every entity with
    /// `attr = v`" is a prefix walk. Written only by [`World::index_entry`]
    /// next to its `E` entry, so it is derived and rebuilt with `E`.
    v_index: Index,
    /// `V_latest`: the same for `v_index` as `E_latest` is for `e_index`.
    /// Serves [`View::lookup`] at the latest canon.
    pub(crate) v_latest: LatestIndex,
    /// Folded value index `F|attr|fold|original|entity|^tick|^tx -> mark`
    /// (layout in [`crate::key`]): a copy of every `V` entry of a string
    /// attribute, keyed by the value's [`fold_for_search`] form, so "every
    /// entity named edwin, in any case" is a prefix walk. Written only by
    /// [`World::index_entry`] next to its `V` entry; derived and rebuilt
    /// with `E`.
    vfold_index: Index,
    /// `VFold_latest`: the same for `vfold_index` as `E_latest` is for
    /// `e_index`. Serves [`View::lookup_ci`] at the latest canon.
    pub(crate) vfold_latest: LatestIndex,
    /// Every unindexed string value an `E` key refers to, by reference and
    /// by content (see [`crate::value_table`]). Derived like the indexes:
    /// filled only by [`World::index_entry`] on commit and replay.
    values: ValueTable,
    /// The content hash of the value references ([`fnv1a64`]; a test hook
    /// may replace it, see `World::set_content_hash`).
    content_hash: ContentHash,
    /// How many keys hold a fact (assert or retract) at the latest canon,
    /// per tick: a key whose current entry is a `DELETED` marker isn't
    /// counted. The last tick is the world's latest tick. Derived from the
    /// indexes; maintained by [`World::index_entry`] on commit and replay.
    tick_counts: BTreeMap<Tick, u64>,
    /// The correction state of every revision chain a correction touched,
    /// by base entry. Derived from the log; maintained on commit and replay.
    chains: HashMap<EntryId, Chain>,
    /// Every committed log entry, in log order: entry `i` has ID `i + 1`.
    /// Both kinds of world keep them (milestone 0), and an in-memory world
    /// numbers them exactly as a file-backed one, genesis included. Doctor
    /// replays these for an in-memory world.
    entries: Vec<EntryInfo>,
    /// The log file, for a file-backed world; `None` in memory.
    log: Option<LogWriter>,
    /// What opening the log cut off (default unless [`World::open`] cut a
    /// torn tail).
    recovery: RecoveryReport,
    /// Writes are refused with [`Error::RecoveryUnacknowledged`]: the open
    /// cut a damaged tail and [`World::acknowledge_recovery`] hasn't been
    /// called yet. Never set for a torn cut, a clean open or a new world.
    recovery_unacknowledged: bool,
    /// Opened by [`World::open_read_only`]: every write is refused with
    /// [`Error::ReadOnly`]. Such a world has no `log`.
    read_only: bool,
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
        let names = schema.node_types();
        let mut node_types = Vec::with_capacity(names.len());
        let mut node_type_by_name = HashMap::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            // IDs are assigned in declaration order, so the first one that
            // doesn't fit is exactly MAX_NODE_TYPE + 1.
            if i > usize::from(MAX_NODE_TYPE) {
                return Err(IdError::NodeTypeOutOfRange(MAX_NODE_TYPE + 1).into());
            }
            if name.is_empty() {
                return Err(Error::InvalidName(name.clone()));
            }
            let id = NodeTypeId(i as u16); // i <= MAX_NODE_TYPE, checked above
            if node_type_by_name.insert(name.clone(), id).is_some() {
                return Err(Error::DuplicateName(name.clone()));
            }
            node_types.push(NodeType {
                name: name.clone(),
                defined_in: Vec::new(),
            });
        }
        let defs = schema.attributes();
        let mut attr_by_name = HashMap::with_capacity(defs.len());
        let mut inverse_by_name = HashMap::new();
        for (i, def) in defs.iter().enumerate() {
            if def.name.is_empty() {
                return Err(Error::InvalidName(def.name.clone()));
            }
            // More than u32::MAX attributes can't be keyed; report it as a
            // bad schema rather than wrapping into another attribute's ID.
            let id = AttrId(u32::try_from(i).map_err(|_| Error::InvalidName(def.name.clone()))?);
            if attr_by_name.insert(def.name.clone(), id).is_some()
                || inverse_by_name.contains_key(&def.name)
            {
                return Err(Error::DuplicateName(def.name.clone()));
            }
            check_direction(def)?;
            if let Some(inverse) = &def.inverse {
                if inverse.is_empty() {
                    return Err(Error::InvalidName(inverse.clone()));
                }
                // Inverse names share the attribute namespace (in both
                // directions), so a name always means one thing.
                if attr_by_name.contains_key(inverse)
                    || inverse_by_name.insert(inverse.clone(), id).is_some()
                {
                    return Err(Error::DuplicateName(inverse.clone()));
                }
            }
        }
        let mut world = World {
            node_types,
            node_type_by_name,
            entity_by_handle: HashMap::new(),
            handle_by_entity: HashMap::new(),
            attributes: defs.to_vec(),
            attr_by_name,
            inverse_by_name,
            edges: Vec::new(),
            edges_defined_in: Vec::new(),
            edge_by_triple: HashMap::new(),
            latest_tx: TxId(0),
            e_index: BTreeMap::new(),
            r_index: BTreeMap::new(),
            e_latest: BTreeMap::new(),
            r_latest: BTreeMap::new(),
            e_precision: BTreeMap::new(),
            e_latest_precision: BTreeMap::new(),
            v_index: BTreeMap::new(),
            v_latest: BTreeMap::new(),
            vfold_index: BTreeMap::new(),
            vfold_latest: BTreeMap::new(),
            values: ValueTable::default(),
            content_hash: fnv1a64,
            tick_counts: BTreeMap::new(),
            chains: HashMap::new(),
            entries: Vec::new(),
            log: None,
            recovery: RecoveryReport::default(),
            recovery_unacknowledged: false,
            read_only: false,
        };
        let node_types = world
            .node_types
            .iter()
            .enumerate()
            .map(|(i, nt)| NewEntry::NodeType {
                // In range: refused above if more than MAX_NODE_TYPE + 1 types.
                id: NodeTypeId(i as u16),
                name: &nt.name,
            });
        let attrs = world
            .attributes
            .iter()
            .enumerate()
            .map(|(i, def)| NewEntry::Attr {
                // In range: refused above if more than u32::MAX + 1 attributes.
                id: AttrId(i as u32),
                def,
            });
        let genesis = log::encode_txn(EntryId(1), TxId(0), node_types.chain(attrs))?;
        world.record(genesis.entries.iter().cloned(), TxId(0));
        Ok((world, genesis))
    }

    /// Keeps the entries of committed transaction `tx` (in log order, right
    /// after the ones already kept) for [`World::entry`].
    fn record(&mut self, entries: impl IntoIterator<Item = LogEntry>, tx: TxId) {
        for e in entries {
            self.record_one(e, tx);
        }
    }

    /// Keeps one entry of committed transaction `tx` (see [`World::record`]).
    fn record_one(&mut self, e: LogEntry, tx: TxId) {
        // `World::entry` finds an entry by position, so IDs must be
        // consecutive from 1: the log scan and `encode_txn` from
        // `next_entry` guarantee it.
        debug_assert_eq!(
            e.id,
            self.next_entry(),
            "invariant: entries are kept in log order without gaps"
        );
        self.entries.push(EntryInfo {
            id: e.id,
            tx,
            body: e.body,
        });
    }

    /// The ID the next appended entry gets.
    fn next_entry(&self) -> EntryId {
        // A Vec's length always fits a u64.
        EntryId(self.entries.len() as u64 + 1)
    }

    /// The committed entry with ID `id`: its tx and what it says, for every
    /// kind of entry (`DEFINE`s and `Commit`s included), exactly as
    /// [`crate::log::read_log`] returns it. `None` for an ID no committed
    /// entry has (0, past the end, or an entry of a transaction that was
    /// dropped or failed to commit). Not limited by canon: every committed
    /// entry is visible.
    pub fn entry(&self, id: EntryId) -> Option<EntryInfo> {
        self.kept_entry(id).cloned()
    }

    /// The kept entry with ID `id` (see [`World::entry`]), borrowed.
    fn kept_entry(&self, id: EntryId) -> Option<&EntryInfo> {
        let i = usize::try_from(id.0.checked_sub(1)?).ok()?;
        match self.entries.get(i) {
            Some(e) if e.id == id => Some(e),
            // Only a doctor rebuild keeps entries with ID gaps (it leaves
            // refused entries out); they still ascend, so search by ID.
            _ => {
                let i = self.entries.binary_search_by_key(&id, |e| e.id).ok()?;
                self.entries.get(i)
            }
        }
    }

    /// Creates a new world backed by a new log file at `path`.
    ///
    /// The schema is validated exactly as by [`World::in_memory`] before the
    /// file system is touched. Then the file is created (never overwritten:
    /// an existing path fails with [`Error::Io`] of kind `AlreadyExists`),
    /// the header and the schema are written as the genesis transaction
    /// tx#0, and the file and its directory are fsynced. Like an in-memory
    /// world, the new world's [`latest_tx`](World::latest_tx) is `TxId(0)`.
    ///
    /// The world holds an exclusive advisory lock on the file for its whole
    /// lifetime (taken right after creating it, before writing), so a
    /// [`World::open`] of the same file meanwhile fails with
    /// [`Error::WorldLocked`]. Advisory means it keeps out other Kala
    /// worlds, not other programs.
    ///
    /// Every later commit is appended to the file and fsynced before it
    /// becomes visible. If an append fails, the commit fails with
    /// [`Error::Io`], the world is left as it was, and every later write
    /// fails with [`Error::Io`] too. If only the fsync failed, the
    /// transaction's bytes are first cut off the file again (best effort),
    /// so the failed commit doesn't reappear on reopen.
    pub fn create(path: &Path, schema: Schema) -> Result<World, Error> {
        let (mut world, genesis) = World::with_genesis(schema)?;
        world.log = Some(LogWriter::create(path, &genesis)?);
        Ok(world)
    }

    /// Opens the world backed by the existing log file at `path` by
    /// replaying its committed transactions: the schema from the genesis
    /// transaction tx#0, then every later transaction's entity `DEFINE`s
    /// and facts, in log order. The rebuilt world answers every read, at
    /// every canon, exactly as the world that wrote the log did, and later
    /// commits are appended to the same file. A clean log is not modified.
    ///
    /// Fails with [`Error::Io`] if the file can't be opened (`NotFound` if
    /// it doesn't exist; it is never created), [`Error::BadMagic`] or
    /// [`Error::UnsupportedVersion`] for a foreign header, or
    /// [`Error::Corrupt`] if the log has no committed genesis transaction or
    /// a committed transaction that a live write could not have produced
    /// (an ID used before its `DEFINE`, a value of the wrong type, a no-op
    /// fact, an edge `DEFINE` with no assert of its triple in its
    /// transaction, a transaction with nothing but its `Commit`, two entries
    /// at one key in one transaction, a `DELETED` or `Revised` entry that
    /// doesn't point at an earlier base fact or skips a revision, a
    /// `DELETED` marker not at its chain's current key or of an already
    /// deleted chain, a `Revised` that changes nothing at the current key,
    /// sits at another key without directly following its rev's `DELETED`
    /// marker, or lands on a key with a current entry, ...).
    ///
    /// The file is locked exclusively (an advisory lock, held for the
    /// world's lifetime) before it is read. If another world holds the lock
    /// this fails with [`Error::WorldLocked`] and the file is untouched: no
    /// cut, no quarantine.
    ///
    /// A torn tail (a last transaction cut short by a crash mid-append, or a
    /// zero-filled tail) or a damaged one (a last transaction whose bytes
    /// are all there but fail its body checksum, with no valid begin record
    /// after it; exact rules in [`crate::log`]) is cut off the file and
    /// fsynced once the rest has replayed cleanly; the world then continues
    /// entry IDs and tx from the last committed transaction. Before the cut,
    /// the cut bytes are saved and fsynced to `<path>.torn-<valid_len>`
    /// (with `-1`, `-2`, ... added if that exists); if saving them fails,
    /// this fails with [`Error::Io`] and the log is not cut.
    /// [`World::recovery`] reports what was cut, why and where it was saved.
    /// Any other damage, in any transaction, is [`Error::Corrupt`] instead,
    /// with the file untouched.
    pub fn open(path: &Path) -> Result<World, Error> {
        let (mut world, writer, recovery) = LogWriter::open(path, World::replay)?;
        world.log = Some(writer);
        world.recovery_unacknowledged = recovery.cause == Some(CutCause::Damaged);
        world.recovery = recovery;
        Ok(world)
    }

    /// Opens the world in the existing log file at `path` for reading only:
    /// a snapshot of its committed transactions at the moment of the call,
    /// replayed exactly as [`World::open`] replays them, so every read
    /// answers as a `World::open` of the same bytes would. Commits appended
    /// to the file afterwards (by a writer world) are not seen; open again
    /// to see them.
    ///
    /// Takes no lock, so it succeeds while another world (from
    /// [`World::open`] or [`World::create`]) holds the file, and it never
    /// keeps a writer out. It never modifies the file system: a torn or
    /// damaged tail is left in the file and only left out in memory, and
    /// [`World::recovery`] reports what `World::open` would cut
    /// (`bytes_cut`, `cause`) with `quarantine: None`.
    ///
    /// Every write (create, assert, retract, correct, a [`Txn`] op or
    /// commit, a no-op included) fails with [`Error::ReadOnly`] and appends
    /// nothing; [`World::acknowledge_recovery`] has no effect on that.
    ///
    /// Fails as [`World::open`] does short of locking and cutting:
    /// [`Error::Io`] if the file can't be read (`NotFound` if it doesn't
    /// exist; it is never created), [`Error::BadMagic`] or
    /// [`Error::UnsupportedVersion`] for a foreign header, or
    /// [`Error::Corrupt`] for any damage other than a torn or damaged tail.
    pub fn open_read_only(path: &Path) -> Result<World, Error> {
        let (mut world, recovery) = LogWriter::read_committed(path, World::replay)?;
        world.read_only = true;
        world.recovery = recovery;
        Ok(world)
    }

    /// Rebuilds a world from the entries of a log's committed transactions.
    fn replay(entries: &[LogEntry]) -> Result<World, Error> {
        World::replay_with_hash(entries, fnv1a64)
    }

    /// [`World::replay`] with value references hashed by `hash`.
    fn replay_with_hash(entries: &[LogEntry], hash: ContentHash) -> Result<World, Error> {
        // Split off the genesis transaction: everything up to the first
        // Commit. `read_log`'s scan guarantees the first Commit is tx#0.
        let genesis_end = entries
            .iter()
            .position(|e| matches!(e.body, EntryBody::Commit { .. }))
            .ok_or_else(|| Error::Corrupt("no committed genesis transaction".to_owned()))?;
        let mut world = World::from_genesis(&entries[..=genesis_end])?;
        world.set_rebuild_hash(hash);
        let mut txn = ReplayTxn::default();
        for entry in &entries[genesis_end + 1..] {
            world.replay_entry(entry, &mut txn)?;
        }
        Ok(world)
    }

    /// A world holding just the genesis transaction `genesis` (its schema
    /// `DEFINE`s, then its `Commit`), as a log has it. Fails with
    /// [`Error::Corrupt`] if those entries don't declare a valid schema.
    pub(crate) fn from_genesis(genesis: &[LogEntry]) -> Result<World, Error> {
        let defines = genesis.split_last().map_or(&[][..], |(_, d)| d);
        let mut world = World::in_memory(genesis_schema(defines)?)
            .map_err(|e| Error::Corrupt(format!("genesis schema: {e}")))?;
        // Keep the genesis entries as the file has them, not as this build
        // would re-encode them: `entry` reports what the log says. Pushed
        // as given, not through `record_one`: `World::open`'s scan has
        // already refused entry IDs that don't start at 1, but doctor's
        // survey reports them (`EntryIdGap`) and rebuilds anyway, and a
        // rebuild copes with ascending IDs that have gaps (see
        // `World::kept_entry`).
        world.entries.clear();
        world.entries.extend(genesis.iter().map(|e| EntryInfo {
            id: e.id,
            tx: TxId(0),
            body: e.body.clone(),
        }));
        Ok(world)
    }

    /// The content hash this world's value references use: doctor rebuilds
    /// a world with the same one, so the two assign the same references.
    pub(crate) fn content_hash(&self) -> ContentHash {
        self.content_hash
    }

    /// Sets the content hash of a world being rebuilt, before any fact is
    /// applied (a world holding just its genesis).
    pub(crate) fn set_rebuild_hash(&mut self, hash: ContentHash) {
        debug_assert!(
            self.values.is_empty(),
            "invariant: the content hash is set before any value is referenced"
        );
        self.content_hash = hash;
    }

    /// Replaces the content hash of this world's value references with `f`
    /// and rebuilds its indexes and value table from its kept entries, as a
    /// replay with that hash would: a test hook, so a forced-collision world
    /// (`|_| 0`) can be checked to answer exactly like a normal one.
    /// [`crate::doctor`] rebuilds this world with `f` too. Only this World
    /// object changes: nothing is logged, and a reopen uses the real hash.
    /// Not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn set_content_hash(&mut self, f: fn(&[u8]) -> u64) {
        let entries: Vec<LogEntry> = self
            .entries
            .iter()
            .map(|e| LogEntry {
                id: e.id,
                body: e.body.clone(),
            })
            .collect();
        let rebuilt = World::replay_with_hash(&entries, f)
            .expect("invariant: a world's kept entries replay cleanly");
        // The rebuild holds the same schema, IDs and entries; what is kept
        // from this World is what isn't derived from the log.
        let log = self.log.take();
        let recovery = std::mem::take(&mut self.recovery);
        let unacknowledged = self.recovery_unacknowledged;
        let read_only = self.read_only;
        *self = rebuilt;
        self.log = log;
        self.recovery = recovery;
        self.recovery_unacknowledged = unacknowledged;
        self.read_only = read_only;
    }

    /// Applies one entry of a transaction after the genesis. Facts are
    /// keyed at the tx of the `Commit` ending their transaction; since the
    /// tx isn't known until then, they're keyed at `latest_tx + 1`, which
    /// the scan guarantees is the Commit's tx. `txn` tracks the rules that
    /// span the current transaction and is reset by its `Commit`.
    fn replay_entry(&mut self, entry: &LogEntry, txn: &mut ReplayTxn) -> Result<(), Error> {
        // The scan guarantees the transaction's Commit has tx `next_tx`.
        let tx = self.next_tx();
        self.apply_entry(entry, txn)
            .map_err(|fault| Error::Corrupt(format!("entry#{}: {fault}", entry.id.0)))?;
        self.record_one(entry.clone(), tx);
        Ok(())
    }

    /// Doctor's replay of one whole transaction `tx` (after the genesis):
    /// like [`World::replay_entry`] for each entry, but an entry that fails
    /// is passed to `refused` with its [`ReplayFault`], left out (not
    /// applied, not kept) and the replay goes on. The transaction counts as
    /// committed at `tx` whatever happened to its entries, so the next one
    /// replays at its own tx even after a skipped transaction (a gap).
    ///
    /// Entries kept this way may have ID gaps; [`World::kept_entry`] copes.
    /// Only a doctor rebuild, never a live world, takes this path.
    pub(crate) fn replay_txn_collecting(
        &mut self,
        tx: TxId,
        entries: &[LogEntry],
        mut refused: impl FnMut(EntryId, ReplayFault),
    ) {
        if tx <= self.latest_tx {
            // A scan only hands over ascending txs; refuse rather than rewind.
            if let Some(first) = entries.first() {
                refused(
                    first.id,
                    ReplayFault::Invalid(format!("{tx:?} after {:?}", self.latest_tx)),
                );
            }
            return;
        }
        // Facts are keyed at `next_tx()`: make that `tx`.
        self.latest_tx = TxId(tx.0 - 1);
        let mut txn = ReplayTxn::default();
        for entry in entries {
            match self.apply_entry(entry, &mut txn) {
                Ok(()) => self.entries.push(EntryInfo {
                    id: entry.id,
                    tx,
                    body: entry.body.clone(),
                }),
                Err(fault) => refused(entry.id, fault),
            }
        }
        // Also when its Commit was refused: the transaction is over.
        self.latest_tx = tx;
    }

    /// Every kept entry grouped by transaction, in log order: what doctor
    /// replays for an in-memory world (a file-backed one is re-read from
    /// disk instead).
    pub(crate) fn kept_txns(&self) -> Vec<SurveyedTxn> {
        let mut txns: Vec<SurveyedTxn> = Vec::new();
        for e in &self.entries {
            let entry = LogEntry {
                id: e.id,
                body: e.body.clone(),
            };
            match txns.last_mut() {
                Some(t) if t.tx == e.tx => t.entries.push(entry),
                _ => txns.push(SurveyedTxn {
                    tx: e.tx,
                    entries: vec![entry],
                }),
            }
        }
        txns
    }

    /// The path of a file-backed world's log; `None` in memory.
    pub(crate) fn log_path(&self) -> Option<&Path> {
        self.log.as_ref().map(LogWriter::path)
    }

    /// The stored indexes, for doctor to compare against a rebuild.
    pub(crate) fn indexes(&self) -> StoredIndexes<'_> {
        StoredIndexes {
            e: &self.e_index,
            r: &self.r_index,
            v: &self.v_index,
            e_latest: &self.e_latest,
            r_latest: &self.r_latest,
            e_precision: &self.e_precision,
            e_latest_precision: &self.e_latest_precision,
            v_latest: &self.v_latest,
            vfold: &self.vfold_index,
            vfold_latest: &self.vfold_latest,
        }
    }

    /// Validates and applies one replayed entry (see
    /// [`World::replay_entry`]).
    fn apply_entry(&mut self, entry: &LogEntry, txn: &mut ReplayTxn) -> Result<(), ReplayFault> {
        let corrupt = |why: &dyn std::fmt::Display| ReplayFault::Invalid(why.to_string());
        let bad_correction =
            |why: &dyn std::fmt::Display| ReplayFault::BadCorrection(why.to_string());
        let unknown_attr = |attr: &AttrId| ReplayFault::DanglingAttr(*attr);
        if !matches!(entry.body, EntryBody::Commit { .. }) {
            txn.has_entries = true;
        }
        // Only the entry right after a DELETED marker may share its rev.
        let prev_deleted = txn.prev_deleted.take();
        match &entry.body {
            EntryBody::DefineNodeType { .. } | EntryBody::DefineAttr { .. } => Err(corrupt(
                &"schema definition outside the genesis transaction",
            )),
            EntryBody::DefineEntity { id, handle } => {
                // Validate as a live create would, then check the log
                // assigned exactly the ID a live create assigns.
                let name = self
                    .node_types
                    .get(usize::from(id.node_type().0))
                    .filter(|_| !id.is_edge())
                    .map(|nt| nt.name.clone())
                    .ok_or_else(|| corrupt(&format!("entity {id:?} has an unknown node type")))?;
                let def = self
                    .prepare_entity(&StagedIds::default(), &name, handle.as_deref())
                    .map_err(ReplayFault::Refused)?;
                if def.id != *id {
                    return Err(corrupt(&format!(
                        "entity {id:?} defined where {:?} was next",
                        def.id
                    )));
                }
                // Both replay paths key the transaction at `next_tx`.
                let tx = self.next_tx();
                self.apply_entity(def, tx);
                Ok(())
            }
            EntryBody::DefineEdge {
                id,
                source,
                attr,
                target,
            } => {
                // Exactly what a live interning writes: a ref attribute,
                // endpoints defined by earlier entries, a triple not yet
                // interned, and the next edge sequence.
                let def = self
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.value_type != ValueType::Ref {
                    return Err(corrupt(&format!(
                        "edge on non-ref attribute {:?}",
                        def.name
                    )));
                }
                for end in [source, target] {
                    if !self.is_defined(&StagedIds::default(), *end) {
                        return Err(ReplayFault::DanglingId(*end));
                    }
                }
                // A bidirectional pair is interned once, normalised.
                if def.direction == Some(RefDirection::Bidirectional) && source > target {
                    return Err(corrupt(&"bidirectional edge with source > target"));
                }
                let triple = (*source, *attr, *target);
                if let Some(existing) = self.edge_by_triple.get(&triple) {
                    return Err(corrupt(&format!("triple already interned as {existing:?}")));
                }
                if id.seq() != self.last_edge_seq(&StagedIds::default()) + 1 {
                    return Err(corrupt(&format!("{id:?} out of sequence")));
                }
                txn.unasserted_edges.insert(triple, *id);
                let tx = self.next_tx();
                self.apply_edge(
                    EdgeDef {
                        id: *id,
                        source: *source,
                        attr: *attr,
                        target: *target,
                    },
                    tx,
                );
                Ok(())
            }
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                let def = self
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                // A live write stores a bidirectional fact normalised; checked
                // before `prepare_fact`, which would normalise it silently.
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(corrupt(&"bidirectional fact with entity > value"));
                }
                let mut fact = self
                    .prepare_fact(&StagedIds::default(), *entity, &def.name, value, *tick, *op)
                    .map_err(ReplayFault::from_write)?;
                fact.precision = *precision;
                // A live write logs a ref fact's DefineEdge ahead of its
                // first stored assert, and a stored retract needs a stored
                // assert, so every stored ref fact's triple has an edge
                // defined by an earlier entry.
                if let Value::Ref(target) = value
                    && self
                        .edge_of(&StagedIds::default(), fact.entity, fact.attr, *target)
                        .is_none()
                {
                    return Err(corrupt(&"ref fact with no edge defined before it"));
                }
                // A live write never stores a fact at a key whose current
                // entry has the opposite op: a same-tick conflict. (The same
                // op again is a no-op, caught below.) A key whose current
                // entry is a DELETED marker holds nothing. Judged at the
                // latest canon; the current transaction's entries are
                // already in.
                let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
                let all = TxId(u64::MAX);
                if let Some(stored) = op_at_tick(&[&self.e_index], &mut prefix, fact.tick, all)
                    && stored != fact.op
                {
                    return Err(corrupt(&format!(
                        "a same-tick conflict: {:?} at {:?} where {stored:?} is current",
                        fact.op, fact.tick
                    )));
                }
                // A live write never stores a no-op: an assert of a value
                // that holds at its tick, or a retract of one that doesn't.
                // Judged like a live write, against everything before it
                // (the current transaction's earlier facts are already in).
                let held = holds(&[&self.e_index], &mut prefix, fact.tick, all);
                if held == (fact.op == Op::Assert) {
                    return Err(corrupt(&format!(
                        "a no-op {:?} at {:?} (the value {} held there)",
                        fact.op,
                        fact.tick,
                        if held { "already" } else { "never" }
                    )));
                }
                // A live transaction stores at most one entry per key.
                // Claimed last, so a refused fact claims nothing.
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                if let (Some(target), Op::Assert) = (fact.target, fact.op) {
                    txn.unasserted_edges
                        .remove(&(fact.entity, fact.attr, target));
                }
                self.index_entry(&fact, Mark::from(fact.op), self.next_tx());
                Ok(())
            }
            EntryBody::Deleted {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
            } => {
                let def = self
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(bad_correction(
                        &"bidirectional DELETED marker with entity > value",
                    ));
                }
                let chain = self.replay_chain(*of).map_err(|why| bad_correction(&why))?;
                let current = chain.current.ok_or_else(|| {
                    bad_correction(&format!("the chain of entry#{} is already deleted", of.0))
                })?;
                check_next_rev(chain, *of, *rev)?;
                // The marker sits at the key of the chain's current fact.
                // (Its op is unused: the marker is indexed as Deleted.)
                let fact = self
                    .stored_fact_def(*entity, *attr, value, *tick, Op::Assert)
                    .map_err(ReplayFault::Refused)?;
                if self.op_if_at_key(current, &fact).is_none() {
                    return Err(bad_correction(&format!(
                        "DELETED marker not at the current key of entry#{}",
                        of.0
                    )));
                }
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                self.index_entry(&fact, Mark::Deleted, self.next_tx());
                self.chains.insert(
                    *of,
                    Chain {
                        rev: *rev,
                        current: None,
                    },
                );
                txn.prev_deleted = Some((*of, *rev));
                Ok(())
            }
            EntryBody::Revised {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
                precision,
            } => {
                let def = self
                    .attributes
                    .get(attr.0 as usize)
                    .ok_or_else(|| unknown_attr(attr))?;
                if def.direction == Some(RefDirection::Bidirectional)
                    && let Value::Ref(target) = value
                    && entity > target
                {
                    return Err(bad_correction(
                        &"bidirectional revised fact with entity > value",
                    ));
                }
                // Validated like a live write (type, defined IDs, string
                // length, RetractEvent); not judged by the hold-based no-op
                // rules, which don't apply to corrections.
                let mut fact = self
                    .prepare_fact(&StagedIds::default(), *entity, &def.name, value, *tick, *op)
                    .map_err(ReplayFault::from_write)?;
                fact.precision = *precision;
                // A live correction interns the edge of a revised assert's
                // triple before it, as for a fact; a revised retract may be
                // the triple's only entry (no edge).
                if let (Some(target), Op::Assert) = (fact.target, fact.op)
                    && self
                        .edge_of(&StagedIds::default(), fact.entity, fact.attr, target)
                        .is_none()
                {
                    return Err(corrupt(
                        &"revised ref assert with no edge defined before it",
                    ));
                }
                let chain = self.replay_chain(*of).map_err(|why| bad_correction(&why))?;
                if prev_deleted == Some((*of, *rev)) {
                    // A moved key, right after the DELETED marker of the old
                    // one (which already took this rev). The same-tick rule
                    // at the new key: a live writer errs on the opposite op
                    // and writes no Revised for the same one, so any current
                    // entry there means no live writer wrote this.
                    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
                    let all = TxId(u64::MAX);
                    if let Some(stored) = op_at_tick(&[&self.e_index], &mut prefix, fact.tick, all)
                    {
                        return Err(corrupt(&format!(
                            "a moved revision onto {:?} where {stored:?} is current",
                            fact.tick
                        )));
                    }
                } else {
                    // Otherwise it revises the current fact in place: same
                    // key, its own next rev, another op or precision.
                    let current = chain.current.ok_or_else(|| {
                        bad_correction(&format!(
                            "a revision of the deleted chain of entry#{} not right after its \
                             DELETED marker",
                            of.0
                        ))
                    })?;
                    check_next_rev(chain, *of, *rev)?;
                    let Some((c_op, c_precision)) = self.op_if_at_key(current, &fact) else {
                        return Err(bad_correction(&format!(
                            "a revision of entry#{} at another key not right after its \
                             DELETED marker",
                            of.0
                        )));
                    };
                    if (fact.op, fact.precision) == (c_op, c_precision) {
                        return Err(corrupt(&format!(
                            "a revision of entry#{} that changes nothing",
                            of.0
                        )));
                    }
                }
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
                // Only an accepted revised assert asserts its edge's triple
                // (as for a Fact): a refused one must leave the edge
                // unasserted, so doctor's collecting replay judges the
                // Commit like a fresh rebuild.
                if let (Some(target), Op::Assert) = (fact.target, fact.op) {
                    txn.unasserted_edges
                        .remove(&(fact.entity, fact.attr, target));
                }
                self.index_entry(&fact, Mark::from(fact.op), self.next_tx());
                self.chains.insert(
                    *of,
                    Chain {
                        rev: *rev,
                        current: Some(entry.id),
                    },
                );
                Ok(())
            }
            EntryBody::Commit { tx, .. } => {
                if *tx != self.next_tx() {
                    return Err(corrupt(&format!("{tx:?} out of sequence")));
                }
                // A live commit with nothing staged appends nothing.
                if !txn.has_entries {
                    return Err(corrupt(&"a transaction with nothing but its Commit"));
                }
                // A live write interns an edge only for a stored assert of
                // its triple in the same transaction.
                if let Some(id) = txn.unasserted_edges.values().min() {
                    return Err(corrupt(&format!(
                        "{id:?} defined in a transaction that doesn't assert its triple"
                    )));
                }
                *txn = ReplayTxn::default();
                self.latest_tx = *tx;
                Ok(())
            }
        }
    }

    /// Stores one entry of transaction `tx` at the key of `fact` in the
    /// indexes: `mark` (the fact's op, or [`Mark::Deleted`] for a `DELETED`
    /// marker at that key) under its `E` key and, for a `ref` key, its `R`
    /// mirror, for an indexed non-ref attribute its `V` mirror (and for a
    /// string attribute its `VFold` mirror). Commit and
    /// replay both write the indexes only through here, so the two always
    /// hold the same entries, and the latest-revision indexes and the
    /// latest tick follow the keys' current entries.
    fn index_entry(&mut self, fact: &FactDef, mark: Mark, tx: TxId) {
        // A new unindexed value enters the table with the reference its
        // keys were built with (see `World::value_ref`).
        if let Some(r) = fact.value_ref() {
            self.values.insert(&fact.value, r);
        }
        let k = key::e_fact_key(fact.entity, fact.attr, fact.key_value(), fact.tick, tx);
        // `tx` is newer than every stored entry, so this entry becomes the
        // key's current one at the latest canon: `E_latest` holds the
        // previous current entry, if it was a fact.
        let at_key = key::without_tx(&k);
        let was_fact = self.e_latest.contains_key(at_key);
        set_latest(&mut self.e_latest, at_key, mark);
        // The precision rides along with the entry (sparse: vague ticks
        // only). The key's current entry replaces the previous one's at
        // the latest canon, so a DELETED marker or an exact fact clears it.
        let vague = mark.op().is_some() && fact.precision != Precision::Exact;
        if vague {
            self.e_precision.insert(k.clone(), fact.precision);
            match self.e_latest_precision.get_mut(at_key) {
                Some(p) => *p = fact.precision,
                None => {
                    self.e_latest_precision
                        .insert(at_key.to_vec(), fact.precision);
                }
            }
        } else {
            self.e_latest_precision.remove(at_key);
        }
        match (was_fact, mark.op().is_some()) {
            (false, true) => *self.tick_counts.entry(fact.tick).or_insert(0) += 1,
            (true, false) => {
                if let Some(n) = self.tick_counts.get_mut(&fact.tick) {
                    *n -= 1;
                    if *n == 0 {
                        self.tick_counts.remove(&fact.tick);
                    }
                }
            }
            _ => {}
        }
        self.e_index.insert(k, mark);
        if let Some(target) = fact.target {
            let k = key::r_fact_key(target, fact.attr, fact.entity, fact.tick, tx);
            set_latest(&mut self.r_latest, key::without_tx(&k), mark);
            self.r_index.insert(k, mark);
        }
        if self.value_indexed(fact.attr) {
            let k = key::v_fact_key(fact.attr, &fact.value, fact.entity, fact.tick, tx);
            set_latest(&mut self.v_latest, key::without_tx(&k), mark);
            self.v_index.insert(k, mark);
            // A string attribute's entry is mirrored once more under its
            // fold. `fact.value` comes from `Value::encode`, so it decodes;
            // anything but a string (never stored for a string attribute)
            // has no fold.
            if let Ok(Value::Str(text)) = Value::decode(&fact.value) {
                let fold = Value::Str(fold_for_search(&text))
                    .encode()
                    .expect("invariant: encoding a string never fails");
                let k =
                    key::vfold_fact_key(fact.attr, &fold, &fact.value, fact.entity, fact.tick, tx);
                set_latest(&mut self.vfold_latest, key::without_tx(&k), mark);
                self.vfold_index.insert(k, mark);
            }
        }
    }

    /// Whether `attr`'s entries are mirrored in `V`: it is declared
    /// indexed and isn't a `ref` attribute (`R` is the value index of a
    /// `ref` attribute).
    fn value_indexed(&self, attr: AttrId) -> bool {
        let def = &self.attributes[attr.0 as usize];
        def.indexed && def.value_type != ValueType::Ref
    }

    /// Whether `attr`'s `E` keys carry value references: it is an
    /// unindexed string attribute.
    fn keys_by_ref(&self, attr: AttrId) -> bool {
        self.attributes
            .get(attr.0 as usize)
            .is_some_and(|def| !def.indexed && def.value_type == ValueType::Str)
    }

    /// The full value encoding an `E`-key value part of attribute `attr`
    /// stands for: the part itself, or for an unindexed string the value
    /// its reference names in the table. `None` if the part doesn't fit the
    /// attribute's layout (a reference on an attribute not keyed by
    /// reference, a plain encoding on one that is) or names a reference the
    /// table lacks: only reachable by tampering, so readers skip such keys
    /// (doctor reports them).
    fn e_part_encoding<'a>(&'a self, attr: AttrId, part: &'a [u8]) -> Option<&'a [u8]> {
        match (self.keys_by_ref(attr), ValueRef::from_key_bytes(part)) {
            (true, Some(r)) => self.values.resolve(r),
            (false, None) => Some(part),
            _ => None,
        }
    }

    /// The value an `E`-key value part of attribute `attr` stands for, or
    /// `None` if it can't be resolved or decoded (see
    /// [`World::e_part_encoding`]); readers skip such keys.
    fn decode_e_part(&self, attr: AttrId, part: &[u8]) -> Option<Value> {
        self.decode_e_value(attr, part).map(|(_, v)| v)
    }

    /// The full value encoding and the value an `E`-key value part of
    /// attribute `attr` stands for, or `None` if it can't be resolved or
    /// decoded, or decodes to a value of another type than the attribute's
    /// (a plain value on a `ref` attribute, say). Every write and replay
    /// checks the type, so such a key is only reachable by tampering:
    /// readers skip it (doctor reports it).
    fn decode_e_value<'a>(&'a self, attr: AttrId, part: &'a [u8]) -> Option<(&'a [u8], Value)> {
        let bytes = self.e_part_encoding(attr, part)?;
        let value = Value::decode(bytes).ok()?;
        let def = self.attributes.get(attr.0 as usize)?;
        (value.value_type() == def.value_type).then_some((bytes, value))
    }

    /// The `E`-key value part of the committed value `v` (encoded as
    /// `encoding`) of `attr`: the encoding, or for an unindexed string its
    /// reference; `None` if no committed entry ever stored that unindexed
    /// value (so it has no history).
    fn committed_key_part<'a>(&self, attr: AttrId, encoding: &'a [u8]) -> Option<Cow<'a, [u8]>> {
        if !self.keys_by_ref(attr) {
            return Some(Cow::Borrowed(encoding));
        }
        let r = self.values.get(encoding)?;
        Some(Cow::Owned(r.to_key_bytes().to_vec()))
    }

    /// Decodes a stored `E` key with its value resolved through this
    /// world's value table: `(entity, attr, value, tick, tx)`, or `None` if
    /// the bytes aren't a well-formed `E` key or refer to no value. Never
    /// panics, whatever the bytes.
    fn decode_e_row_resolved(&self, k: &[u8]) -> Option<(EntityId, AttrId, Value, Tick, TxId)> {
        let (entity, attr, part, tick, tx) = key::decode_e_key(k).ok()?;
        let (_, value) = self.decode_e_value(attr, part)?;
        Some((entity, attr, value, tick, tx))
    }

    /// A copy of an `E`-layout index (`main`: full keys, else
    /// latest-revision keys) with every value reference replaced by the
    /// full value encoding it names, for doctor to compare rows with values
    /// resolved: two worlds that agree on every row agree on these keys,
    /// whatever references they assigned. The second list holds the stored
    /// keys that can't be resolved: a reference on an attribute that isn't
    /// an unindexed string, an unindexed string not keyed by reference, or a
    /// reference the table lacks. Keys that don't parse as `E` keys at all
    /// are kept as they are (doctor reports them as it always has). Borrows
    /// the index when nothing needs resolving.
    pub(crate) fn resolved_e_index<'i, V: Clone>(
        &self,
        index: &'i BTreeMap<Vec<u8>, V>,
        main: bool,
    ) -> ResolvedIndex<'i, V> {
        let unindexed_strings = self
            .attributes
            .iter()
            .any(|d| !d.indexed && d.value_type == ValueType::Str);
        if self.values.is_empty() && !unindexed_strings {
            return (Cow::Borrowed(index), Vec::new());
        }
        let mut out = BTreeMap::new();
        let mut bad = Vec::new();
        for (k, v) in index {
            let split = if main {
                key::decode_e_key(k).map(|(e, a, part, ..)| (e, a, part))
            } else {
                key::decode_latest_e_key(k).map(|(e, a, part, _)| (e, a, part))
            };
            let Ok((_, attr, part)) = split else {
                out.insert(k.clone(), v.clone());
                continue;
            };
            let is_ref = ValueRef::from_key_bytes(part).is_some();
            if self.attributes.get(attr.0 as usize).is_none() && !is_ref {
                // An unknown attribute: no layout to check against.
                out.insert(k.clone(), v.clone());
                continue;
            }
            let resolved = self.e_part_encoding(attr, part);
            let Some(full) = resolved else {
                bad.push(k.clone());
                continue;
            };
            // The part sits right after `E|e|a|`; the suffix follows it.
            let start = key::E_ATTR_PREFIX_LEN;
            let mut canonical = Vec::with_capacity(k.len() - part.len() + full.len());
            canonical.extend_from_slice(&k[..start]);
            canonical.extend_from_slice(full);
            canonical.extend_from_slice(&k[start + part.len()..]);
            out.insert(canonical, v.clone());
        }
        (Cow::Owned(out), bad)
    }

    /// Every stored `E` and `R` entry, decoded, in key order: an inspection
    /// hook for tests. Reads the two stored indexes independently; it never
    /// derives one from the other. A row whose key doesn't decode (only
    /// reachable by tampering, see [`World::tamper_index`]) is skipped, with
    /// the same decoder doctor uses.
    #[doc(hidden)]
    pub fn index_dump(&self) -> IndexDump {
        let e = self
            .e_index
            .iter()
            .filter_map(|(k, mark)| {
                let (entity, attr, value, tick, tx) = self.decode_e_row_resolved(k)?;
                Some((entity, attr, value, tick, tx, *mark))
            })
            .collect();
        let r = self
            .r_index
            .iter()
            .filter_map(|(k, mark)| {
                let (target, attr, source, tick, tx) = key::decode_r_key(k).ok()?;
                Some((target, attr, source, tick, tx, *mark))
            })
            .collect();
        let v = self
            .v_index
            .iter()
            .filter_map(|(k, mark)| {
                let (attr, value, entity, tick, tx) = decode_v_row(k)?;
                Some((attr, value, entity, tick, tx, *mark))
            })
            .collect();
        let v_fold = self
            .vfold_index
            .iter()
            .filter_map(|(k, mark)| {
                let (attr, folded, original, entity, tick, tx) = decode_vfold_row(k)?;
                Some((attr, folded, original, entity, tick, tx, *mark))
            })
            .collect();
        IndexDump { e, r, v, v_fold }
    }

    /// Damages one stored index in memory, for doctor's tests: the
    /// indexes are copies of the log, and this is how a copy goes wrong
    /// without the log changing. Only that index changes (nothing else is
    /// kept in step), and nothing is logged. A row number past the end does
    /// nothing. Not part of the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn tamper_index(&mut self, t: Tamper) {
        fn remove_nth<V>(index: &mut BTreeMap<Vec<u8>, V>, nth: usize) {
            if let Some(k) = index.keys().nth(nth).cloned() {
                index.remove(&k);
            }
        }
        let flip_mark = |m: &mut Mark| {
            *m = match m {
                Mark::Assert => Mark::Retract,
                // A DELETED marker has no op; it becomes a fact.
                Mark::Retract | Mark::Deleted => Mark::Assert,
            }
        };
        let flip_op = |o: &mut Op| {
            *o = match o {
                Op::Assert => Op::Retract,
                Op::Retract => Op::Assert,
            }
        };
        match t {
            Tamper::InsertRawKey { index, key } => match index {
                IndexName::E => {
                    self.e_index.insert(key, Mark::Assert);
                }
                IndexName::R => {
                    self.r_index.insert(key, Mark::Assert);
                }
                IndexName::ELatest => {
                    self.e_latest.insert(key, Op::Assert);
                }
                IndexName::RLatest => {
                    self.r_latest.insert(key, Op::Assert);
                }
                IndexName::V => {
                    self.v_index.insert(key, Mark::Assert);
                }
                IndexName::VLatest => {
                    self.v_latest.insert(key, Op::Assert);
                }
                IndexName::VFold => {
                    self.vfold_index.insert(key, Mark::Assert);
                }
                IndexName::VFoldLatest => {
                    self.vfold_latest.insert(key, Op::Assert);
                }
            },
            Tamper::RemoveRow { index, nth } => match index {
                IndexName::E => remove_nth(&mut self.e_index, nth),
                IndexName::R => remove_nth(&mut self.r_index, nth),
                IndexName::ELatest => remove_nth(&mut self.e_latest, nth),
                IndexName::RLatest => remove_nth(&mut self.r_latest, nth),
                IndexName::V => remove_nth(&mut self.v_index, nth),
                IndexName::VLatest => remove_nth(&mut self.v_latest, nth),
                IndexName::VFold => remove_nth(&mut self.vfold_index, nth),
                IndexName::VFoldLatest => remove_nth(&mut self.vfold_latest, nth),
            },
            Tamper::FlipOp { index, nth } => match index {
                IndexName::E => self
                    .e_index
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_mark),
                IndexName::R => self
                    .r_index
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_mark),
                IndexName::ELatest => self
                    .e_latest
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_op),
                IndexName::RLatest => self
                    .r_latest
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_op),
                IndexName::V => self
                    .v_index
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_mark),
                IndexName::VLatest => self
                    .v_latest
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_op),
                IndexName::VFold => self
                    .vfold_index
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_mark),
                IndexName::VFoldLatest => self
                    .vfold_latest
                    .values_mut()
                    .nth(nth)
                    .into_iter()
                    .for_each(flip_op),
            },
        }
    }

    /// Makes the next log append fail as `fault` says, to test failure
    /// handling. An in-memory world has no log and ignores it. Not part of
    /// the supported API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn inject_log_fault(&mut self, fault: log::LogFault) {
        if let Some(log) = self.log.as_mut() {
            log.inject_fault(fault);
        }
    }

    /// `Err` if the world refuses writes: a read-only world or an
    /// unacknowledged damaged-tail recovery (checked first, see
    /// [`World::accepts_writes`]), or a log append that failed earlier.
    fn writable(&self) -> Result<(), Error> {
        self.accepts_writes()?;
        self.log.as_ref().map_or(Ok(()), LogWriter::check)
    }

    /// `Err(ReadOnly)` for a world from [`World::open_read_only`], else
    /// `Err(RecoveryUnacknowledged)` until a damaged-tail cut is
    /// acknowledged (see [`World::acknowledge_recovery`]).
    fn accepts_writes(&self) -> Result<(), Error> {
        if self.read_only {
            return Err(Error::ReadOnly);
        }
        if self.recovery_unacknowledged {
            return Err(Error::RecoveryUnacknowledged);
        }
        Ok(())
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
        self.recovery_unacknowledged = false;
    }

    /// What opening the log did to recover it (see [`RecoveryReport`]).
    pub fn recovery(&self) -> &RecoveryReport {
        &self.recovery
    }

    /// The last committed transaction. Every write that stores a fact
    /// advances it by one; a failed or 0-changed write leaves it unchanged.
    pub fn latest_tx(&self) -> TxId {
        self.latest_tx
    }

    /// The world's latest tick: the highest tick of any fact (assert or
    /// retract) at the latest canon, or `Tick(0)` if there is none. A fact
    /// deleted by a correction doesn't count.
    ///
    /// A tick-less write lands here (see [`World::begin`]). A prequel fact, a
    /// no-op or a failed write leaves it as it is; deleting the fact(s) at
    /// the highest tick lowers it to the next highest.
    pub fn latest_tick(&self) -> Tick {
        self.tick_counts
            .last_key_value()
            .map_or(Tick(0), |(tick, _)| *tick)
    }

    /// Starts a write transaction.
    ///
    /// The world's [`latest tick`](World::latest_tick) is captured now: a
    /// write in the transaction with no tick of its own and no
    /// [`default`](Txn::set_default_tick) lands on it, so ticks written
    /// earlier in the same transaction don't move it. Nothing is visible
    /// until [`Txn::commit`]; dropping the transaction discards it.
    pub fn begin(&mut self) -> Txn<'_> {
        let base_tick = self.latest_tick();
        let tx = self.next_tx();
        Txn {
            world: self,
            base_tick,
            default_tick: None,
            tx,
            staged: BTreeMap::new(),
            entries: Vec::new(),
            ids: StagedIds::default(),
            poisoned: false,
        }
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
        let mut txn = self.begin();
        let mut res = txn.correct(entry, c)?;
        let commit = txn.commit()?;
        res.entries = commit.entries;
        res.edges = commit.edges;
        res.warnings = commit.warnings;
        Ok(res)
    }

    /// Resolves the target of a correction of `entry` (a base `Fact` or any
    /// `Revised` entry of its chain): the chain's base entry, its state and
    /// its current fact (as stored, encoded). Changes nothing.
    fn correction_target(&self, entry: EntryId) -> Result<Target, Error> {
        let info = self.kept_entry(entry).ok_or(Error::UnknownEntry(entry))?;
        let base = match info.body {
            EntryBody::Fact { .. } => entry,
            // A Revised entry always names its chain's base (replay checks it).
            EntryBody::Revised { of, .. } => of,
            _ => return Err(Error::NotAFact(entry)),
        };
        let chain = self.chain(base);
        let current = chain.current.ok_or(Error::AlreadyDeleted(entry))?;
        let (entity, attr, value, tick, op, precision) = self
            .kept_entry(current)
            .and_then(|info| as_fact(&info.body))
            .expect("invariant: a chain's current entry is a committed Fact or Revised entry");
        // A committed fact's value was encoded once before; it encodes.
        let mut fact = self.stored_fact_def(entity, attr, value, tick, op)?;
        fact.precision = precision;
        Ok(Target {
            base,
            chain,
            fact,
            value: value.clone(),
        })
    }

    /// The correction state of the chain of base entry `base`.
    fn chain(&self, base: EntryId) -> Chain {
        self.chains
            .get(&base)
            .copied()
            .unwrap_or(Chain::untouched(base))
    }

    /// Replay: the op and precision of the committed fact entry `current`
    /// (a chain's current fact) if `fact` is at its key, else `None`.
    /// Values compare by encoding: exact bytes.
    fn op_if_at_key(&self, current: EntryId, fact: &FactDef) -> Option<(Op, Precision)> {
        let (entity, attr, value, tick, op, precision) = self
            .kept_entry(current)
            .and_then(|info| as_fact(&info.body))
            .expect("invariant: a chain's current entry is a committed Fact or Revised entry");
        // Committed values were encoded once before, so they encode.
        let same = (entity, attr, tick) == (fact.entity, fact.attr, fact.tick)
            && value.encode().is_ok_and(|v| v == fact.value);
        same.then_some((op, precision))
    }

    /// Replay: the state of the chain a correction names as `of`, which
    /// must be an earlier base `Fact` entry (a live correction only names
    /// committed entries, and resolves a chain to its base).
    fn replay_chain(&self, of: EntryId) -> Result<Chain, String> {
        let base = self
            .kept_entry(of)
            .filter(|b| b.tx <= self.latest_tx)
            .ok_or_else(|| format!("corrects entry#{}, not an earlier entry", of.0))?;
        if !matches!(base.body, EntryBody::Fact { .. }) {
            return Err(format!("corrects entry#{}, not a base fact", of.0));
        }
        Ok(self.chain(of))
    }

    /// One auto-committed write: a transaction holding just this write.
    /// A no-op stages nothing, so its commit consumes no tx.
    fn write(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        tick: Option<Tick>,
        op: Op,
        precision: Precision,
    ) -> Result<WriteResult, Error> {
        let mut txn = self.begin();
        let mut res = txn.write(entity, attr, v, tick, op, precision)?;
        let commit = txn.commit()?;
        res.entries = commit.entries;
        res.edges = commit.edges;
        res.warnings = commit.warnings;
        Ok(res)
    }

    /// Validates a fact against the schema and the defined entities without
    /// changing the world.
    ///
    /// Entities staged by the writing transaction (`staged`) count as
    /// defined, both as the subject and as a `ref` value. A fact on a
    /// bidirectional attribute comes back normalised (see
    /// [`World::stored_pair`]), so both spellings judge and store as one.
    ///
    fn prepare_fact(
        &self,
        staged: &StagedIds,
        entity: EntityId,
        attr: &str,
        v: &Value,
        tick: Tick,
        op: Op,
    ) -> Result<FactDef, Error> {
        let (attr_id, def) = self.resolve(staged, entity, attr)?;
        check_type(attr, def, v)?;
        // Length check before encoding, so an oversized string isn't copied.
        if def.indexed {
            v.check_indexed()?;
        }
        let mut value = v.encode()?;
        if let Value::Ref(target) = v
            && !self.is_defined(staged, *target)
        {
            return Err(Error::UnknownEntity(*target));
        }
        // Events are never retracted in story time, whatever is stored, so
        // this is a schema error: judged after input validation but before
        // the same-tick and no-op rules, which look at stored facts.
        if op == Op::Retract && def.kind == Kind::Event {
            return Err(Error::RetractEvent(attr.to_owned()));
        }
        let (entity, target) = match v {
            Value::Ref(target) => {
                let (source, target) = self.stored_pair(attr_id, entity, *target);
                if source != entity {
                    // Swapped: the value is now the caller's entity.
                    value = Value::Ref(target).encode()?;
                }
                (source, Some(target))
            }
            _ => (entity, None),
        };
        let vref = self.value_ref(staged, def, v, &value)?;
        Ok(FactDef {
            entity,
            attr: attr_id,
            value,
            tick,
            op,
            precision: Precision::Exact,
            target,
            vref,
        })
    }

    /// The value reference the `E` keys of value `v` (encoded as
    /// `encoding`) of attribute `def` carry: `None` unless `def` is an
    /// unindexed string attribute. A value the world's table (or the
    /// writing transaction, `staged`) already holds keeps its reference; a
    /// new one gets the next ordinal of its content hash, counting the
    /// values `staged` adds, which is exactly what it gets when its entry
    /// is applied (commit and replay assign in log order). A new value has
    /// no stored history, so judging it under that reference finds nothing,
    /// as it should.
    ///
    /// Fails with [`Error::Id`] only if every ordinal of the hash is spent
    /// (`u32::MAX + 1` distinct values sharing one content hash).
    fn value_ref(
        &self,
        staged: &StagedIds,
        def: &AttrDef,
        v: &Value,
        encoding: &[u8],
    ) -> Result<Option<[u8; key::VALUE_REF_LEN]>, Error> {
        let Value::Str(text) = v else {
            return Ok(None);
        };
        if def.indexed || def.value_type != ValueType::Str {
            return Ok(None);
        }
        if let Some(r) = self
            .values
            .get(encoding)
            .or_else(|| staged.values.get(encoding))
        {
            return Ok(Some(r.to_key_bytes()));
        }
        let hash = (self.content_hash)(text.as_bytes());
        // Staged values of a hash continue the committed ordinals.
        let next = self
            .values
            .next_ordinal(hash)
            .max(staged.values.next_ordinal(hash));
        let ordinal = u32::try_from(next).map_err(|_| IdError::SeqOutOfRange(next))?;
        Ok(Some(ValueRef { hash, ordinal }.to_key_bytes()))
    }

    /// The [`FactDef`] (keys and op) of a stored entry, from its decoded
    /// fields: its value reference is the one the world's table holds.
    fn stored_fact_def(
        &self,
        entity: EntityId,
        attr: AttrId,
        value: &Value,
        tick: Tick,
        op: Op,
    ) -> Result<FactDef, Error> {
        let encoded = value.encode()?;
        let vref = match self.attributes.get(attr.0 as usize) {
            Some(def) => self.value_ref(&StagedIds::default(), def, value, &encoded)?,
            None => None,
        };
        Ok(FactDef {
            entity,
            attr,
            value: encoded,
            tick,
            op,
            precision: Precision::Exact,
            target: match value {
                Value::Ref(target) => Some(*target),
                _ => None,
            },
            vref,
        })
    }

    /// The transaction number the next commit gets.
    fn next_tx(&self) -> TxId {
        TxId(self.latest_tx.0 + 1)
    }

    /// Looks up a known attribute and checks the entity is defined: the
    /// validation shared by writes and reads. Reads pass no staged entities.
    fn resolve(
        &self,
        staged: &StagedIds,
        entity: EntityId,
        attr: &str,
    ) -> Result<(AttrId, &AttrDef), Error> {
        let attr_id = *self
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        if !self.is_defined(staged, entity) {
            return Err(Error::UnknownEntity(entity));
        }
        Ok((attr_id, &self.attributes[attr_id.0 as usize]))
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
        let canon = tx.min(self.latest_tx);
        View {
            world: self,
            canon,
            latest: canon == self.latest_tx,
        }
    }

    /// Like [`World::at_canon`], but always reading the main indexes with
    /// the canon skip, even at the latest canon: the reference path the
    /// latest-revision indexes must agree with. Not part of the supported
    /// API.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn at_canon_main(&self, tx: TxId) -> View<'_> {
        View {
            world: self,
            canon: tx.min(self.latest_tx),
            latest: false,
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
        self.at_canon(self.latest_tx)
            .as_of_value(entity, attr, v, t)
    }

    /// Every entity (or edge) for which `attr = v` holds at tick `t`
    /// (latest canon). Same as [`View::lookup`] on `at_canon(latest_tx())`.
    pub fn lookup(&self, attr: &str, v: &Value, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.at_canon(self.latest_tx).lookup(attr, v, t)
    }

    /// Every entity of node type `node_type` (latest canon). Same as
    /// [`View::entities_of_type`] on `at_canon(latest_tx())`.
    pub fn entities_of_type<'w>(
        &'w self,
        node_type: &str,
    ) -> Result<impl Iterator<Item = EntityId> + use<'w>, Error> {
        self.at_canon(self.latest_tx).entities_of_type(node_type)
    }

    /// Every entity (or edge) with a value of the string attribute `attr`
    /// holding at tick `t` whose [`fold_for_search`] equals `text`'s
    /// (latest canon). Same as [`View::lookup_ci`] on
    /// `at_canon(latest_tx())`.
    pub fn lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error> {
        self.at_canon(self.latest_tx).lookup_ci(attr, text, t)
    }

    /// Every value of `entity.attr` that holds at tick `t` (latest canon).
    /// Same as [`View::as_of`] on `at_canon(latest_tx())`.
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        self.at_canon(self.latest_tx).as_of(entity, attr, t)
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
        self.at_canon(self.latest_tx).as_of_detail(entity, attr, t)
    }

    /// The change points of `entity.attr` (latest canon).
    /// Same as [`View::history`] on `at_canon(latest_tx())`.
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        self.at_canon(self.latest_tx).history(entity, attr)
    }

    /// Every cardinality overlap of `attr` (latest canon).
    /// Same as [`View::overlaps`] on `at_canon(latest_tx())`.
    pub fn overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error> {
        self.at_canon(self.latest_tx).overlaps(attr)
    }

    /// Every defined entity and edge ID, ascending: entities by node type
    /// then sequence (the type is the ID's high bits), then edges (tagged
    /// with the ID's top bit).
    fn defined_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
        let entities = self.node_types.iter().enumerate().flat_map(|(t, nt)| {
            // In range: at most MAX_NODE_TYPE + 1 types are declared.
            let t = NodeTypeId(t as u16);
            (1..=nt.last_seq()).map(move |seq| {
                EntityId::new(t, seq).expect("invariant: assigned entity sequences fit an ID")
            })
        });
        let edges = (1..=self.edges.len() as u64).map(|seq| {
            EntityId::from(EdgeId::new(seq).expect("invariant: assigned edge sequences fit an ID"))
        });
        entities.chain(edges)
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
        self.at_canon(self.latest_tx)
            .neighbors(entity, attr, dir, t)
    }

    /// Resolves a name `neighbors` accepts: an attribute, read in `dir`, or
    /// an inverse name, read as its base attribute with `dir` flipped.
    fn resolve_neighbor_attr(
        &self,
        attr: &str,
        dir: Direction,
    ) -> Result<(AttrId, Direction), Error> {
        if let Some(id) = self.attr_by_name.get(attr) {
            return Ok((*id, dir));
        }
        match self.inverse_by_name.get(attr) {
            Some(id) => Ok((*id, dir.flipped())),
            None => Err(Error::UnknownAttribute(attr.to_owned())),
        }
    }

    /// Whether `id` names an entity or edge defined in this world or staged
    /// in `staged` by the transaction asking.
    fn is_defined(&self, staged: &StagedIds, id: EntityId) -> bool {
        // Sequences are assigned without gaps, so sequences 1..=last of a
        // declared node type, or of the edges, are exactly the defined IDs.
        if id.is_edge() {
            return id
                .as_edge()
                .is_some_and(|e| e.seq() <= self.last_edge_seq(staged));
        }
        id.seq() >= 1
            && self
                .last_seq(staged, id.node_type())
                .is_some_and(|last| id.seq() <= last)
    }

    /// The last edge sequence number assigned (0 = no edges), counting
    /// staged edges.
    fn last_edge_seq(&self, staged: &StagedIds) -> u64 {
        // A Vec's length always fits a u64.
        (self.edges.len() + staged.edges.len()) as u64
    }

    /// Whether `attr` is a bidirectional `ref` attribute.
    fn is_bidirectional(&self, attr: AttrId) -> bool {
        self.attributes[attr.0 as usize].direction == Some(RefDirection::Bidirectional)
    }

    /// The order a `ref` pair `(source, target)` of `attr` is stored and
    /// interned in: `(min, max)` by entity ID for a bidirectional attribute
    /// (one edge, one history, whichever spelling was written), as given
    /// for a directed one.
    fn stored_pair(
        &self,
        attr: AttrId,
        source: EntityId,
        target: EntityId,
    ) -> (EntityId, EntityId) {
        if self.is_bidirectional(attr) && source > target {
            (target, source)
        } else {
            (source, target)
        }
    }

    /// The edge interned for the `ref` fact `(source, attr, target)`,
    /// committed or staged in `staged`. A bidirectional pair is found in
    /// either order.
    fn edge_of(
        &self,
        staged: &StagedIds,
        source: EntityId,
        attr: AttrId,
        target: EntityId,
    ) -> Option<EdgeId> {
        let (source, target) = self.stored_pair(attr, source, target);
        let triple = (source, attr, target);
        self.edge_by_triple
            .get(&triple)
            .or_else(|| staged.edge_by_triple.get(&triple))
            .copied()
    }

    /// The edge a stored assert of `(source, attr, target)` has to intern:
    /// `None` if the triple already has an edge (committed or staged),
    /// else a definition with the next edge sequence.
    fn prepare_edge(
        &self,
        staged: &StagedIds,
        source: EntityId,
        attr: AttrId,
        target: EntityId,
    ) -> Result<Option<EdgeDef>, Error> {
        if self.edge_of(staged, source, attr, target).is_some() {
            return Ok(None);
        }
        let (source, target) = self.stored_pair(attr, source, target);
        Ok(Some(EdgeDef {
            id: EdgeId::new(self.last_edge_seq(staged) + 1)?,
            source,
            attr,
            target,
        }))
    }

    /// Applies an edge definition (its `DefineEdge` entry) as part of a
    /// commit or a replay. Infallible.
    /// The `(entity, attr, side)` overlap checks the stored entries at the
    /// keys `facts` can change (write warnings): the fact's entity
    /// (Forward), for a `ref` fact its target (Reverse), and for a
    /// bidirectional one also the target's Forward side (its partners
    /// include the entity). A value-moving correction lists both its old
    /// and its new key, so both targets are covered. Sorted by `(entity,
    /// attr name, side)` with Forward first, deduplicated: the order the
    /// warnings are reported in. Cost: proportional to the entries, not the
    /// world.
    fn touched_sides<'f>(
        &self,
        facts: impl Iterator<Item = &'f FactDef>,
    ) -> Vec<(EntityId, AttrId, Side)> {
        let mut touched = Vec::new();
        for f in facts {
            self.sides_of(f, |side, _| touched.push(side));
        }
        self.sort_sides(&mut touched, |s| *s);
        touched.dedup();
        touched
    }

    /// [`World::touched_sides`], each side with the values the entries
    /// change on it (their encodings, ascending and deduplicated): on the
    /// entity's Forward side the fact's value, on the target's Reverse (and
    /// bidirectional Forward) side `Ref(entity)`. Only those values'
    /// intervals can differ before and after the commit.
    fn touched_side_values<'f>(
        &self,
        facts: impl Iterator<Item = &'f FactDef>,
    ) -> Vec<TouchedSide> {
        let mut pairs: Vec<((EntityId, AttrId, Side), Vec<u8>)> = Vec::new();
        for f in facts {
            self.sides_of(f, |side, value| pairs.push((side, value)));
        }
        self.sort_sides(&mut pairs, |p| p.0);
        let mut out: Vec<TouchedSide> = Vec::new();
        for (side, value) in pairs {
            match out.last_mut() {
                Some(last) if last.side == side => last.values.push(value),
                _ => out.push(TouchedSide {
                    side,
                    values: vec![value],
                }),
            }
        }
        for t in &mut out {
            t.values.sort_unstable();
            t.values.dedup();
        }
        out
    }

    /// Calls `push` with each `(entity, attr, side)` check the stored entry
    /// at `f`'s key can change and the value (encoded) it changes there.
    fn sides_of(&self, f: &FactDef, mut push: impl FnMut((EntityId, AttrId, Side), Vec<u8>)) {
        push((f.entity, f.attr, Side::Forward), f.value.clone());
        if let Some(target) = f.target {
            let source = || {
                Value::Ref(f.entity)
                    .encode()
                    .expect("invariant: a ref value always encodes")
            };
            push((target, f.attr, Side::Reverse), source());
            if self.is_bidirectional(f.attr) {
                push((target, f.attr, Side::Forward), source());
            }
        }
    }

    /// Sorts by `(entity, attr name, side)` with Forward first (stable, so
    /// equal sides keep their order): the order warnings are reported in.
    fn sort_sides<T>(&self, items: &mut [T], side: impl Fn(&T) -> (EntityId, AttrId, Side)) {
        let rank = |s: Side| match s {
            Side::Forward => 0u8,
            Side::Reverse => 1,
        };
        let name = |a: AttrId| self.attributes[a.0 as usize].name.as_str();
        items.sort_by(|x, y| {
            let (x, y) = (side(x), side(y));
            (x.0.raw(), name(x.1), rank(x.2)).cmp(&(y.0.raw(), name(y.1), rank(y.2)))
        });
    }

    /// The overlaps at the latest canon of each touched side in `sides`
    /// that involve one of its touched values, one list per entry (see
    /// [`View::overlaps_of_touching`]).
    fn overlaps_of_sides(&self, sides: &[TouchedSide]) -> Vec<Vec<Overlap>> {
        let view = self.at_canon(self.latest_tx);
        sides
            .iter()
            .map(|t| {
                let (entity, attr, side) = t.side;
                view.overlaps_of_touching(entity, attr, side, Some(&t.values))
            })
            .collect()
    }

    /// Interns edge `def`, defined by (committed or replayed) transaction
    /// `tx`. Infallible.
    fn apply_edge(&mut self, def: EdgeDef, tx: TxId) {
        debug_assert_eq!(
            def.id.seq(),
            self.edges.len() as u64 + 1,
            "invariant: edges are applied in sequence order"
        );
        self.edge_by_triple.insert(def.triple(), def.id);
        self.edges.push(def.triple());
        self.edges_defined_in.push(tx);
    }

    /// The edge interned for `(source, attr, target)`, if any: the ID the
    /// first stored assert of that `ref` fact gave it. `None` for a triple
    /// never asserted, an unknown attribute, an inverse name (inverse names
    /// are only read by `neighbors`), a non-ref attribute or an undefined
    /// entity; never an error.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        let attr = *self.attr_by_name.get(attr)?;
        self.edge_of(&StagedIds::default(), source, attr, target)
    }

    /// The triple `(source, attribute name, target)` of a defined edge, or
    /// `None` if `id` isn't defined in this world.
    pub fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
        let i = usize::try_from(id.seq().checked_sub(1)?).ok()?;
        let (source, attr, target) = *self.edges.get(i)?;
        Some((
            source,
            self.attributes[attr.0 as usize].name.clone(),
            target,
        ))
    }

    /// The last sequence number assigned to node type `t`, counting staged
    /// entities; `None` if the type isn't declared.
    fn last_seq(&self, staged: &StagedIds, t: NodeTypeId) -> Option<u64> {
        let committed = self.node_types.get(usize::from(t.0))?.last_seq();
        Some(staged.last_seq.get(&t).copied().unwrap_or(committed))
    }

    /// Creates an entity of the given node type, with an optional handle.
    ///
    /// The sequence number is the next one for that type, starting at 1.
    /// Handles are exact strings, unique across the whole world (not per
    /// type) and permanent. One auto-committed transaction (see
    /// [`Txn::create_entity`]): it advances the tx by one. A failed create
    /// changes nothing.
    pub fn create_entity(
        &mut self,
        node_type: &str,
        handle: Option<&str>,
    ) -> Result<EntityId, Error> {
        let mut txn = self.begin();
        let id = txn.create_entity(node_type, handle)?;
        txn.commit()?;
        Ok(id)
    }

    /// Validates a create without changing the world, counting the
    /// entities and handles `staged` by the creating transaction.
    fn prepare_entity(
        &self,
        staged: &StagedIds,
        node_type: &str,
        handle: Option<&str>,
    ) -> Result<EntityDef, Error> {
        let type_id = *self
            .node_type_by_name
            .get(node_type)
            .ok_or_else(|| Error::UnknownNodeType(node_type.to_owned()))?;
        if let Some(h) = handle {
            if h.is_empty() {
                return Err(Error::InvalidHandle(h.to_owned()));
            }
            if self.entity_by_handle.contains_key(h) || staged.handles.contains(h) {
                return Err(Error::DuplicateHandle(h.to_owned()));
            }
        }
        let last_seq = self
            .last_seq(staged, type_id)
            .expect("invariant: node_type_by_name only maps declared types");
        // `EntityId::new` rejects a sequence past 48 bits, so exhaustion is
        // an error rather than a wrap into the next node type's prefix.
        let id = EntityId::new(type_id, last_seq + 1)?;
        Ok(EntityDef {
            id,
            handle: handle.map(str::to_owned),
        })
    }

    /// Applies a definition produced by [`World::prepare_entity`], as part
    /// of the commit (or replay) of transaction `tx`. Infallible.
    fn apply_entity(&mut self, def: EntityDef, tx: TxId) {
        let nt = &mut self.node_types[usize::from(def.id.node_type().0)];
        debug_assert_eq!(
            def.id.seq(),
            nt.last_seq() + 1,
            "invariant: entity sequences are applied in order, without gaps"
        );
        nt.defined_in.push(tx);
        if let Some(h) = def.handle {
            self.handle_by_entity.insert(def.id, h.clone());
            self.entity_by_handle.insert(h, def.id);
        }
    }

    /// The entity with this handle, if any. Handles are matched exactly.
    pub fn entity_by_handle(&self, handle: &str) -> Option<EntityId> {
        self.entity_by_handle.get(handle).copied()
    }

    /// The handle of entity `id`, if it was created with one: the inverse
    /// of [`World::entity_by_handle`]. `None` for an entity without a
    /// handle, an edge or an ID that isn't defined in this world.
    pub fn handle_of(&self, id: EntityId) -> Option<&str> {
        self.handle_by_entity.get(&id).map(String::as_str)
    }

    /// Parses an ID in the form [`World::format_id`] writes: `TYPE#seq` for
    /// a node type this schema declares (matched exactly, case-sensitive),
    /// or `edge#seq` for an edge (the edge's ID in the entity space, see
    /// [`EntityId::from`]).
    ///
    /// `seq` is plain decimal: ASCII digits only, no sign, no leading zero,
    /// at least 1 and within the sequence range ([`MAX_SEQ`](crate::id::MAX_SEQ)
    /// for entities, [`MAX_EDGE_SEQ`](crate::id::MAX_EDGE_SEQ) for edges).
    /// Anything else is `None`: surrounding whitespace, an unknown type, a
    /// sequence that overflows, and the `#<type id>#seq` form `format_id`
    /// uses for a type the schema doesn't declare. Checks syntax and the
    /// declared type only, not whether the ID is defined yet: use it with
    /// [`World::entity_by_handle`] to resolve what a user typed.
    pub fn parse_id(&self, s: &str) -> Option<EntityId> {
        // The last `#` splits, so a type name containing `#` still parses;
        // anything with an extra `#` leaves a prefix no type has.
        let (prefix, digits) = s.rsplit_once('#')?;
        if digits.is_empty()
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        // Digits only, so the only parse failure is overflow.
        let seq: u64 = digits.parse().ok()?;
        // `edge` is what `format_id` writes for every edge, so it wins over
        // a node type of that name.
        if prefix == "edge" {
            return EdgeId::new(seq).ok().map(EntityId::from);
        }
        let t = *self.node_type_by_name.get(prefix)?;
        EntityId::new(t, seq).ok()
    }

    /// Every declared attribute, in ID (= declaration) order. Inverse names
    /// aren't attributes and aren't listed (they are on their attribute's
    /// [`AttrDef::inverse`]).
    pub fn attributes(&self) -> impl Iterator<Item = &AttrDef> {
        self.attributes.iter()
    }

    /// The declaration of attribute `name`, matched exactly. `None` for an
    /// unknown name and for an inverse name (see [`World::attributes`]).
    pub fn attribute(&self, name: &str) -> Option<&AttrDef> {
        let id = self.attr_by_name.get(name)?;
        self.attributes.get(id.0 as usize)
    }

    /// The name of attribute `id` (as stored in a log entry), or `None` if
    /// the schema declares no attribute with that ID.
    pub fn attr_name(&self, id: AttrId) -> Option<&str> {
        let i = usize::try_from(id.0).ok()?;
        self.attributes.get(i).map(|a| a.name.as_str())
    }

    /// Every declared node type name, in ID (= declaration) order.
    pub fn node_types(&self) -> impl Iterator<Item = &str> {
        self.node_types.iter().map(|nt| nt.name.as_str())
    }

    /// Every committed log entry, in entry-ID order: the same items
    /// [`World::entry`] returns for each ID, genesis included, `DEFINE`s
    /// and `Commit`s included. Entries of a transaction that was dropped or
    /// failed to commit are never listed. Not limited by canon (filter on
    /// [`EntryInfo::tx`] for that). Each item is cloned as it is yielded.
    pub fn entries(&self) -> impl Iterator<Item = EntryInfo> + '_ {
        self.entries.iter().cloned()
    }

    /// Formats an ID as `TYPE#seq` (`CHARACTER#1`), or an edge ID as
    /// `edge#seq` (`edge#9`).
    ///
    /// An ID whose node type isn't in this world's schema (e.g. decoded from
    /// foreign bytes) is shown as `#<type id>#seq` rather than failing; an
    /// edge-tagged ID is shown as `edge#seq` whether or not it is defined.
    pub fn format_id(&self, id: EntityId) -> String {
        if id.is_edge() {
            // Bits 0..=62: the edge sequence (0 for the invalid tagged ID).
            return format!("edge#{}", id.raw() & !(1 << 63));
        }
        let t = id.node_type();
        match self.node_types.get(usize::from(t.0)) {
            Some(nt) => format!("{}#{}", nt.name, id.seq()),
            None => format!("#{}#{}", t.0, id.seq()),
        }
    }
}

/// One `(entity, attr, side)` check a commit touches, with the values
/// (encoded, ascending, deduplicated) whose intervals it may change there.
#[derive(Debug)]
struct TouchedSide {
    side: (EntityId, AttrId, Side),
    values: Vec<Vec<u8>>,
}

/// Replay state spanning one transaction after the genesis, for the rules a
/// single entry can't check on its own.
#[derive(Debug, Default)]
struct ReplayTxn {
    /// Whether the transaction has any entry before its `Commit`.
    has_entries: bool,
    /// Edges the transaction defined whose triple it hasn't (yet) asserted.
    unasserted_edges: HashMap<Triple, EdgeId>,
    /// The keys (`E` key without `^tx`) of the transaction's facts,
    /// `DELETED` markers and revised facts so far.
    keys: HashSet<Vec<u8>>,
    /// `(of, rev)` of the `DELETED` marker that is the previous entry, if it
    /// is one: a `Revised` right after it shares its rev (a moved key).
    prev_deleted: Option<(EntryId, u32)>,
}

impl ReplayTxn {
    /// Records that the transaction writes an entry at `fact`'s key; `Err`
    /// if it already wrote one there (a live transaction stores at most one
    /// entry per key).
    fn claim_key(&mut self, fact: &FactDef) -> Result<(), String> {
        let mut k = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
        key::push_seek_tick(&mut k, fact.tick);
        if self.keys.insert(k) {
            Ok(())
        } else {
            Err(format!(
                "a second entry at one key at {:?} in one transaction",
                fact.tick
            ))
        }
    }
}

/// Replay: `rev` must be the next revision of `chain` (base entry `of`).
fn check_next_rev(chain: Chain, of: EntryId, rev: u32) -> Result<(), ReplayFault> {
    match chain.rev.checked_add(1) {
        Some(expected) if expected == rev => Ok(()),
        Some(expected) => Err(ReplayFault::RevisionSkip {
            of,
            expected,
            found: rev,
        }),
        None => Err(ReplayFault::Invalid(format!(
            "revision {rev} of entry#{}, which already has the most revisions",
            of.0
        ))),
    }
}

/// Why replay refused one entry: no live writer could have written it.
/// [`World::open`] turns any of them into [`Error::Corrupt`]; doctor reports
/// each as its own finding and replays on without the entry.
#[derive(Debug)]
pub(crate) enum ReplayFault {
    /// The entry uses an entity or edge ID that no earlier entry defines.
    DanglingId(EntityId),
    /// The entry uses an attribute ID the schema doesn't define.
    DanglingAttr(AttrId),
    /// A correction that names a missing or non-fact entry, isn't at its
    /// chain's current key, or corrects a deleted chain.
    BadCorrection(String),
    /// A correction whose revision isn't its chain's next one.
    RevisionSkip {
        /// The chain's base entry.
        of: EntryId,
        /// The chain's next revision.
        expected: u32,
        /// The revision the entry carries.
        found: u32,
    },
    /// A write a live writer refuses with this error (a value of the wrong
    /// type, a retracted event, an invalid value, ...).
    Refused(Error),
    /// Any other replay rule.
    Invalid(String),
}

impl ReplayFault {
    /// The fault of a fact a live write refuses with `e`.
    fn from_write(e: Error) -> ReplayFault {
        match e {
            Error::UnknownEntity(id) => ReplayFault::DanglingId(id),
            other => ReplayFault::Refused(other),
        }
    }
}

impl std::fmt::Display for ReplayFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayFault::DanglingId(id) => write!(f, "undefined ID {id:?}"),
            ReplayFault::DanglingAttr(attr) => write!(f, "unknown attribute {attr:?}"),
            ReplayFault::BadCorrection(why) | ReplayFault::Invalid(why) => f.write_str(why),
            ReplayFault::RevisionSkip {
                of,
                expected,
                found,
            } => write!(
                f,
                "revision {found} of entry#{} where {expected} was next",
                of.0
            ),
            ReplayFault::Refused(e) => write!(f, "{e}"),
        }
    }
}

/// Borrowed views of a world's stored indexes (see [`World::indexes`]).
pub(crate) struct StoredIndexes<'w> {
    pub(crate) e: &'w Index,
    pub(crate) r: &'w Index,
    pub(crate) v: &'w Index,
    pub(crate) e_latest: &'w LatestIndex,
    pub(crate) r_latest: &'w LatestIndex,
    pub(crate) e_precision: &'w Precisions,
    pub(crate) e_latest_precision: &'w Precisions,
    pub(crate) v_latest: &'w LatestIndex,
    pub(crate) vfold: &'w Index,
    pub(crate) vfold_latest: &'w LatestIndex,
}

/// The schema declared by the genesis transaction's entries (without its
/// Commit): node type and attribute `DEFINE`s only, each kind numbered
/// 0, 1, 2, ... in log order, as [`World::create`] writes them.
fn genesis_schema(entries: &[LogEntry]) -> Result<Schema, Error> {
    let mut schema = Schema::new();
    let (mut node_types, mut attrs) = (0usize, 0usize);
    for entry in entries {
        let corrupt = |why: String| Error::Corrupt(format!("genesis entry#{}: {why}", entry.id.0));
        match &entry.body {
            EntryBody::DefineNodeType { id, name } => {
                if usize::from(id.0) != node_types {
                    return Err(corrupt(format!("node type {id:?} out of sequence")));
                }
                node_types += 1;
                schema = schema.node_type(name);
            }
            EntryBody::DefineAttr { id, def } => {
                if usize::try_from(id.0).ok() != Some(attrs) {
                    return Err(corrupt(format!("attribute {id:?} out of sequence")));
                }
                attrs += 1;
                schema = schema.attribute(def.clone());
            }
            other => return Err(corrupt(format!("not a schema definition: {other:?}"))),
        }
    }
    Ok(schema)
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

/// A fact as a `Fact` or `Revised` entry says it: `(entity, attr, value,
/// tick, op, precision)`.
type FactFields<'b> = (EntityId, AttrId, &'b Value, Tick, Op, Precision);

/// The fact a `Fact` or `Revised` entry says; `None` for any other entry.
fn as_fact(body: &EntryBody) -> Option<FactFields<'_>> {
    match body {
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        }
        | EntryBody::Revised {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
            ..
        } => Some((*entity, *attr, value, *tick, *op, *precision)),
        _ => None,
    }
}

/// Borrows an owned key bound as a slice bound, for `BTreeMap::range`.
fn as_slice(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The result of committing a transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitResult {
    /// The transaction's number. If nothing was written (every write was a
    /// no-op and no entity was created), nothing is appended and this is
    /// the unchanged latest tx.
    pub tx: TxId,
    /// Entries stored by the transaction: facts, `DELETED` markers and
    /// revised facts
    /// (no-ops not counted).
    pub changed: usize,
    /// IDs of the fact, `DELETED` and `Revised` entries the transaction appended, in
    /// log order (`DEFINE`s and the `Commit` are not listed): one per stored
    /// entry. Empty if nothing was appended.
    pub entries: Vec<EntryId>,
    /// Edges newly interned by the transaction, in interning order (the
    /// order of their `DEFINE` entries). Re-asserting an already interned
    /// triple interns nothing.
    pub edges: Vec<EdgeId>,
    /// The cardinality overlaps the transaction introduced and resolved:
    /// its net effect, so an overlap made and resolved inside the
    /// transaction isn't reported (see [`Warnings`]). Empty if nothing was
    /// appended.
    pub warnings: Warnings,
}

/// A write transaction on a [`World`], started with [`World::begin`].
///
/// Writes are judged as they are made, against the committed facts *and*
/// the transaction's own earlier writes, then staged; [`Txn::commit`] makes
/// them all visible under one new tx. Dropping a `Txn` without committing
/// discards everything it staged, including created entities: their IDs and
/// handles are free again.
///
/// The first write that fails returns its own error and poisons the
/// transaction: every later write returns [`Error::TransactionAborted`], and
/// so does [`Txn::commit`], which then applies nothing. A no-op is not a
/// failure.
#[derive(Debug)]
pub struct Txn<'w> {
    world: &'w mut World,
    /// The world's latest tick when the transaction began: the tick of a
    /// write with no tick of its own and no transaction default.
    base_tick: Tick,
    default_tick: Option<Tick>,
    /// The tx this transaction commits as. Fixed at begin: the `Txn` holds
    /// the world mutably, so no other commit can take it first.
    tx: TxId,
    /// Staged entries (facts, `DELETED` markers, revised facts), keyed exactly as they
    /// will be in the `E` index (with `tx`), so judging seeks the committed
    /// and staged entries the same way.
    staged: Index,
    /// The staged entries in write order: the order of their log entries.
    entries: Vec<Staged>,
    /// Entities created and edges interned by this transaction.
    ids: StagedIds,
    /// Set by the first failed write; the transaction then applies nothing.
    poisoned: bool,
}

impl Txn<'_> {
    /// Sets the tick for this transaction's writes that don't give their
    /// own (`AT TICK` for the transaction). It wins over the world's latest
    /// tick; a write's own tick wins over it.
    pub fn set_default_tick(&mut self, t: Tick) {
        self.default_tick = Some(t);
    }

    /// Stages a new entity of the given node type, with an optional handle,
    /// and returns its ID.
    ///
    /// The ID and handle are assigned as by [`World::create_entity`],
    /// counting this transaction's earlier creates: sequences continue from
    /// them and handles must differ from them. Later writes in this
    /// transaction can use the entity as subject or `ref` value. It exists
    /// for everyone else only after [`Txn::commit`]. Fails (poisoning the
    /// transaction) with [`Error::UnknownNodeType`],
    /// [`Error::InvalidHandle`], [`Error::DuplicateHandle`] or
    /// [`Error::Id`].
    pub fn create_entity(
        &mut self,
        node_type: &str,
        handle: Option<&str>,
    ) -> Result<EntityId, Error> {
        self.guard(|txn| {
            let def = txn.world.prepare_entity(&txn.ids, node_type, handle)?;
            let id = def.id;
            txn.ids.push(def);
            Ok(id)
        })
    }

    /// Runs one write: refused if the transaction is already poisoned or
    /// the world refuses writes (failed log), and poisons it if the write
    /// fails.
    fn guard<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let res = self.world.writable().and_then(|()| f(self));
        self.poisoned = res.is_err();
        res
    }

    /// Stages an assert of `v` for `entity.attr` from `tick` on.
    ///
    /// The tick is `tick`, else the transaction default, else the world's
    /// latest tick captured at [`World::begin`]; the result reports it.
    /// Validated and judged like [`World::assert`], with the transaction's
    /// own earlier writes counted as stored. Nothing is visible until
    /// [`Txn::commit`].
    pub fn assert(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert, Precision::Exact)
    }

    /// [`Txn::assert`] with the tick's precision: staged with `p`, judged
    /// exactly like [`Txn::assert`] (see [`World::assert_with_precision`]).
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

    /// [`Txn::retract`] with the tick's precision: staged with `p`, judged
    /// exactly like [`Txn::retract`].
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

    /// Stages a retract of `v` for `entity.attr` from `tick` on.
    ///
    /// Ticks default as for [`Txn::assert`]. Validated and judged like
    /// [`World::retract`], with the transaction's own earlier writes
    /// counted as stored.
    pub fn retract(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Retract, Precision::Exact)
    }

    /// The edge interned for `(source, attr, target)`, as
    /// [`World::edge_id`] but also seeing edges interned earlier in this
    /// transaction.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        let attr = *self.world.attr_by_name.get(attr)?;
        self.world.edge_of(&self.ids, source, attr, target)
    }

    /// Resolves the tick, validates, judges against committed + staged
    /// facts, and stages the fact unless it is a no-op.
    fn write(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        tick: Option<Tick>,
        op: Op,
        precision: Precision,
    ) -> Result<WriteResult, Error> {
        self.guard(|txn| {
            let tick = tick.or(txn.default_tick).unwrap_or(txn.base_tick);
            let mut fact = txn
                .world
                .prepare_fact(&txn.ids, entity, attr, v, tick, op)?;
            // A payload only: judged below exactly as an exact fact.
            fact.precision = precision;
            check_entry_size(attr, &fact)?;
            if judge(&txn.world.e_index, &txn.staged, &fact, entity, attr)? == Verdict::Noop {
                // Find-then-act: nothing to change, so nothing is staged.
                return Ok(WriteResult {
                    changed: 0,
                    tick,
                    entries: Vec::new(),
                    edges: Vec::new(),
                    warnings: Warnings::default(),
                });
            }
            // A stored retract never needs an edge: it requires a stored
            // assert, which interned it.
            txn.intern_edge(&fact)?;
            txn.stage(Staged::Fact(fact));
            Ok(WriteResult {
                changed: 1,
                tick,
                entries: Vec::new(),
                edges: Vec::new(),
                warnings: Warnings::default(),
            })
        })
    }

    /// Stages a correction of the committed fact entry `entry`: see
    /// [`World::correct`]. Nothing is visible until [`Txn::commit`], whose
    /// result lists the correction's entry IDs (this result's `entries` is
    /// empty).
    ///
    /// Fails (poisoning the transaction) as [`World::correct`] does, judged
    /// against the committed world plus this transaction's staged entries,
    /// and with [`Error::TxnKeyConflict`] if this transaction already staged
    /// an entry at the corrected key (e.g. it corrected the same chain
    /// before, even if this correction would be a no-op) or at a replaced
    /// fact's new key.
    pub fn correct(&mut self, entry: EntryId, c: Correction) -> Result<WriteResult, Error> {
        self.guard(|txn| {
            let Target {
                base: of,
                chain,
                fact: current,
                value: current_value,
            } = txn.world.correction_target(entry)?;
            let attr = txn.world.attributes[current.attr.0 as usize].name.clone();
            // Judged against the committed chain: if this transaction already
            // corrected it, that state is stale, so refuse before anything
            // else (a "no-op" against stale state could be a real change).
            if has_key(&txn.staged, &current) {
                return Err(Error::TxnKeyConflict {
                    entity: current.entity,
                    attr,
                    tick: current.tick,
                });
            }
            // Every correction entry carries the current value (a `DELETED`
            // marker) or a value derived from it, so the current value must
            // fit a correction entry before anything is staged. A value that
            // fit only as a plain fact (a crafted log) is refused here with
            // the typed error, not later by the log writer.
            check_entry_size(&attr, &current)?;
            let rev = || chain.rev.checked_add(1).ok_or(Error::TooManyRevisions(of));
            let (value, tick, op, precision) = match c {
                Correction::Delete => {
                    let tick = current.tick;
                    txn.stage(Staged::Deleted {
                        of,
                        rev: rev()?,
                        fact: current,
                    });
                    return Ok(written(1, tick));
                }
                Correction::Replace {
                    value,
                    tick,
                    op,
                    precision,
                } => (value, tick, op, precision),
            };
            // The new fact is the current one with the given fields
            // replaced, validated like a write. The subject is the current
            // fact's (stored) entity; a bidirectional pair is renormalised,
            // which may swap it with the value.
            let mut new = txn.world.prepare_fact(
                &txn.ids,
                current.entity,
                &attr,
                value.as_ref().unwrap_or(&current_value),
                tick.unwrap_or(current.tick),
                op.unwrap_or(current.op),
            )?;
            // Kept unless given, also when the key moves.
            new.precision = precision.unwrap_or(current.precision);
            check_entry_size(&attr, &new)?;
            let tick = new.tick;
            let same_key = (new.entity, &new.value, new.tick)
                == (current.entity, &current.value, current.tick);
            if same_key {
                if (new.op, new.precision) == (current.op, current.precision) {
                    // Nothing changed: find-then-act, nothing staged.
                    return Ok(written(0, tick));
                }
                // The chain's own entry is the current one at its key, so
                // changing the op or the precision there can't conflict
                // with anything else.
                txn.intern_edge(&new)?;
                txn.stage(Staged::Revised {
                    of,
                    rev: rev()?,
                    fact: new,
                });
                return Ok(written(1, tick));
            }
            // A moved key: the same-tick rule at the new key, judged at the
            // latest canon plus the staged entries. The hold-based no-op
            // rules don't apply: this changes what canon says, not the story.
            let layers = [&txn.world.e_index, &txn.staged];
            let mut prefix = key::e_value_prefix(new.entity, new.attr, new.key_value());
            let there = op_at_tick(&layers, &mut prefix, new.tick, TxId(u64::MAX));
            match there {
                Some(stored) if stored != new.op => Err(Error::SameTickConflict {
                    entity: new.entity,
                    attr,
                    tick,
                }),
                Some(_) => {
                    // The new key already says this: only the old key goes.
                    txn.stage(Staged::Deleted {
                        of,
                        rev: rev()?,
                        fact: current,
                    });
                    Ok(written(1, tick))
                }
                None => {
                    if has_key(&txn.staged, &new) {
                        return Err(Error::TxnKeyConflict {
                            entity: new.entity,
                            attr,
                            tick,
                        });
                    }
                    let rev = rev()?;
                    txn.intern_edge(&new)?;
                    txn.stage(Staged::Deleted {
                        of,
                        rev,
                        fact: current,
                    });
                    txn.stage(Staged::Revised { of, rev, fact: new });
                    Ok(written(2, tick))
                }
            }
        })
    }

    /// Stages the edge a stored assert of the `ref` fact `fact` interns, if
    /// its triple has none yet (committed or staged). Retracts and non-ref
    /// facts intern nothing.
    fn intern_edge(&mut self, fact: &FactDef) -> Result<(), Error> {
        if let (Some(target), Op::Assert) = (fact.target, fact.op)
            && let Some(edge) =
                self.world
                    .prepare_edge(&self.ids, fact.entity, fact.attr, target)?
        {
            self.ids.push_edge(edge);
        }
        Ok(())
    }

    /// Stages one entry: its `E` key in the staged index (so later writes
    /// in the transaction are judged against it) and the entry itself, in
    /// write order.
    fn stage(&mut self, entry: Staged) {
        let (fact, mark) = entry.key_and_mark();
        // A value new to the world is held here until the commit applies it.
        if let Some(r) = fact.value_ref()
            && self.world.values.get(&fact.value).is_none()
        {
            self.ids.values.insert(&fact.value, r);
        }
        let k = key::e_fact_key(fact.entity, fact.attr, fact.key_value(), fact.tick, self.tx);
        self.staged.insert(k, mark);
        self.entries.push(entry);
    }

    /// Makes every staged entity, write and correction visible as one new
    /// transaction and returns its tx and the number of entries stored.
    ///
    /// With nothing staged (only no-ops), nothing is appended and the tx
    /// doesn't advance; a transaction that only creates entities does
    /// advance it. Fails with [`Error::TransactionAborted`], applying
    /// nothing, if a write in the transaction failed.
    ///
    /// In a file-backed world the transaction is appended to the log (entity
    /// and edge `DEFINE`s, then facts, `DELETED` markers and revised facts in write order,
    /// then `COMMIT`) in one write and
    /// fsynced, and only then applied in memory. If that fails, the commit
    /// returns [`Error::Io`], nothing is applied, and the world refuses
    /// further writes.
    pub fn commit(self) -> Result<CommitResult, Error> {
        self.commit_with(true)
    }

    /// [`Txn::commit`] without the write warnings: the returned
    /// [`CommitResult::warnings`] is always empty. Everything stored (log
    /// bytes, entry and edge IDs, tx numbering) is identical to `commit`.
    ///
    /// For the fixture loader, which discards warnings: computing them
    /// rescans each touched cardinality-one side's whole history and is
    /// quadratic in the overlapping values one transaction touches
    /// (security review H1/H2).
    pub(crate) fn commit_quiet(self) -> Result<CommitResult, Error> {
        self.commit_with(false)
    }

    /// The body of [`Txn::commit`]; `warn` selects whether the overlap
    /// warnings diff is computed. Nothing else depends on it.
    fn commit_with(self, warn: bool) -> Result<CommitResult, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let world = self.world;
        // Before anything else, an empty transaction included: a read-only
        // world or an unacknowledged recovery refuses every write.
        world.accepts_writes()?;
        let changed = self.entries.len();
        if changed == 0 && self.ids.is_empty() {
            return Ok(CommitResult {
                tx: world.latest_tx,
                changed: 0,
                entries: Vec::new(),
                edges: Vec::new(),
                warnings: Warnings::default(),
            });
        }
        // The overlaps of everything the transaction touches, before it
        // lands: half of the warnings diff. Read before the append, so a
        // failed append has nothing to undo.
        let (touched, before) = if warn {
            let touched =
                world.touched_side_values(self.entries.iter().map(|e| e.key_and_mark().0));
            let before = world.overlaps_of_sides(&touched);
            (touched, before)
        } else {
            (Vec::new(), Vec::new())
        };
        // Entity DEFINEs, then edge DEFINEs (whose endpoints may be new
        // entities), precede every fact, so no fact uses an ID before the
        // log defines it. Both kinds of world encode the transaction, so
        // they number (and checksum) its entries alike.
        let entities = self.ids.defs.iter().map(|d| NewEntry::Entity {
            id: d.id,
            handle: d.handle.as_deref(),
        });
        let edges = self.ids.edges.iter().map(|d| NewEntry::Edge {
            id: d.id,
            source: d.source,
            attr: d.attr,
            target: d.target,
        });
        let facts = self.entries.iter().map(|e| match e {
            Staged::Revised { of, rev, fact: f } => NewEntry::Revised {
                of: *of,
                rev: *rev,
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
                op: f.op,
                precision: f.precision,
            },
            Staged::Fact(f) => NewEntry::Fact {
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
                op: f.op,
                precision: f.precision,
            },
            Staged::Deleted { of, rev, fact: f } => NewEntry::Deleted {
                of: *of,
                rev: *rev,
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
            },
        });
        let encoded = log::encode_txn(
            world.next_entry(),
            self.tx,
            entities.chain(edges).chain(facts),
        )?;
        // Write-ahead: the transaction is durable in the log before anything
        // in memory changes, so a failed append leaves the world as it was.
        if let Some(log) = world.log.as_mut() {
            log.append(&encoded)?;
        }
        for def in self.ids.defs {
            world.apply_entity(def, self.tx);
        }
        let edges = self.ids.edges.iter().map(|d| d.id).collect();
        for def in self.ids.edges {
            world.apply_edge(def, self.tx);
        }
        // The staged index holds exactly these entries' E keys (it exists so
        // judging can see them); applying them rewrites those keys and adds
        // their R mirrors through the same path as replay.
        // The staged entries are exactly the encoded Fact / Deleted / Revised
        // entries, in the same order: that gives each its entry ID.
        let entries: Vec<EntryId> = encoded
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    EntryBody::Fact { .. } | EntryBody::Deleted { .. } | EntryBody::Revised { .. }
                )
            })
            .map(|e| e.id)
            .collect();
        for (entry, id) in self.entries.iter().zip(&entries) {
            let (fact, mark) = entry.key_and_mark();
            world.index_entry(fact, mark, self.tx);
            match entry {
                Staged::Fact(_) => {}
                Staged::Deleted { of, rev, .. } => {
                    let chain = Chain {
                        rev: *rev,
                        current: None,
                    };
                    world.chains.insert(*of, chain);
                }
                Staged::Revised { of, rev, .. } => {
                    let chain = Chain {
                        rev: *rev,
                        current: Some(*id),
                    };
                    world.chains.insert(*of, chain);
                }
            }
        }
        world.latest_tx = self.tx;
        world.record(encoded.entries, self.tx);
        let warnings = if warn {
            let after = world.overlaps_of_sides(&touched);
            overlap_diff(before, after)
        } else {
            Warnings::default()
        };
        Ok(CommitResult {
            tx: self.tx,
            changed,
            entries,
            edges,
            warnings,
        })
    }
}

/// A read-only view of a [`World`] pinned to one canon tx, from
/// [`World::at_canon`]: it reads only facts written by transactions `<=`
/// that tx, so it never sees half a transaction or anything committed later.
///
/// Entity existence is not pinned: an entity created after the view's canon
/// is still a valid subject (with no facts visible), and one that doesn't
/// exist in the current world is [`Error::UnknownEntity`]. Only facts are
/// filtered by canon.
#[derive(Clone, Copy, Debug)]
pub struct View<'w> {
    world: &'w World,
    /// The newest tx visible; never past the world's latest tx.
    canon: TxId,
    /// Read the latest-revision indexes (`canon` is the latest tx) instead
    /// of the main ones with the canon skip.
    latest: bool,
}

impl<'w> View<'w> {
    /// Every entity of node type `node_type` defined in this canon (its
    /// `DEFINE` in a transaction `<=` the view's canon), ascending by
    /// sequence number (so by [`EntityId`]): `NodeScan` by type. Never an
    /// entity of another type, never an edge; an entity with no facts is
    /// listed. Node type names are exact (case-sensitive).
    ///
    /// Answered from the ID tables, not the facts: finding the canon's
    /// entities is one binary search, then the iterator yields each ID
    /// lazily. Fails with [`Error::UnknownNodeType`] for a type the schema
    /// doesn't declare; a declared type with no entities is empty.
    pub fn entities_of_type(
        &self,
        node_type: &str,
    ) -> Result<impl Iterator<Item = EntityId> + use<'w>, Error> {
        let world = self.world;
        let type_id = *world
            .node_type_by_name
            .get(node_type)
            .ok_or_else(|| Error::UnknownNodeType(node_type.to_owned()))?;
        let canon = self.canon;
        let defined_in = &world.node_types[usize::from(type_id.0)].defined_in;
        // Entities are defined in tx order, so those visible at the canon
        // are a prefix of the type's sequences.
        let visible = defined_in.partition_point(|&tx| tx <= canon) as u64;
        Ok((1..=visible).map(move |seq| {
            EntityId::new(type_id, seq).expect("invariant: assigned entity sequences fit an ID")
        }))
    }
}

impl View<'_> {
    /// The canon tx this view reads at.
    pub fn canon(&self) -> TxId {
        self.canon
    }

    /// Whether the edge with ID `id` was defined at this view's canon (its
    /// `DEFINE` is in a transaction `<=` the canon).
    fn edge_visible(&self, id: EdgeId) -> bool {
        id.seq()
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.world.edges_defined_in.get(i))
            .is_some_and(|&tx| tx <= self.canon)
    }

    /// The triple `(source, attribute name, target)` of edge `id`, as
    /// [`World::edge`], but `None` if the edge's `DEFINE` is in a
    /// transaction after this view's canon.
    pub fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
        self.edge_visible(id).then(|| self.world.edge(id))?
    }

    /// The edge interned for `(source, attr, target)`, as [`World::edge_id`]
    /// (a bidirectional pair in either order), but `None` if the edge's
    /// `DEFINE` is in a transaction after this view's canon.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        self.world
            .edge_id(source, attr, target)
            .filter(|&id| self.edge_visible(id))
    }

    /// The latest tick at this canon: the highest tick of any fact (assert
    /// or retract) visible here, or `Tick(0)` if there is none. A fact that
    /// a correction visible here deleted (or moved away) doesn't count; one
    /// deleted only by a later correction does. At the latest canon this is
    /// [`World::latest_tick`].
    ///
    /// Cost: at the latest canon, one lookup in the world's per-tick counts.
    /// At an older canon, a walk over the whole `E` index (every stored
    /// entry), so linear in the size of the world: fine for milestone 0's
    /// in-memory worlds, not for a hot path.
    pub fn latest_tick(&self) -> Tick {
        if self.latest {
            return self.world.latest_tick();
        }
        let canon = self.canon;
        let mut best: Option<Tick> = None;
        // The key (history and tick) whose current entry at the canon was
        // already found: its older entries are skipped.
        let mut done: Option<&[u8]> = None;
        for (k, mark) in &self.world.e_index {
            let at_key = key::without_tx(k);
            if done == Some(at_key) {
                continue;
            }
            // Within one key, entries are newest tx first, so the first
            // with `tx <= canon` is the key's current entry at the canon.
            let Some((tick, tx)) = key::e_key_tick_and_tx(k) else {
                continue;
            };
            if tx > canon {
                continue;
            }
            done = Some(at_key);
            if mark.op().is_some() && best.is_none_or(|b| tick > b) {
                best = Some(tick);
            }
        }
        best.unwrap_or(Tick(0))
    }

    /// Whether value `v` of `entity.attr` holds at tick `t` in this canon.
    ///
    /// Takes the latest op with tick `<= t` for that `(entity, attr, v)`
    /// among the visible transactions (ticks whose current entry is a
    /// `DELETED` marker skipped): it holds if that op is an assert, and not
    /// if it is a retract or there is none. Fails with
    /// [`Error::UnknownAttribute`], [`Error::UnknownEntity`],
    /// [`Error::WrongValueType`] for a value of another type than the
    /// attribute's, or [`Error::Value`] for NaN.
    pub fn as_of_value(
        &self,
        entity: EntityId,
        attr: &str,
        v: &Value,
        t: Tick,
    ) -> Result<bool, Error> {
        let world = self.world;
        let (attr_id, def) = world.resolve(&StagedIds::default(), entity, attr)?;
        check_type(attr, def, v)?;
        // A bidirectional pair is stored once, normalised: look it up there.
        let mut prefix = match v {
            Value::Ref(target) if world.is_bidirectional(attr_id) => {
                let (source, target) = world.stored_pair(attr_id, entity, *target);
                key::e_value_prefix(source, attr_id, &Value::Ref(target).encode()?)
            }
            _ => {
                let encoded = v.encode()?;
                // An unindexed value never stored has no history.
                let Some(part) = world.committed_key_part(attr_id, &encoded) else {
                    return Ok(false);
                };
                key::e_value_prefix(entity, attr_id, &part)
            }
        };
        Ok(self.holds_e(&mut prefix, t))
    }

    /// Whether the `E` history `value_prefix` (`E|e|a|v|`) holds at `t` in
    /// this canon: one seek in `E_latest` at the latest canon, else in the
    /// main `E` index with the canon skip. Reuses `value_prefix` as the seek
    /// key buffer; it is restored before returning.
    fn holds_e(&self, value_prefix: &mut Vec<u8>, t: Tick) -> bool {
        if self.latest {
            holds_latest(&self.world.e_latest, value_prefix, t)
        } else {
            holds(&[&self.world.e_index], value_prefix, t, self.canon)
        }
    }

    /// Every value of `entity.attr` that holds at tick `t` in this canon,
    /// sorted by value encoding (the value's natural order within its type;
    /// for `ref`, entity ID order).
    ///
    /// For a bidirectional attribute these are `entity`'s partners from
    /// either side (each pair is stored once, under its smaller ID), each
    /// once, so both ends of a pair agree.
    ///
    /// Cost grows with the number of distinct values the attribute has ever
    /// had: one seek per value (plus skipped newer-tx entries at an older
    /// canon). Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        let world = self.world;
        let (attr_id, _) = world.resolve(&StagedIds::default(), entity, attr)?;
        if world.is_bidirectional(attr_id) {
            // Ref encoding order is entity ID order, so sorting the IDs
            // sorts by value encoding.
            let mut partners = Vec::new();
            self.for_each_partner_holding(entity, attr_id, true, true, t, |p| partners.push(p));
            partners.sort_unstable();
            partners.dedup();
            return Ok(partners.into_iter().map(Value::Ref).collect());
        }
        let mut out = Vec::new();
        self.for_each_e_value_holding(entity, attr_id, t, |value| {
            // An unresolvable key (only by tampering) is skipped.
            if let Some(v) = world.decode_e_part(attr_id, value) {
                out.push(v);
            }
        });
        if world.keys_by_ref(attr_id) {
            // Value references sort by hash, not by value: restore the
            // value-encoding order every other attribute has by key order.
            out.sort_by_cached_key(|v| v.encode().ok());
        }
        Ok(out)
    }

    /// [`View::as_of`] with detail: for every value of `entity.attr` that
    /// holds at tick `t` in this canon, the [`Change`] that makes it hold,
    /// i.e. its latest op with tick `<= t` (an assert), with that op's tick
    /// and [`Precision`]. Sorted by value encoding, so the values are
    /// exactly [`View::as_of`]'s, in the same order; for a bidirectional
    /// attribute each partner once, from either side.
    ///
    /// Same cost as [`View::as_of`]: one seek per value the attribute has
    /// ever had. Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn as_of_detail(
        &self,
        entity: EntityId,
        attr: &str,
        t: Tick,
    ) -> Result<Vec<Change>, Error> {
        let world = self.world;
        let (attr_id, _) = world.resolve(&StagedIds::default(), entity, attr)?;
        // (value encoding, change), sorted by the encoding at the end.
        let mut out: Vec<(Vec<u8>, Change)> = Vec::new();
        self.for_each_e_history(entity, attr_id, |p| {
            let Some((k, Op::Assert)) = self.entry_at(&world.e_index, &world.e_latest, p, t) else {
                return;
            };
            // An unresolvable or mistyped key (only by tampering) is skipped.
            let Some((bytes, value)) = world.decode_e_value(attr_id, &p[key::E_ATTR_PREFIX_LEN..])
            else {
                return;
            };
            let change = Change {
                tick: tick_after(k, p.len()),
                op: Op::Assert,
                value,
                precision: self.precision_at(k),
            };
            out.push((bytes.to_vec(), change));
        });
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner, through their R
            // mirrors; a self pair was found on the E side.
            let prefix = key::r_attr_prefix(entity, attr_id);
            let source_end = |_: &[u8]| key::R_SOURCE_PREFIX_LEN;
            let mut on_history = |p: &mut Vec<u8>| {
                let Some((k, Op::Assert)) = self.entry_at(&world.r_index, &world.r_latest, p, t)
                else {
                    return;
                };
                let Some(source) = key::r_source(p).filter(|s| *s != entity) else {
                    return;
                };
                let tick = tick_after(k, p.len());
                // A latest-revision key has no tx (and the latest path
                // ignores it); a main R key ends in its entry's.
                let tx = key::e_key_tx(k).filter(|_| !self.latest);
                let value = Value::Ref(source);
                let bytes = value
                    .encode()
                    .expect("invariant: a ref value always encodes");
                let change = Change {
                    tick,
                    op: Op::Assert,
                    value,
                    precision: self.r_precision(
                        source,
                        attr_id,
                        entity,
                        tick,
                        tx.unwrap_or(TxId(0)),
                    ),
                };
                out.push((bytes, change));
            };
            if self.latest {
                for_each_history(&world.r_latest, &prefix, source_end, &mut on_history);
            } else {
                for_each_history(&world.r_index, &prefix, source_end, &mut on_history);
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.dedup_by(|a, b| a.0 == b.0);
        Ok(out.into_iter().map(|(_, c)| c).collect())
    }

    /// The key and op of the current entry of the latest tick `<= t` in
    /// the history `value_prefix` (`E|e|a|v|` or `R|t|a|s|`) of the index
    /// this view reads: `latest` at the latest canon, else `main` with the
    /// canon skip (`DELETED` ticks skipped). Reuses `value_prefix` as the
    /// seek key buffer; it is restored before returning.
    fn entry_at<'i>(
        &self,
        main: &'i Index,
        latest: &'i LatestIndex,
        value_prefix: &mut Vec<u8>,
        t: Tick,
    ) -> Option<(&'i [u8], Op)> {
        if self.latest {
            return latest_seek(latest, value_prefix, t);
        }
        let len = value_prefix.len();
        key::push_seek_tick(value_prefix, t);
        let found = seek_entry(&[main], value_prefix, &value_prefix[..len], self.canon);
        value_prefix.truncate(len);
        found
    }

    /// Every entity for which `attr = v` holds at tick `t` in this canon,
    /// ascending by [`EntityId`], each once: "the entity named Edwin".
    ///
    /// Holding is judged as in [`View::as_of_value`]: the latest op with
    /// tick `<= t` for `(e, attr, v)` among the visible transactions
    /// (`DELETED` ticks skipped) is an assert, so an event holds from its
    /// tick on. An edge property finds the edge's entity ID.
    ///
    /// A non-ref attribute is answered from the `V` index (`V_latest` at
    /// the latest canon, the main `V` with the canon skip at an older one):
    /// one seek per entity that ever had `attr = v`. A `ref` attribute is
    /// answered from `R`, whatever its `indexed` flag: for `Ref(x)`, every
    /// source `s` of `(s, attr, x)`, and for a bidirectional attribute the
    /// partners of `x` on either side.
    ///
    /// Fails with [`Error::UnknownAttribute`] (an inverse name included),
    /// [`Error::NotIndexed`] for a non-ref attribute declared unindexed,
    /// [`Error::WrongValueType`], or [`Error::Value`] for NaN. `-0.0` finds
    /// `0.0`, as it is stored. A string too long to be indexed, or a `Ref`
    /// to an undefined entity, can't have been written, so it finds nothing.
    pub fn lookup(&self, attr: &str, v: &Value, t: Tick) -> Result<Vec<EntityId>, Error> {
        let world = self.world;
        let attr_id = *world
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        let def = &world.attributes[attr_id.0 as usize];
        if def.value_type != ValueType::Ref && !def.indexed {
            return Err(Error::NotIndexed(attr.to_owned()));
        }
        check_type(attr, def, v)?;
        // Encoding rejects NaN and normalises -0.0, exactly as a write does.
        let encoded = v.encode()?;
        let mut out = Vec::new();
        match v {
            Value::Ref(x) => {
                if !world.is_defined(&StagedIds::default(), *x) {
                    return Ok(out);
                }
                let both = world.is_bidirectional(attr_id);
                self.for_each_partner_holding(*x, attr_id, both, true, t, |s| out.push(s));
                // Two sides (and a self pair, stored under both) may repeat.
                out.sort_unstable();
                out.dedup();
            }
            _ => {
                if v.check_indexed().is_err() {
                    return Ok(out);
                }
                let prefix = key::v_value_prefix(attr_id, &encoded);
                // V keys are fixed-width from the value on: `V|a|v|e|` is
                // the history of one entity, in entity order.
                let entity_end = |k: &[u8]| (prefix.len() + key::V_ENTITY_LEN).min(k.len());
                self.for_each_value_holder(
                    &world.v_index,
                    &world.v_latest,
                    &prefix,
                    entity_end,
                    t,
                    |e| out.push(e),
                );
            }
        }
        Ok(out)
    }

    /// Every entity (or edge) with ANY value of the string attribute `attr`
    /// holding at tick `t` in this canon whose [`fold_for_search`] equals
    /// `fold_for_search(text)`: a case- and normalisation-insensitive
    /// [`View::lookup`] ("edwin" finds "Edwin" and "EDWIN"). Ascending by
    /// [`EntityId`], each once.
    ///
    /// Each original value keeps its own history, judged latest-wins as in
    /// [`View::lookup`]: an entity is found if at least one of its values
    /// that fold equal holds at `t`. Answered from `VFold_latest` at the
    /// latest canon and from the main `VFold` with the canon skip at an
    /// older one: one seek per `(original value, entity)` that ever folded
    /// to the text.
    ///
    /// Fails with [`Error::UnknownAttribute`] (an inverse name included),
    /// then [`Error::NotIndexed`] for an attribute declared unindexed (of
    /// any type), then [`Error::WrongValueType`] for a non-string
    /// attribute. The text has no length limit: a fold no stored value has
    /// finds nothing.
    pub fn lookup_ci(&self, attr: &str, text: &str, t: Tick) -> Result<Vec<EntityId>, Error> {
        let world = self.world;
        let attr_id = *world
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        let def = &world.attributes[attr_id.0 as usize];
        if !def.indexed {
            return Err(Error::NotIndexed(attr.to_owned()));
        }
        if def.value_type != ValueType::Str {
            return Err(Error::WrongValueType {
                attr: attr.to_owned(),
                expected: def.value_type,
                got: ValueType::Str,
            });
        }
        let fold = Value::Str(fold_for_search(text))
            .encode()
            .expect("invariant: encoding a string never fails");
        // The fold's encoding is self-delimiting, so `F|a|fold|` never
        // matches a longer fold it is a byte prefix of.
        let prefix = key::vfold_prefix(attr_id, &fold);
        let fold_prefix_len = prefix.len();
        let history_end = |k: &[u8]| key::vfold_history_end(fold_prefix_len, k);
        let mut out = Vec::new();
        self.for_each_value_holder(
            &world.vfold_index,
            &world.vfold_latest,
            &prefix,
            history_end,
            t,
            |e| out.push(e),
        );
        // Keys run by original value first, so one entity may hold several
        // values that fold equal, and entities aren't in order across them.
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Calls `f`, in key order, with the entity of every history under
    /// `value_prefix` in a value index (`main` and its latest-revision copy
    /// `latest`) that holds at `t` in this canon. `history_end(key)` is the
    /// length of a key's `…|entity|` history prefix, whose last 8 bytes are
    /// the entity. Shared by every value index layout.
    fn for_each_value_holder(
        &self,
        main: &Index,
        latest: &LatestIndex,
        value_prefix: &[u8],
        history_end: impl Fn(&[u8]) -> usize,
        t: Tick,
        mut f: impl FnMut(EntityId),
    ) {
        let f = |p: &[u8]| {
            f(key::v_prefix_entity(p)
                .expect("invariant: a value index history prefix ends in its entity"))
        };
        if self.latest {
            let holds_at = |p: &mut Vec<u8>| holds_latest(latest, p, t);
            for_each_holding(latest, value_prefix, history_end, holds_at, f);
        } else {
            let holds_at = |p: &mut Vec<u8>| holds(&[main], p, t, self.canon);
            for_each_holding(main, value_prefix, history_end, holds_at, f);
        }
    }

    /// Calls `f` with every neighbor `x` of `entity` through the `ref`
    /// attribute `attr` whose pair holds at `t` in this canon: targets of
    /// `E|entity|attr|` if `out`, sources of `R|entity|attr|` if `inn`.
    /// Unsorted across the two sides, and a self pair (stored under both)
    /// is reported by each side read.
    fn for_each_partner_holding(
        &self,
        entity: EntityId,
        attr: AttrId,
        out: bool,
        inn: bool,
        t: Tick,
        mut f: impl FnMut(EntityId),
    ) {
        if out {
            let world = self.world;
            self.for_each_e_value_holding(entity, attr, t, |value| {
                // An unresolvable key, or one holding a plain value on this
                // ref attribute (only by tampering), is skipped.
                if let Some(Value::Ref(target)) = world.decode_e_part(attr, value) {
                    f(target);
                }
            });
        }
        if inn {
            let prefix = key::r_attr_prefix(entity, attr);
            // Both R layouts are fixed-width up to and including the source.
            let source_end = |_: &[u8]| key::R_SOURCE_PREFIX_LEN;
            let f =
                |p: &[u8]| f(key::r_source(p).expect("invariant: R index keys are fixed-width"));
            let world = self.world;
            if self.latest {
                let index = &world.r_latest;
                let holds_at = |p: &mut Vec<u8>| holds_latest(index, p, t);
                for_each_holding(index, &prefix, source_end, holds_at, f);
            } else {
                let index = &world.r_index;
                let holds_at = |p: &mut Vec<u8>| holds(&[index], p, t, self.canon);
                for_each_holding(index, &prefix, source_end, holds_at, f);
            }
        }
    }

    /// Calls `f` with the encoded value of every value of `entity.attr`
    /// that holds at `t` in this canon, in value-encoding order: the
    /// distinct-value walk under `E|entity|attr|`.
    fn for_each_e_value_holding(
        &self,
        entity: EntityId,
        attr: AttrId,
        t: Tick,
        mut f: impl FnMut(&[u8]),
    ) {
        self.for_each_e_history(entity, attr, |p| {
            if self.holds_e(p, t) {
                f(&p[key::E_ATTR_PREFIX_LEN..]);
            }
        });
    }

    /// Calls `f` with the history prefix `E|entity|attr|value` of every
    /// value `entity.attr` has ever had in the index this view reads, in
    /// key order (value-encoding order, value references aside).
    fn for_each_e_history(&self, entity: EntityId, attr: AttrId, f: impl FnMut(&mut Vec<u8>)) {
        let prefix = key::e_attr_prefix(entity, attr);
        let world = self.world;
        // Keys in both E indexes are only ever built by `key::e_fact_key`
        // (the latest one without its `^tx`) from a successful
        // `Value::encode`, so they always parse.
        if self.latest {
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx")
                        .0
                        .len()
            };
            for_each_history(&world.e_latest, &prefix, value_end, f);
        } else {
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::e_value_bytes(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key")
                        .len()
            };
            for_each_history(&world.e_index, &prefix, value_end, f);
        }
    }

    /// The current entry of every `E` key under `attr_prefix` (`E|e|a|`) in
    /// this canon, `DELETED` ones left out, as `(tick, value encoding, op,
    /// precision)` in key order.
    fn e_current<'v>(&'v self, attr_prefix: &'v [u8]) -> Vec<(Tick, &'v [u8], Op, Precision)> {
        let world = self.world;
        if self.latest {
            under(&world.e_latest, attr_prefix)
                .map(|(k, op)| {
                    let (value, tick) = key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx");
                    (tick, value, *op, self.precision_at(k))
                })
                .collect()
        } else {
            current_entries(&world.e_index, attr_prefix, self.canon)
                .map(|(k, op)| {
                    let (value, tick) = key::e_value_and_tick(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key");
                    (tick, value, op, self.precision_at(k))
                })
                .collect()
        }
    }

    /// The precision of the fact stored at `E` key `k` of the index this
    /// view reads (`E_latest` keys at the latest canon, main `E` keys
    /// otherwise): [`Precision::Exact`] unless a vague one is recorded.
    fn precision_at(&self, k: &[u8]) -> Precision {
        let precisions = if self.latest {
            &self.world.e_latest_precision
        } else {
            &self.world.e_precision
        };
        precisions.get(k).copied().unwrap_or_default()
    }

    /// The precision of the fact an `R` entry mirrors: the `E` entry of
    /// `(source, attr, Ref(target))` at `tick` (and `tx`, for a main-index
    /// read). Builds that `E` key only if the world holds any vague tick.
    fn r_precision(
        &self,
        source: EntityId,
        attr: AttrId,
        target: EntityId,
        tick: Tick,
        tx: TxId,
    ) -> Precision {
        let world = self.world;
        if world.e_precision.is_empty() {
            return Precision::Exact;
        }
        let value = Value::Ref(target)
            .encode()
            .expect("invariant: a ref value always encodes");
        let k = key::e_fact_key(source, attr, &value, tick, tx);
        if self.latest {
            self.precision_at(key::without_tx(&k))
        } else {
            self.precision_at(&k)
        }
    }

    /// The current entry of every `R` key under `R|target|attr|` in this
    /// canon, `DELETED` ones left out, as `(source, tick, op, precision)` in
    /// key order.
    fn r_current(&self, target: EntityId, attr: AttrId) -> Vec<(EntityId, Tick, Op, Precision)> {
        let world = self.world;
        let attr_prefix = key::r_attr_prefix(target, attr);
        if self.latest {
            under(&world.r_latest, &attr_prefix)
                .map(|(k, op)| {
                    let (source, tick) = key::latest_r_source_and_tick(k)
                        .expect("invariant: R_latest keys are R keys without their ^tx");
                    // The latest path ignores the tx.
                    let precision = self.r_precision(source, attr, target, tick, TxId(0));
                    (source, tick, *op, precision)
                })
                .collect()
        } else {
            current_entries(&world.r_index, &attr_prefix, self.canon)
                .map(|(k, op)| {
                    let (_, _, source, tick, tx) =
                        key::decode_r_key(k).expect("invariant: R index keys are fixed-width");
                    let precision = self.r_precision(source, attr, target, tick, tx);
                    (source, tick, op, precision)
                })
                .collect()
        }
    }

    /// The neighbors of `entity` through the `ref` attribute `attr` at tick
    /// `t` in this canon, as `(neighbor, edge)` pairs sorted ascending,
    /// without duplicates.
    ///
    /// [`Direction::Out`]: `entity` is the source; every target `x` with
    /// `(entity, attr, x)` holding at `t`. [`Direction::In`]: `entity` is
    /// the target; every source `x` with `(x, attr, entity)` holding.
    /// [`Direction::Both`]: the union (a pair linked both ways appears once
    /// per edge). A pair holds iff its latest op with tick `<= t` among the
    /// visible transactions (deleted ticks skipped) is an assert, so an event holds from its tick
    /// on. `attr` may be an inverse name (`child_of` for `parent_of`): it
    /// reads the base attribute with `Out` and `In` swapped. For a
    /// bidirectional attribute every direction returns the partners from
    /// either side (each pair is stored once, normalised to `(min, max)`).
    ///
    /// Cost, per direction read, grows with the number of distinct
    /// neighbors the entity has *ever* had through `attr`: one seek each.
    /// Fails with [`Error::UnknownAttribute`] (neither an attribute nor an
    /// inverse name), [`Error::UnknownEntity`] (undefined entity or edge) or
    /// [`Error::NotARef`] (not a `ref` attribute).
    pub fn neighbors(
        &self,
        entity: EntityId,
        attr: &str,
        dir: Direction,
        t: Tick,
    ) -> Result<Vec<(EntityId, EdgeId)>, Error> {
        let world = self.world;
        let (attr_id, dir) = world.resolve_neighbor_attr(attr, dir)?;
        if !world.is_defined(&StagedIds::default(), entity) {
            return Err(Error::UnknownEntity(entity));
        }
        if world.attributes[attr_id.0 as usize].value_type != ValueType::Ref {
            return Err(Error::NotARef(attr.to_owned()));
        }
        // A bidirectional pair is stored once, under its normalised
        // (min, max) order, so every direction reads both sides.
        let (read_out, read_in) = if world.is_bidirectional(attr_id) {
            (true, true)
        } else {
            (dir.reads_out(), dir.reads_in())
        };
        // Every stored ref fact's triple has an edge: a stored assert
        // interns it before (or with) the fact, and a stored retract needs a
        // stored assert. An edge defined after this canon can't show up
        // here, since its facts are newer too, so the latest table serves.
        // `edge_of` finds a bidirectional pair in either order; a directed
        // one is asked with `entity` on the side it was read from.
        let mut out = Vec::new();
        if read_out {
            self.for_each_partner_holding(entity, attr_id, true, false, t, |x| {
                out.push((x, edge_or_panic(world, attr_id, entity, x)));
            });
        }
        if read_in {
            self.for_each_partner_holding(entity, attr_id, false, true, t, |x| {
                out.push((x, edge_or_panic(world, attr_id, x, entity)));
            });
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// The change points of `entity.attr` in this canon: for every key
    /// `(value, tick)`, its current entry (the one written by the newest tx
    /// `<=` the canon) if that is an assert or retract, ordered by tick
    /// ascending, then (same tick) by value encoding ascending. A key whose
    /// current entry is a `DELETED` marker is left out; older canon, before
    /// the correction, still lists it.
    ///
    /// Only effective writes are stored, so no-ops and failed writes never
    /// appear. A change point is where something changed, not every tick
    /// where a value holds. For a bidirectional attribute the changes of
    /// every pair `entity` is in are listed, from either side, with the
    /// partner as value; a self pair's changes are listed once. Cost is one
    /// scan of `E|e|a|` (and, bidirectional, of `R|e|a|`): every entry the
    /// attribute has ever had. Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        let (attr_id, _) = self.world.resolve(&StagedIds::default(), entity, attr)?;
        Ok(self.changes(entity, attr_id))
    }

    /// [`View::history`] of a resolved attribute.
    fn changes(&self, entity: EntityId, attr_id: AttrId) -> Vec<Change> {
        let world = self.world;
        let attr_prefix = key::e_attr_prefix(entity, attr_id);
        // The scan yields value-ascending, then newest tick first; collect
        // the value bytes (borrowed from E keys) and reorder by
        // (tick, value bytes).
        // Value references are resolved to the full encodings they name, so
        // the order below is by value, whatever the references are.
        // A key whose value part can't be resolved or decoded (only by
        // tampering) is skipped; doctor reports it.
        // (value encoding, change): sorted by (tick, encoding) at the end.
        let mut points: Vec<(Cow<'_, [u8]>, Change)> = self
            .e_current(&attr_prefix)
            .into_iter()
            .filter_map(|(tick, part, op, precision)| {
                let (bytes, value) = world.decode_e_value(attr_id, part)?;
                let change = Change {
                    tick,
                    op,
                    value,
                    precision,
                };
                Some((Cow::Borrowed(bytes), change))
            })
            .collect();
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner: their R mirrors.
            // A self pair's R mirror is the E entry already collected.
            for (source, tick, op, precision) in self.r_current(entity, attr_id) {
                if source != entity {
                    let value = Value::Ref(source);
                    let bytes = value
                        .encode()
                        .expect("invariant: a ref value always encodes");
                    let change = Change {
                        tick,
                        op,
                        value,
                        precision,
                    };
                    points.push((Cow::Owned(bytes), change));
                }
            }
        }
        // (tick, value) is unique among the points: only each key's
        // current entry is listed, and a bidirectional pair is stored under
        // one side only (the self pair's R side is skipped).
        points.sort_unstable_by(|a, b| (a.1.tick, &a.0).cmp(&(b.1.tick, &b.0)));
        points.into_iter().map(|(_, change)| change).collect()
    }

    /// The change points of the sources pointing at `target` through the
    /// `ref` attribute `attr` (`R|target|attr|`) in this canon, with
    /// `Ref(source)` as value, ordered like [`View::history`]: by tick, then
    /// source ID (= value encoding).
    fn reverse_changes(&self, target: EntityId, attr: AttrId) -> Vec<Change> {
        let mut points = self.r_current(target, attr);
        points.sort_unstable_by_key(|&(source, tick, ..)| (tick, source));
        points
            .into_iter()
            .map(|(source, tick, op, precision)| Change {
                tick,
                op,
                value: Value::Ref(source),
                precision,
            })
            .collect()
    }

    /// Every cardinality overlap of `attr` in this canon, over the whole
    /// story timeline (see [`crate::overlap`]): two values that hold at the
    /// same time on a side labelled cardinality one.
    ///
    /// For a cardinality-one **state** attribute, the [`Side::Forward`]
    /// overlaps of every entity (and edge) among its values as
    /// [`View::history`] lists them (for a bidirectional attribute, the
    /// partners from either side, so both ends of a pair agree); for a
    /// directed `ref` state attribute with a reverse cardinality of one,
    /// the [`Side::Reverse`] overlaps of every target among its sources.
    /// Cardinality many, no reverse label and event attributes have none.
    /// Touching intervals and gaps are never reported.
    ///
    /// Sorted by `(entity, side, a, b, during.from)`, Forward before
    /// Reverse. Cost: one history read per defined entity and side checked.
    /// Fails with [`Error::UnknownAttribute`].
    pub fn overlaps(&self, attr: &str) -> Result<Vec<Overlap>, Error> {
        let world = self.world;
        let attr_id = *world
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        let mut out = Vec::new();
        if !self.is_checked(attr_id, Side::Forward) && !self.is_checked(attr_id, Side::Reverse) {
            return Ok(out);
        }
        for entity in world.defined_ids() {
            for side in [Side::Forward, Side::Reverse] {
                out.extend(self.overlaps_of(entity, attr_id, side));
            }
        }
        Ok(out)
    }

    /// Whether `side` of `attr` is labelled cardinality one on a state
    /// attribute, i.e. has overlaps to check.
    fn is_checked(&self, attr: AttrId, side: Side) -> bool {
        let def = &self.world.attributes[attr.0 as usize];
        def.kind == Kind::State
            && match side {
                Side::Forward => def.cardinality == Cardinality::One,
                // Only a directed ref attribute can carry this label
                // (checked at world creation and replay).
                Side::Reverse => def.reverse_cardinality == Some(Cardinality::One),
            }
    }

    /// The overlaps of one `(entity, attr, side)` in this canon, sorted by
    /// `(a, b, during.from)`; empty if that side isn't checked (see
    /// [`View::overlaps`]). The per-entity step of [`View::overlaps`], for
    /// callers that only need the entities a write touched.
    pub(crate) fn overlaps_of(&self, entity: EntityId, attr: AttrId, side: Side) -> Vec<Overlap> {
        self.overlaps_of_touching(entity, attr, side, None)
    }

    /// [`View::overlaps_of`] restricted, when `touched` is given, to the
    /// overlaps in which one value is among `touched` (value encodings,
    /// ascending and deduplicated), in the same order.
    fn overlaps_of_touching(
        &self,
        entity: EntityId,
        attr: AttrId,
        side: Side,
        touched: Option<&[Vec<u8>]>,
    ) -> Vec<Overlap> {
        if !self.is_checked(attr, side) {
            return Vec::new();
        }
        let changes = match side {
            Side::Forward => self.changes(entity, attr),
            Side::Reverse => self.reverse_changes(entity, attr),
        };
        if changes.is_empty() {
            return Vec::new();
        }
        let name = &self.world.attributes[attr.0 as usize].name;
        let per_value = value_intervals(&changes);
        let rows = match touched {
            None => find_overlaps(&per_value),
            Some(values) => find_overlaps_touching(&per_value, values),
        };
        rows.into_iter()
            .map(|(a, b, during)| Overlap {
                entity,
                attr: name.clone(),
                side,
                a,
                b,
                during,
            })
            .collect()
    }

    /// The entries visible in this canon that store something at a key
    /// (facts, `Revised` facts and `DELETED` markers), in log order, with
    /// their tx and key.
    fn visible_keyed_entries(&self) -> impl Iterator<Item = (&EntryInfo, FactDef)> + '_ {
        let canon = self.canon;
        let world = self.world;
        world
            .entries
            .iter()
            .take_while(move |e| e.tx <= canon)
            .filter_map(|e| {
                let (entity, attr, value, tick, op) = match &e.body {
                    EntryBody::Deleted {
                        entity,
                        attr,
                        value,
                        tick,
                        ..
                    } => (*entity, *attr, value, *tick, Op::Assert),
                    body => {
                        let (entity, attr, value, tick, op, _) = as_fact(body)?;
                        (entity, attr, value, tick, op)
                    }
                };
                // Kept entries were applied, so their values encode.
                Some((
                    e,
                    world.stored_fact_def(entity, attr, value, tick, op).ok()?,
                ))
            })
    }

    /// Every cardinality overlap in this canon ([`View::overlaps`] of every
    /// attribute, in schema order), each with the newest tx `<=` canon of an
    /// entry touching its `(entity, attr, side)` as [`World::touched_sides`]
    /// judges a write: doctor's `since` tag.
    pub(crate) fn tagged_overlaps(&self) -> Vec<(TxId, Overlap)> {
        let world = self.world;
        let mut touched: HashMap<(EntityId, AttrId, Side), TxId> = HashMap::new();
        for (e, fact) in self.visible_keyed_entries() {
            for side in world.touched_sides(std::iter::once(&fact)) {
                let tx = touched.entry(side).or_insert(e.tx);
                *tx = (*tx).max(e.tx);
            }
        }
        let mut out = Vec::new();
        for (i, def) in world.attributes.iter().enumerate() {
            let attr = AttrId(i as u32);
            let overlaps = self
                .overlaps(&def.name)
                .expect("invariant: every schema attribute is known by its name");
            out.extend(overlaps.into_iter().map(|o| {
                // Every overlap's side has entries, so it was touched.
                let tx = touched
                    .get(&(o.entity, attr, o.side))
                    .copied()
                    .unwrap_or(self.canon);
                (tx, o)
            }));
        }
        out
    }

    /// Every property assert on an edge whose current entry in this canon
    /// is an assert at a tick where the edge's own triple doesn't hold in
    /// this canon, in entry order, as `(tag, entry, edge, attr name, tick)`.
    /// `entry` is the key's current entry (the base fact or its last
    /// `Revised`); `tag` is the newer of its tx and the newest tx `<=` canon
    /// of an entry on the edge's triple: doctor's `since` tag. Property
    /// retracts are never reported: cleaning up at the edge's end is normal.
    pub(crate) fn unasserted_edge_properties(&self) -> Vec<(TxId, EntryId, EdgeId, String, Tick)> {
        // Per key: its current entry (the last one in log order); per
        // history `E|e|a|v`: the newest tx writing to it.
        type Current = (EntryId, TxId, Mark, EdgeId, FactDef);
        let mut current: HashMap<(Vec<u8>, Tick), Current> = HashMap::new();
        let mut history_tx: HashMap<Vec<u8>, TxId> = HashMap::new();
        for (e, fact) in self.visible_keyed_entries() {
            let prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
            // An edge's own triple may have an edge as its source.
            let tx = history_tx.entry(prefix.clone()).or_insert(e.tx);
            *tx = (*tx).max(e.tx);
            let Some(edge) = fact.entity.as_edge() else {
                continue;
            };
            let mark = match e.body {
                EntryBody::Deleted { .. } => Mark::Deleted,
                _ => Mark::from(fact.op),
            };
            current.insert((prefix, fact.tick), (e.id, e.tx, mark, edge, fact));
        }
        let mut out: Vec<_> = current
            .into_values()
            .filter(|(_, _, mark, _, _)| *mark == Mark::Assert)
            .filter_map(|(entry, tx, _, edge, fact)| {
                let i = usize::try_from(edge.seq().checked_sub(1)?).ok()?;
                let &(source, attr, target) = self
                    .world
                    .edges
                    .get(i)
                    .expect("invariant: a kept entry's edge subject is defined");
                let value = Value::Ref(target)
                    .encode()
                    .expect("invariant: a ref value always encodes");
                let mut triple = key::e_value_prefix(source, attr, &value);
                if self.holds_e(&mut triple, fact.tick) {
                    return None;
                }
                let tag = history_tx.get(&triple).map_or(tx, |t| tx.max(*t));
                let name = self.world.attributes[fact.attr.0 as usize].name.clone();
                Some((tag, entry, edge, name, fact.tick))
            })
            .collect();
        out.sort_unstable_by_key(|f| f.1);
        out
    }
}

/// The distinct-history walk behind `AsOf(entity, attr, T)` and
/// `Neighbors`, for any index whose keys are `attr_prefix | x | ^tick`,
/// optionally followed by `| ^tx` (`E|e|a|value|...`, `R|t|a|source|...`,
/// main or latest-revision). Calls `f` with the history prefix
/// `attr_prefix | x` of every `x` for which `holds_at(prefix)` (the seek at
/// the read's tick and canon) is true, in key order.
///
/// `history_end(key)` is the length of a key's history prefix. Each round
/// finds the next distinct history, seeks it, then jumps past all of its
/// keys, so the cost is one seek per distinct `x` ever stored.
fn for_each_holding<V>(
    index: &BTreeMap<Vec<u8>, V>,
    attr_prefix: &[u8],
    history_end: impl Fn(&[u8]) -> usize,
    mut holds_at: impl FnMut(&mut Vec<u8>) -> bool,
    mut f: impl FnMut(&[u8]),
) {
    for_each_history(index, attr_prefix, history_end, |prefix| {
        if holds_at(prefix) {
            f(prefix);
        }
    });
}

/// The walk of [`for_each_holding`] without the seek: calls `f` with the
/// history prefix of every distinct history under `attr_prefix`, in key
/// order (`f` may use it as a seek buffer, restoring it before returning).
fn for_each_history<V>(
    index: &BTreeMap<Vec<u8>, V>,
    attr_prefix: &[u8],
    history_end: impl Fn(&[u8]) -> usize,
    mut f: impl FnMut(&mut Vec<u8>),
) {
    let mut from = Bound::Included(attr_prefix.to_vec());
    let mut prefix = Vec::new();
    while let Some((k, _)) = index
        .range::<[u8], _>((as_slice(&from), Bound::Unbounded))
        .next()
    {
        if !k.starts_with(attr_prefix) {
            break;
        }
        prefix.clear();
        prefix.extend_from_slice(&k[..history_end(k)]);
        from = Bound::Excluded(key::past_value(&prefix));
        f(&mut prefix);
    }
}

/// The tick of index key `k` whose history prefix is `prefix_len` bytes
/// long: the `^tick` right after it (main and latest-revision keys alike).
fn tick_after(k: &[u8], prefix_len: usize) -> Tick {
    let bytes = k
        .get(prefix_len..prefix_len + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .expect("invariant: a history's keys carry a ^tick after its prefix");
    Tick::decode_inverted(bytes)
}

/// Every `(key, op)` of a latest-revision index under `prefix`, in key
/// order.
fn under<'i>(
    index: &'i LatestIndex,
    prefix: &'i [u8],
) -> impl Iterator<Item = (&'i [u8], &'i Op)> + 'i {
    index
        .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
        .take_while(move |(k, _)| k.starts_with(prefix))
        .map(|(k, op)| (&k[..], op))
}

/// Makes `mark` the current entry of latest-revision key `at_key`: a fact
/// is stored (replacing the previous current entry), a `DELETED` marker
/// empties the key.
fn set_latest(index: &mut LatestIndex, at_key: &[u8], mark: Mark) {
    match mark.op() {
        Some(op) => match index.get_mut(at_key) {
            Some(current) => *current = op,
            None => {
                index.insert(at_key.to_vec(), op);
            }
        },
        None => {
            index.remove(at_key);
        }
    }
}

/// One seek in a latest-revision index: whether the first key at or after
/// `value_prefix | ^t` within `value_prefix` (`E|e|a|v|` or `R|t|a|s|`),
/// i.e. the current entry of the latest tick `<= t`, is an assert. Deleted
/// keys aren't stored there and no entry is newer than the latest canon,
/// so there is nothing to skip. Reuses `value_prefix` as the seek key
/// buffer; it is restored before returning.
fn holds_latest(index: &LatestIndex, value_prefix: &mut Vec<u8>, t: Tick) -> bool {
    latest_seek(index, value_prefix, t).map(|(_, op)| op) == Some(Op::Assert)
}

/// The seek of [`holds_latest`]: the key and op of the current entry of the
/// latest tick `<= t` under `value_prefix`, if any.
fn latest_seek<'i>(
    index: &'i LatestIndex,
    value_prefix: &mut Vec<u8>,
    t: Tick,
) -> Option<(&'i [u8], Op)> {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = index
        .range::<[u8], _>((Bound::Included(&value_prefix[..]), Bound::Unbounded))
        .next()
        .filter(|(k, _)| k.starts_with(&value_prefix[..len]))
        .map(|(k, op)| (&k[..], *op));
    value_prefix.truncate(len);
    found
}

/// The current entry (newest tx `<= canon`) of every key under `prefix` in
/// `index`, as `(full key, op)` in key order; keys whose current entry is a
/// `DELETED` marker are left out. One pass over every entry under `prefix`:
/// the entries of one key are adjacent, newest tx first, so the first
/// visible one of each run is that key's current entry.
fn current_entries<'i>(
    index: &'i Index,
    prefix: &'i [u8],
    canon: TxId,
) -> impl Iterator<Item = (&'i [u8], Op)> + 'i {
    let mut last_key: Option<&'i [u8]> = None;
    index
        .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
        .take_while(move |(k, _)| k.starts_with(prefix))
        .filter(move |(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= canon))
        .filter(move |(k, _)| {
            let at_key = key::without_tx(k);
            let first = last_key != Some(at_key);
            last_key = Some(at_key);
            first
        })
        .filter_map(|(k, mark)| Some((&k[..], mark.op()?)))
}

/// The edge of a stored `ref` pair `(source, attr, target)` of `world`.
fn edge_or_panic(world: &World, attr: AttrId, source: EntityId, target: EntityId) -> EdgeId {
    world
        .edge_of(&StagedIds::default(), source, attr, target)
        .expect("invariant: every stored ref fact's triple has an interned edge")
}

/// Decodes a stored `E` key, value included: `(entity, attr, value, tick,
/// tx)`, or `None` if the bytes aren't a well-formed `E` key holding a
/// canonical value encoding. Never panics, whatever the bytes: doctor and
/// [`World::index_dump`] read tampered indexes through it.
pub(crate) fn decode_e_row(k: &[u8]) -> Option<(EntityId, AttrId, Value, Tick, TxId)> {
    let (entity, attr, value, tick, tx) = key::decode_e_key(k).ok()?;
    Some((entity, attr, Value::decode(value).ok()?, tick, tx))
}

/// Decodes a stored `V` key, value included: `(attr, value, entity, tick,
/// tx)`, or `None` if the bytes aren't a well-formed `V` key holding a
/// canonical value encoding. Never panics, whatever the bytes.
pub(crate) fn decode_v_row(k: &[u8]) -> Option<(AttrId, Value, EntityId, Tick, TxId)> {
    let (attr, value, entity, tick, tx) = key::decode_v_key(k).ok()?;
    Some((attr, Value::decode(value).ok()?, entity, tick, tx))
}

/// Decodes a stored `VFold` key, values included: `(attr, folded,
/// original, entity, tick, tx)`, or `None` if the bytes aren't a
/// well-formed `VFold` key holding a canonical string fold and a canonical
/// original value. Never panics, whatever the bytes.
pub(crate) fn decode_vfold_row(k: &[u8]) -> Option<(AttrId, String, Value, EntityId, Tick, TxId)> {
    let (attr, fold, original, entity, tick, tx) = key::decode_vfold_key(k).ok()?;
    let Value::Str(folded) = Value::decode(fold).ok()? else {
        return None;
    };
    Some((
        attr,
        folded,
        Value::decode(original).ok()?,
        entity,
        tick,
        tx,
    ))
}

/// The result of a [`Txn`] write that stored `changed` entries at `tick`
/// (entry IDs and edges are only known at commit).
/// The write warnings from the overlaps of the same checks before and after
/// a commit (lists in the same order, as [`World::overlaps_of_sides`]
/// returns them): `introduced` = after − before, `resolved` = before −
/// after, by exact equality. The order of the checks and of each list is
/// kept, which gives the reported sort order.
///
/// Each list is sorted by `(a, b, during.from)` (value encodings), and that
/// key is unique within a list (one pair's maximal stretches are
/// disjoint), so the two lists are compared in one sort-merge: an overlap
/// at the same key with another `until` is the old one resolved and the
/// new one introduced.
fn overlap_diff(before: Vec<Vec<Overlap>>, after: Vec<Vec<Overlap>>) -> Warnings {
    fn sort_key(o: &Overlap) -> (Option<Vec<u8>>, Option<Vec<u8>>, Tick) {
        (o.a.encode().ok(), o.b.encode().ok(), o.during.from)
    }
    let mut w = Warnings::default();
    for (old, new) in before.into_iter().zip(after) {
        let mut old = old.into_iter().map(|o| (sort_key(&o), o)).peekable();
        let mut new = new.into_iter().map(|o| (sort_key(&o), o)).peekable();
        loop {
            let order = match (old.peek(), new.peek()) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some((ko, _)), Some((kn, _))) => ko.cmp(kn),
            };
            match order {
                Ordering::Less => w.resolved.extend(old.next().map(|(_, o)| o)),
                Ordering::Greater => w.introduced.extend(new.next().map(|(_, o)| o)),
                Ordering::Equal => {
                    let (o, n) = match (old.next(), new.next()) {
                        (Some((_, o)), Some((_, n))) => (o, n),
                        _ => unreachable!("invariant: both lists were peeked non-empty"),
                    };
                    if o != n {
                        w.resolved.push(o);
                        w.introduced.push(n);
                    }
                }
            }
        }
    }
    w
}

/// Refuses a live write of `fact` with [`Error::EntryTooLarge`] if a
/// `Revised` entry carrying its value would exceed
/// [`MAX_ENTRY_BYTES`](log::MAX_ENTRY_BYTES): every value is judged by the
/// largest entry it may ever need, so any accepted value can later be
/// deleted or replaced in place. Checked before anything is staged, so the
/// log never gets a frame a reader refuses. Replay doesn't check: its
/// entries already fit their frames.
fn check_entry_size(attr: &str, fact: &FactDef) -> Result<(), Error> {
    let len = log::revised_payload_len(fact.value.len());
    if len > log::MAX_ENTRY_BYTES {
        return Err(Error::EntryTooLarge {
            attr: attr.to_owned(),
            len,
            max: log::MAX_ENTRY_BYTES,
        });
    }
    Ok(())
}

fn written(changed: usize, tick: Tick) -> WriteResult {
    WriteResult {
        changed,
        tick,
        entries: Vec::new(),
        edges: Vec::new(),
        warnings: Warnings::default(),
    }
}

/// Judges a validated fact against the committed `E` index plus a
/// transaction's `staged` entries, at the fact's own tick (never the latest
/// tick, so out-of-order writes are kept). A key whose current entry is a
/// `DELETED` marker holds nothing.
///
/// First the same-tick rule: if the opposite op is current for the same
/// `(entity, attr, value)` at exactly this tick, the write fails with
/// [`Error::SameTickConflict`] (tick order can't decide between them, and
/// letting tx order decide would break "truth follows tick order"). Then
/// the no-op rule: an assert of a value that already holds there, or a
/// retract of one that doesn't, changes nothing. Last, a write that would be
/// stored at a key the transaction already staged an entry at (only a
/// `DELETED` marker can get there) fails with [`Error::TxnKeyConflict`]: a
/// transaction stores at most one entry per key. `written` (the subject as
/// the caller wrote it, before any bidirectional normalisation) and `attr`
/// (the attribute's name) are for the errors.
fn judge(
    committed: &Index,
    staged: &Index,
    fact: &FactDef,
    written: EntityId,
    attr: &str,
) -> Result<Verdict, Error> {
    let layers = [committed, staged];
    let layers = &layers[..];
    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
    // Writes are judged at the latest canon plus the transaction's own
    // staged facts, so every entry is visible.
    let all = TxId(u64::MAX);
    if let Some(stored) = op_at_tick(layers, &mut prefix, fact.tick, all)
        && stored != fact.op
    {
        return Err(Error::SameTickConflict {
            entity: written,
            attr: attr.to_owned(),
            tick: fact.tick,
        });
    }
    let holds = holds(layers, &mut prefix, fact.tick, all);
    let noop = match fact.op {
        Op::Assert => holds,
        Op::Retract => !holds,
    };
    if noop {
        return Ok(Verdict::Noop);
    }
    if has_key(staged, fact) {
        return Err(Error::TxnKeyConflict {
            entity: fact.entity,
            attr: attr.to_owned(),
            tick: fact.tick,
        });
    }
    Ok(Verdict::Store)
}

/// Whether the `E`-layout `index` holds any entry (any tx) at `fact`'s key
/// `E|e|a|v|^tick`.
fn has_key(index: &Index, fact: &FactDef) -> bool {
    let mut at_key = key::e_value_prefix(fact.entity, fact.attr, fact.key_value());
    key::push_seek_tick(&mut at_key, fact.tick);
    index
        .range::<[u8], _>((Bound::Included(&at_key[..]), Bound::Unbounded))
        .next()
        .is_some_and(|(k, _)| k.starts_with(&at_key))
}

/// One seek: the op of the current entry (newest tx `<= canon`) under
/// `value_prefix` (`E|e|a|v|`) at exactly tick `t`, if any; `None` if there
/// is none or it is a `DELETED` marker. Reuses `value_prefix` as the seek
/// key buffer; it is restored before returning.
fn op_at_tick(layers: &[&Index], value_prefix: &mut Vec<u8>, t: Tick, canon: TxId) -> Option<Op> {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    // The prefix to match includes `^t`: only entries at exactly `t`.
    let found = seek(layers, value_prefix, value_prefix, canon);
    value_prefix.truncate(len);
    found
}

/// One seek: whether the current entry (newest tx `<= canon`) of the latest
/// tick `<= t` under `value_prefix` (`E|e|a|v|`) is an assert, ticks whose
/// current entry is a `DELETED` marker skipped. Reuses `value_prefix` as
/// the seek key buffer; it is restored before returning.
fn holds(layers: &[&Index], value_prefix: &mut Vec<u8>, t: Tick, canon: TxId) -> bool {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = seek(layers, value_prefix, &value_prefix[..len], canon);
    value_prefix.truncate(len);
    found == Some(Op::Assert)
}

/// The op of the current entry of the first tick at or after `from` under
/// `prefix`, reading all `layers` as if they were one index and only
/// entries written by a tx `<= canon`; ticks whose current entry is a
/// `DELETED` marker are skipped whole.
///
/// Within one `E|e|a|v|` (or `R|t|a|s|`) history the smallest key is the
/// latest entry (newest tick first, then newest tx), i.e. the current entry
/// of the newest tick. That holds across layers because a transaction's
/// staged keys carry its own tx, newer than any committed one. Keys order
/// by `^tick` before `^tx`, so a newer-tx entry can sit before the answer at
/// any tick (a later prequel or retract); those are skipped one by one. A
/// current `DELETED` marker empties its whole tick: older-tx entries at the
/// same tick are not current, so the walk jumps past the tick instead of
/// falling through to them. At the latest canon with no corrections nothing
/// is skipped: still one seek.
fn seek(layers: &[&Index], from: &[u8], prefix: &[u8], canon: TxId) -> Option<Op> {
    seek_entry(layers, from, prefix, canon).map(|(_, op)| op)
}

/// [`seek`], also returning the key of the entry found.
fn seek_entry<'i>(
    layers: &[&'i Index],
    from: &[u8],
    prefix: &[u8],
    canon: TxId,
) -> Option<(&'i [u8], Op)> {
    // Set once a deleted tick was skipped: seek exclusively past it.
    let mut past: Option<Vec<u8>> = None;
    loop {
        let lower = match &past {
            None => Bound::Included(from),
            Some(p) => Bound::Excluded(&p[..]),
        };
        let (k, mark) = layers
            .iter()
            .filter_map(|index| {
                index
                    .range::<[u8], _>((lower, Bound::Unbounded))
                    .take_while(|(k, _)| k.starts_with(prefix))
                    .find(|(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= canon))
            })
            .min_by(|(a, _), (b, _)| a.cmp(b))?;
        match mark.op() {
            Some(op) => return Some((&k[..], op)),
            None => past = Some(key::past_tick(k)),
        }
    }
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
        assert_eq!(w.e_latest, rebuilt(&w.e_index, w.latest_tx));
        assert_eq!(w.r_latest, rebuilt(&w.r_index, w.latest_tx));
        // the deleted -250 key and the deleted pair are absent, the main
        // indexes still hold their entries
        assert_eq!(w.e_latest.len(), 2);
        assert_eq!(w.r_latest.len(), 2);
        assert!(w.e_index.len() > w.e_latest.len());
    }

    #[test]
    fn world_reads_go_through_e_latest_and_main_view_does_not() {
        let (mut w, greg, _, varrow) = world();
        let v = Value::Ref(varrow);
        w.assert(greg, "located_in", v.clone(), Some(Tick(-300)))
            .unwrap();
        // Tamper with E_latest only: the latest path now disagrees with the
        // main indexes, which proves which one each route reads.
        for op in w.e_latest.values_mut() {
            *op = Op::Retract;
        }
        let at = Tick(0);
        assert!(!w.as_of_value(greg, "located_in", &v, at).unwrap());
        assert!(w.as_of(greg, "located_in", at).unwrap().is_empty());
        assert_eq!(w.history(greg, "located_in").unwrap()[0].op, Op::Retract);
        assert!(
            !w.at_canon(w.latest_tx)
                .as_of_value(greg, "located_in", &v, at)
                .unwrap()
        );
        let main = w.at_canon_main(w.latest_tx);
        assert!(main.as_of_value(greg, "located_in", &v, at).unwrap());
        assert_eq!(main.as_of(greg, "located_in", at).unwrap(), vec![v.clone()]);
        assert_eq!(main.history(greg, "located_in").unwrap()[0].op, Op::Assert);
    }

    #[test]
    fn world_neighbors_in_go_through_r_latest() {
        let (mut w, greg, _, varrow) = world();
        w.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(1)))
            .unwrap();
        w.r_latest.clear();
        let at = Tick(1);
        assert!(
            w.neighbors(varrow, "located_in", Direction::In, at)
                .unwrap()
                .is_empty()
        );
        let main = w.at_canon_main(w.latest_tx);
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
        let first = w.latest_tx;
        w.assert(greg, "located_in", Value::Ref(greg), Some(Tick(2)))
            .unwrap();
        w.e_latest.clear();
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
        let mut main = w.e_precision.clone();
        for p in w.e_precision.values_mut() {
            *p = Precision::Day;
        }
        assert!(mismatch_in(&w, IndexName::E));
        w.e_precision.clear();
        assert!(mismatch_in(&w, IndexName::E));
        std::mem::swap(&mut w.e_precision, &mut main);
        for p in w.e_latest_precision.values_mut() {
            *p = Precision::Century;
        }
        assert!(mismatch_in(&w, IndexName::ELatest));
        assert!(!mismatch_in(&w, IndexName::E));
    }
}
