//! A Kala world: one story timeline, its schema and its entities.
//!
//! Facts live in the in-memory `E` index (layout in the private `key`
//! module). A world is either in memory only ([`World::in_memory`]) or backed
//! by a log file ([`World::create`], format in [`crate::log`]), which is then
//! the source of truth: a committed [`Txn`] is appended as its `DEFINE`
//! entries, its facts and a `COMMIT`, and fsynced, before the in-memory
//! tables change. Every operation first validates into a definition
//! ([`EntityDef`], [`FactDef`], [`EdgeDef`]) without touching the world, then
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use crate::doctor::{IndexName, Tamper};
use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, IdError, MAX_NODE_TYPE, NodeTypeId};
use crate::key;
use crate::log::{
    self, EncodedTxn, EntryBody, EntryId, LogEntry, LogFault, LogWriter, NewEntry, SurveyedTxn,
};
use crate::overlap::{Overlap, Side, Warnings, find_overlaps, value_intervals};
use crate::schema::{AttrDef, Cardinality, Kind, RefDirection, Schema};
use crate::tick::Tick;
use crate::value::{Value, ValueType};

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
/// [`View::history`]: at story tick `tick`, `value` was asserted or
/// retracted (`op`).
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    /// Story tick of the change.
    pub tick: Tick,
    /// Assert or retract.
    pub op: Op,
    /// The value asserted or retracted.
    pub value: Value,
}

/// A validated fact, ready to apply. Later this is the payload of the log's
/// fact entry and the source of its index keys.
#[derive(Debug)]
pub(crate) struct FactDef {
    pub(crate) entity: EntityId,
    pub(crate) attr: AttrId,
    /// Order-preserving encoding of the value (index key bytes).
    pub(crate) value: Vec<u8>,
    pub(crate) tick: Tick,
    pub(crate) op: Op,
    /// The target of a `ref` fact (its `R` key), `None` for other values.
    pub(crate) target: Option<EntityId>,
}

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
    /// Sequence number of the last entity created with this type (0 = none).
    last_seq: u64,
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
/// nothing cut, no quarantine file, no cause.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Bytes cut off the end of the log (0 if the log was clean).
    pub bytes_cut: u64,
    /// The quarantine file holding exactly the cut bytes, if any were cut.
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
}

/// A world: schema, entities and (later) their facts over story time.
#[derive(Debug)]
pub struct World {
    /// Indexed by `NodeTypeId.0`.
    node_types: Vec<NodeType>,
    node_type_by_name: HashMap<String, NodeTypeId>,
    entity_by_handle: HashMap<String, EntityId>,
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
    /// A name too long for a log entry (over 4 GiB) fails with
    /// [`Error::Io`] of kind `InvalidInput`, as it would for a file.
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
                last_seq: 0,
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
            attributes: defs.to_vec(),
            attr_by_name,
            inverse_by_name,
            edges: Vec::new(),
            edge_by_triple: HashMap::new(),
            latest_tx: TxId(0),
            e_index: BTreeMap::new(),
            r_index: BTreeMap::new(),
            e_latest: BTreeMap::new(),
            r_latest: BTreeMap::new(),
            tick_counts: BTreeMap::new(),
            chains: HashMap::new(),
            entries: Vec::new(),
            log: None,
            recovery: RecoveryReport::default(),
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
        world.recovery = recovery;
        Ok(world)
    }

    /// Rebuilds a world from the entries of a log's committed transactions.
    fn replay(entries: &[LogEntry]) -> Result<World, Error> {
        // Split off the genesis transaction: everything up to the first
        // Commit. `read_log`'s scan guarantees the first Commit is tx#0.
        let genesis_end = entries
            .iter()
            .position(|e| matches!(e.body, EntryBody::Commit { .. }))
            .ok_or_else(|| Error::Corrupt("no committed genesis transaction".to_owned()))?;
        let mut world = World::from_genesis(&entries[..=genesis_end])?;
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
        // would re-encode them: `entry` reports what the log says.
        world.entries.clear();
        world.record(genesis.iter().cloned(), TxId(0));
        Ok(world)
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

    /// The four stored indexes, for doctor to compare against a rebuild.
    pub(crate) fn indexes(&self) -> StoredIndexes<'_> {
        StoredIndexes {
            e: &self.e_index,
            r: &self.r_index,
            e_latest: &self.e_latest,
            r_latest: &self.r_latest,
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
                self.apply_entity(def);
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
                self.apply_edge(EdgeDef {
                    id: *id,
                    source: *source,
                    attr: *attr,
                    target: *target,
                });
                Ok(())
            }
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
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
                let fact = self
                    .prepare_fact(&StagedIds::default(), *entity, &def.name, value, *tick, *op)
                    .map_err(ReplayFault::from_write)?;
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
                let mut prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
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
                let fact = fact_def(*entity, *attr, value, *tick, Op::Assert)
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
                let fact = self
                    .prepare_fact(&StagedIds::default(), *entity, &def.name, value, *tick, *op)
                    .map_err(ReplayFault::from_write)?;
                // A live correction interns the edge of a revised assert's
                // triple before it, as for a fact; a revised retract may be
                // the triple's only entry (no edge).
                if let (Some(target), Op::Assert) = (fact.target, fact.op) {
                    if self
                        .edge_of(&StagedIds::default(), fact.entity, fact.attr, target)
                        .is_none()
                    {
                        return Err(corrupt(
                            &"revised ref assert with no edge defined before it",
                        ));
                    }
                    txn.unasserted_edges
                        .remove(&(fact.entity, fact.attr, target));
                }
                let chain = self.replay_chain(*of).map_err(|why| bad_correction(&why))?;
                if prev_deleted == Some((*of, *rev)) {
                    // A moved key, right after the DELETED marker of the old
                    // one (which already took this rev). The same-tick rule
                    // at the new key: a live writer errs on the opposite op
                    // and writes no Revised for the same one, so any current
                    // entry there means no live writer wrote this.
                    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
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
                    // key, its own next rev, another op.
                    let current = chain.current.ok_or_else(|| {
                        bad_correction(&format!(
                            "a revision of the deleted chain of entry#{} not right after its \
                             DELETED marker",
                            of.0
                        ))
                    })?;
                    check_next_rev(chain, *of, *rev)?;
                    let Some(c_op) = self.op_if_at_key(current, &fact) else {
                        return Err(bad_correction(&format!(
                            "a revision of entry#{} at another key not right after its \
                             DELETED marker",
                            of.0
                        )));
                    };
                    if fact.op == c_op {
                        return Err(corrupt(&format!(
                            "a revision of entry#{} that changes nothing",
                            of.0
                        )));
                    }
                }
                txn.claim_key(&fact).map_err(|why| corrupt(&why))?;
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
    /// mirror. Commit and replay both write the indexes only through here,
    /// so the two always hold the same entries, and the latest-revision
    /// indexes and the latest tick follow the keys' current entries.
    fn index_entry(&mut self, fact: &FactDef, mark: Mark, tx: TxId) {
        let k = key::e_fact_key(fact.entity, fact.attr, &fact.value, fact.tick, tx);
        // `tx` is newer than every stored entry, so this entry becomes the
        // key's current one at the latest canon: `E_latest` holds the
        // previous current entry, if it was a fact.
        let at_key = key::without_tx(&k);
        let was_fact = self.e_latest.contains_key(at_key);
        set_latest(&mut self.e_latest, at_key, mark);
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
                let (entity, attr, value, tick, tx) = decode_e_row(k)?;
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
        IndexDump { e, r }
    }

    /// Damages one stored index in memory, for doctor's tests: the
    /// indexes are copies of the log, and this is how a copy goes wrong
    /// without the log changing. Only that index changes (nothing else is
    /// kept in step), and nothing is logged. A row number past the end does
    /// nothing. Not part of the supported API.
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
            },
            Tamper::RemoveRow { index, nth } => match index {
                IndexName::E => remove_nth(&mut self.e_index, nth),
                IndexName::R => remove_nth(&mut self.r_index, nth),
                IndexName::ELatest => remove_nth(&mut self.e_latest, nth),
                IndexName::RLatest => remove_nth(&mut self.r_latest, nth),
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
            },
        }
    }

    /// Makes the next log append fail as `fault` says, to test failure
    /// handling. An in-memory world has no log and ignores it. Not part of
    /// the supported API.
    #[doc(hidden)]
    pub fn inject_log_fault(&mut self, fault: LogFault) {
        if let Some(log) = self.log.as_mut() {
            log.inject_fault(fault);
        }
    }

    /// `Err` if the world refuses writes: its log append failed earlier.
    fn writable(&self) -> Result<(), Error> {
        self.log.as_ref().map_or(Ok(()), LogWriter::check)
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
    /// 256 bytes), or the opposite op (a retract) is already stored for
    /// this value at exactly this tick ([`Error::SameTickConflict`]; a
    /// repeated assert at that tick is a no-op instead).
    pub fn assert(
        &mut self,
        entity: EntityId,
        attr: &str,
        v: Value,
        tick: Option<Tick>,
    ) -> Result<WriteResult, Error> {
        self.write(entity, attr, &v, tick, Op::Assert)
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
        self.write(entity, attr, &v, tick, Op::Retract)
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
    /// tick. Same key, only the op changed: one `Revised` entry (`changed:
    /// 1`). A moved key (tick or value): a `DELETED` marker at the old key,
    /// then the `Revised` at the new one, sharing one revision (`changed:
    /// 2`); if the same op is already current at the new key only the marker
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
    /// a new retract of an event), or [`Error::SameTickConflict`] if the
    /// opposite op is current at a moved fact's new key.
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
        let (entity, attr, value, tick, op) = self
            .kept_entry(current)
            .and_then(|info| as_fact(&info.body))
            .expect("invariant: a chain's current entry is a committed Fact or Revised entry");
        Ok(Target {
            base,
            chain,
            // A committed fact's value was encoded once before; it encodes.
            fact: fact_def(entity, attr, value, tick, op)?,
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

    /// Replay: the op of the committed fact entry `current` (a chain's
    /// current fact) if `fact` is at its key, else `None`. Values compare
    /// by encoding: exact bytes.
    fn op_if_at_key(&self, current: EntryId, fact: &FactDef) -> Option<Op> {
        let (entity, attr, value, tick, op) = self
            .kept_entry(current)
            .and_then(|info| as_fact(&info.body))
            .expect("invariant: a chain's current entry is a committed Fact or Revised entry");
        // Committed values were encoded once before, so they encode.
        let same = (entity, attr, tick) == (fact.entity, fact.attr, fact.tick)
            && value.encode().is_ok_and(|v| v == fact.value);
        same.then_some(op)
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
    ) -> Result<WriteResult, Error> {
        let mut txn = self.begin();
        let mut res = txn.write(entity, attr, v, tick, op)?;
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
        Ok(FactDef {
            entity,
            attr: attr_id,
            value,
            tick,
            op,
            target,
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

    /// Every value of `entity.attr` that holds at tick `t` (latest canon).
    /// Same as [`View::as_of`] on `at_canon(latest_tx())`.
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        self.at_canon(self.latest_tx).as_of(entity, attr, t)
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
            (1..=nt.last_seq).map(move |seq| {
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
            touched.push((f.entity, f.attr, Side::Forward));
            if let Some(target) = f.target {
                touched.push((target, f.attr, Side::Reverse));
                if self.is_bidirectional(f.attr) {
                    touched.push((target, f.attr, Side::Forward));
                }
            }
        }
        let rank = |s: Side| match s {
            Side::Forward => 0u8,
            Side::Reverse => 1,
        };
        touched.sort_by(|x, y| {
            let name = |a: AttrId| self.attributes[a.0 as usize].name.as_str();
            (x.0.raw(), name(x.1), rank(x.2)).cmp(&(y.0.raw(), name(y.1), rank(y.2)))
        });
        touched.dedup();
        touched
    }

    /// The overlaps at the latest canon of each `(entity, attr, side)` in
    /// `sides`, one list per entry (see [`View::overlaps_of`]).
    fn overlaps_of_sides(&self, sides: &[(EntityId, AttrId, Side)]) -> Vec<Vec<Overlap>> {
        let view = self.at_canon(self.latest_tx);
        sides
            .iter()
            .map(|&(entity, attr, side)| view.overlaps_of(entity, attr, side))
            .collect()
    }

    fn apply_edge(&mut self, def: EdgeDef) {
        debug_assert_eq!(
            def.id.seq(),
            self.edges.len() as u64 + 1,
            "invariant: edges are applied in sequence order"
        );
        self.edge_by_triple.insert(def.triple(), def.id);
        self.edges.push(def.triple());
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
        let committed = self.node_types.get(usize::from(t.0))?.last_seq;
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

    /// Applies a definition produced by [`World::prepare_entity`] (later:
    /// its `DEFINE` entry), as part of a commit. Infallible.
    fn apply_entity(&mut self, def: EntityDef) {
        self.node_types[usize::from(def.id.node_type().0)].last_seq = def.id.seq();
        if let Some(h) = def.handle {
            self.entity_by_handle.insert(h, def.id);
        }
    }

    /// The entity with this handle, if any. Handles are matched exactly.
    pub fn entity_by_handle(&self, handle: &str) -> Option<EntityId> {
        self.entity_by_handle.get(handle).copied()
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
        let mut k = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
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

/// Borrowed views of a world's four stored indexes (see [`World::indexes`]).
pub(crate) struct StoredIndexes<'w> {
    pub(crate) e: &'w Index,
    pub(crate) r: &'w Index,
    pub(crate) e_latest: &'w LatestIndex,
    pub(crate) r_latest: &'w LatestIndex,
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

/// The fact a `Fact` or `Revised` entry says: `(entity, attr, value, tick,
/// op)`; `None` for any other entry.
fn as_fact(body: &EntryBody) -> Option<(EntityId, AttrId, &Value, Tick, Op)> {
    match body {
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
        }
        | EntryBody::Revised {
            entity,
            attr,
            value,
            tick,
            op,
            ..
        } => Some((*entity, *attr, value, *tick, *op)),
        _ => None,
    }
}

/// A stored fact's [`FactDef`] (its key and op), from its decoded fields.
fn fact_def(
    entity: EntityId,
    attr: AttrId,
    value: &Value,
    tick: Tick,
    op: Op,
) -> Result<FactDef, Error> {
    Ok(FactDef {
        entity,
        attr,
        value: value.encode()?,
        tick,
        op,
        target: match value {
            Value::Ref(target) => Some(*target),
            _ => None,
        },
    })
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
        self.write(entity, attr, &v, tick, Op::Assert)
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
        self.write(entity, attr, &v, tick, Op::Retract)
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
    ) -> Result<WriteResult, Error> {
        self.guard(|txn| {
            let tick = tick.or(txn.default_tick).unwrap_or(txn.base_tick);
            let fact = txn
                .world
                .prepare_fact(&txn.ids, entity, attr, v, tick, op)?;
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
            let rev = || chain.rev.checked_add(1).ok_or(Error::TooManyRevisions(of));
            let (value, tick, op) = match c {
                Correction::Delete => {
                    let tick = current.tick;
                    txn.stage(Staged::Deleted {
                        of,
                        rev: rev()?,
                        fact: current,
                    });
                    return Ok(written(1, tick));
                }
                Correction::Replace { value, tick, op } => (value, tick, op),
            };
            // The new fact is the current one with the given fields
            // replaced, validated like a write. The subject is the current
            // fact's (stored) entity; a bidirectional pair is renormalised,
            // which may swap it with the value.
            let new = txn.world.prepare_fact(
                &txn.ids,
                current.entity,
                &attr,
                value.as_ref().unwrap_or(&current_value),
                tick.unwrap_or(current.tick),
                op.unwrap_or(current.op),
            )?;
            let tick = new.tick;
            let same_key = (new.entity, &new.value, new.tick)
                == (current.entity, &current.value, current.tick);
            if same_key {
                if new.op == current.op {
                    // Nothing changed: find-then-act, nothing staged.
                    return Ok(written(0, tick));
                }
                // The chain's own entry is the current one at its key, so
                // changing the op there can't conflict with anything else.
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
            let mut prefix = key::e_value_prefix(new.entity, new.attr, &new.value);
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
        let k = key::e_fact_key(fact.entity, fact.attr, &fact.value, fact.tick, self.tx);
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
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let world = self.world;
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
        let touched = world.touched_sides(self.entries.iter().map(|e| e.key_and_mark().0));
        let before = world.overlaps_of_sides(&touched);
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
            },
            Staged::Fact(f) => NewEntry::Fact {
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
                op: f.op,
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
            world.apply_entity(def);
        }
        let edges = self.ids.edges.iter().map(|d| d.id).collect();
        for def in self.ids.edges {
            world.apply_edge(def);
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
        let after = world.overlaps_of_sides(&touched);
        let warnings = overlap_diff(before, after);
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

impl View<'_> {
    /// The canon tx this view reads at.
    pub fn canon(&self) -> TxId {
        self.canon
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
            _ => key::e_value_prefix(entity, attr_id, &v.encode()?),
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
            out.push(decode_e_value(value));
        });
        Ok(out)
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
            self.for_each_e_value_holding(entity, attr, t, |value| match decode_e_value(value) {
                Value::Ref(target) => f(target),
                other => panic!("invariant: a ref attribute stores only refs, got {other:?}"),
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
        let prefix = key::e_attr_prefix(entity, attr);
        let f = |p: &[u8]| f(&p[key::E_ATTR_PREFIX_LEN..]);
        let world = self.world;
        // Keys in both E indexes are only ever built by `key::e_fact_key`
        // (the latest one without its `^tx`) from a successful
        // `Value::encode`, so they always parse.
        if self.latest {
            let index = &world.e_latest;
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx")
                        .0
                        .len()
            };
            let holds_at = |p: &mut Vec<u8>| holds_latest(index, p, t);
            for_each_holding(index, &prefix, value_end, holds_at, f);
        } else {
            let index = &world.e_index;
            let value_end = |k: &[u8]| {
                key::E_ATTR_PREFIX_LEN
                    + key::e_value_bytes(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key")
                        .len()
            };
            let holds_at = |p: &mut Vec<u8>| holds(&[index], p, t, self.canon);
            for_each_holding(index, &prefix, value_end, holds_at, f);
        }
    }

    /// The current entry of every `E` key under `attr_prefix` (`E|e|a|`) in
    /// this canon, `DELETED` ones left out, as `(tick, value encoding, op)`
    /// in key order.
    fn e_current<'v>(&'v self, attr_prefix: &'v [u8]) -> Vec<(Tick, &'v [u8], Op)> {
        let world = self.world;
        if self.latest {
            under(&world.e_latest, attr_prefix)
                .map(|(k, op)| {
                    let (value, tick) = key::latest_e_value_and_tick(k)
                        .expect("invariant: E_latest keys are E keys without their ^tx");
                    (tick, value, *op)
                })
                .collect()
        } else {
            current_entries(&world.e_index, attr_prefix, self.canon)
                .map(|(k, op)| {
                    let (value, tick) = key::e_value_and_tick(k)
                        .expect("invariant: E index keys are built only by key::e_fact_key");
                    (tick, value, op)
                })
                .collect()
        }
    }

    /// The current entry of every `R` key under `attr_prefix` (`R|t|a|`) in
    /// this canon, `DELETED` ones left out, as `(source, tick, op)` in key
    /// order.
    fn r_current(&self, attr_prefix: &[u8]) -> Vec<(EntityId, Tick, Op)> {
        let world = self.world;
        if self.latest {
            under(&world.r_latest, attr_prefix)
                .map(|(k, op)| {
                    let (source, tick) = key::latest_r_source_and_tick(k)
                        .expect("invariant: R_latest keys are R keys without their ^tx");
                    (source, tick, *op)
                })
                .collect()
        } else {
            current_entries(&world.r_index, attr_prefix, self.canon)
                .map(|(k, op)| {
                    let (_, _, source, tick, _) =
                        key::decode_r_key(k).expect("invariant: R index keys are fixed-width");
                    (source, tick, op)
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
        let mut points: Vec<(Tick, Cow<'_, [u8]>, Op)> = self
            .e_current(&attr_prefix)
            .into_iter()
            .map(|(tick, value, op)| (tick, Cow::Borrowed(value), op))
            .collect();
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner: their R mirrors.
            // A self pair's R mirror is the E entry already collected.
            let r_prefix = key::r_attr_prefix(entity, attr_id);
            for (source, tick, op) in self.r_current(&r_prefix) {
                if source != entity {
                    let value = Value::Ref(source)
                        .encode()
                        .expect("invariant: a ref value always encodes");
                    points.push((tick, Cow::Owned(value), op));
                }
            }
        }
        // (tick, value) is unique among the points: only each key's
        // current entry is listed, and a bidirectional pair is stored under
        // one side only (the self pair's R side is skipped).
        points.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        points
            .into_iter()
            .map(|(tick, value, op)| Change {
                tick,
                op,
                value: Value::decode(&value).expect(
                    "invariant: E index keys are built only by key::e_fact_key \
                     from a successful Value::encode",
                ),
            })
            .collect()
    }

    /// The change points of the sources pointing at `target` through the
    /// `ref` attribute `attr` (`R|target|attr|`) in this canon, with
    /// `Ref(source)` as value, ordered like [`View::history`]: by tick, then
    /// source ID (= value encoding).
    fn reverse_changes(&self, target: EntityId, attr: AttrId) -> Vec<Change> {
        let mut points = self.r_current(&key::r_attr_prefix(target, attr));
        points.sort_unstable_by_key(|&(source, tick, _)| (tick, source));
        points
            .into_iter()
            .map(|(source, tick, op)| Change {
                tick,
                op,
                value: Value::Ref(source),
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
        find_overlaps(&value_intervals(&changes))
            .into_iter()
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
        self.world
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
                    body => as_fact(body)?,
                };
                // Kept entries were applied, so their values encode.
                Some((e, fact_def(entity, attr, value, tick, op).ok()?))
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
        if holds_at(&mut prefix) {
            f(&prefix);
        }
    }
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
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = index
        .range::<[u8], _>((Bound::Included(&value_prefix[..]), Bound::Unbounded))
        .next()
        .filter(|(k, _)| k.starts_with(&value_prefix[..len]))
        .map(|(_, op)| *op);
    value_prefix.truncate(len);
    found == Some(Op::Assert)
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

/// Decodes the value bytes of a stored `E` key.
fn decode_e_value(value: &[u8]) -> Value {
    Value::decode(value).expect(
        "invariant: E index keys are built only by key::e_fact_key from a successful Value::encode",
    )
}

/// The result of a [`Txn`] write that stored `changed` entries at `tick`
/// (entry IDs and edges are only known at commit).
/// The write warnings from the overlaps of the same checks before and after
/// a commit (lists in the same order, as [`World::overlaps_of_sides`]
/// returns them): `introduced` = after − before, `resolved` = before −
/// after, by exact equality. Each check's list is short (pairs of one
/// entity's values), so the per-check comparison is quadratic in that, not
/// in the world; the order of the checks and of each list is kept, which
/// gives the reported sort order.
fn overlap_diff(before: Vec<Vec<Overlap>>, after: Vec<Vec<Overlap>>) -> Warnings {
    let mut w = Warnings::default();
    for (old, new) in before.iter().zip(&after) {
        w.introduced
            .extend(new.iter().filter(|o| !old.contains(o)).cloned());
        w.resolved
            .extend(old.iter().filter(|o| !new.contains(o)).cloned());
    }
    w
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
    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
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
    let mut at_key = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
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
            Some(op) => return Some(op),
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
}
