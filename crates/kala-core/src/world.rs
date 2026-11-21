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
//! Every write goes through a [`Txn`]: [`World::create_entity`],
//! [`World::assert`] and [`World::retract`] are one-write transactions that
//! commit at once. Every read goes through a [`View`] pinned to a canon tx;
//! the world's own reads are the view at its latest tx.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::id::{AttrId, EdgeId, EntityId, IdError, MAX_NODE_TYPE, NodeTypeId};
use crate::key;
use crate::log::{EntryBody, LogEntry, LogWriter, NewEntry};
use crate::schema::{AttrDef, Kind, RefDirection, Schema};
use crate::tick::Tick;
use crate::value::{Value, ValueType};

/// A position on the author timeline: the number of the transaction that
/// wrote a fact. Each committed transaction advances it by one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxId(pub u64);

/// What a write did: how many facts it stored (0 or 1) and the tick it used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteResult {
    /// Facts stored by this write (0 for a no-op).
    pub changed: usize,
    /// The tick the write landed on (its own, or the defaulted one).
    pub tick: Tick,
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

/// An `E`-layout index: full key -> op. Used for the committed index and for
/// a transaction's staged facts, so one seek routine serves both.
type Index = BTreeMap<Vec<u8>, Op>;

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

/// What [`World::open`] did to recover the log: how many bytes of a torn
/// tail it cut off and where it saved them. A clean open, a new world
/// ([`World::create`]) and an in-memory world report the default: nothing
/// cut, no quarantine file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Bytes cut off the end of the log (0 if the log was clean).
    pub bytes_cut: u64,
    /// The quarantine file holding exactly the cut bytes, if any were cut.
    pub quarantine: Option<PathBuf>,
}

/// A decoded copy of the stored indexes, from [`World::index_dump`].
/// Rows are in key order.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDump {
    /// `E` rows: `(entity, attr, value, tick, tx, op)`.
    pub e: Vec<(EntityId, AttrId, Value, Tick, TxId, Op)>,
    /// `R` rows: `(target, attr, source, tick, tx, op)`.
    pub r: Vec<(EntityId, AttrId, EntityId, Tick, TxId, Op)>,
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
    /// Forward index `E|entity|attr|value|^tick|^tx -> op` (layout in
    /// [`crate::key`]). With `r_index`, the in-memory copy of the facts
    /// (milestone 0); both are rebuilt from the log on open.
    e_index: Index,
    /// Reverse index `R|target|attr|source|^tick|^tx -> op`: one entry per
    /// stored `ref` fact, written by [`World::apply_fact`] together with its
    /// `E` entry. Read by [`View::neighbors`] for [`Direction::In`].
    r_index: Index,
    /// Highest tick of any stored fact (assert or retract), `None` if no
    /// fact is stored. Derived from the facts; maintained on commit (later
    /// rebuilt on log replay, like the indexes).
    max_tick: Option<Tick>,
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
    pub fn in_memory(schema: Schema) -> Result<World, Error> {
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
        Ok(World {
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
            max_tick: None,
            log: None,
            recovery: RecoveryReport::default(),
        })
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
    /// fails with [`Error::Io`] too.
    pub fn create(path: &Path, schema: Schema) -> Result<World, Error> {
        let mut world = World::in_memory(schema)?;
        let node_types = world
            .node_types
            .iter()
            .enumerate()
            .map(|(i, nt)| NewEntry::NodeType {
                // In range: `in_memory` refused more than MAX_NODE_TYPE + 1 types.
                id: NodeTypeId(i as u16),
                name: &nt.name,
            });
        let attrs = world
            .attributes
            .iter()
            .enumerate()
            .map(|(i, def)| NewEntry::Attr {
                // In range: `in_memory` refused more than u32::MAX + 1 attributes.
                id: AttrId(i as u32),
                def,
            });
        let log = LogWriter::create(path, node_types.chain(attrs))?;
        world.log = Some(log);
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
    /// (an ID used before its `DEFINE`, a value of the wrong type, ...).
    ///
    /// The file is locked exclusively (an advisory lock, held for the
    /// world's lifetime) before it is read. If another world holds the lock
    /// this fails with [`Error::WorldLocked`] and the file is untouched: no
    /// cut, no quarantine.
    ///
    /// A torn tail (a last transaction cut short by a crash mid-append, or a
    /// zero-filled tail; exact rules in [`crate::log`]) is cut off the file
    /// and fsynced once the rest has replayed cleanly; the world then
    /// continues entry IDs and tx from the last committed transaction.
    /// Before the cut, the cut bytes are saved and fsynced to
    /// `<path>.torn-<valid_len>` (with `-1`, `-2`, ... added if that exists);
    /// if saving them fails, this fails with [`Error::Io`] and the log is not
    /// cut. [`World::recovery`] reports what was cut and where it was saved.
    /// Any other damage, in any transaction (the last one included), is
    /// [`Error::Corrupt`] instead, with the file untouched.
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
        let mut world = World::in_memory(genesis_schema(&entries[..genesis_end])?)
            .map_err(|e| Error::Corrupt(format!("genesis schema: {e}")))?;
        for entry in &entries[genesis_end + 1..] {
            world.replay_entry(entry)?;
        }
        Ok(world)
    }

    /// Applies one entry of a transaction after the genesis. Facts are
    /// keyed at the tx of the `Commit` ending their transaction; since the
    /// tx isn't known until then, they're keyed at `latest_tx + 1`, which
    /// the scan guarantees is the Commit's tx.
    fn replay_entry(&mut self, entry: &LogEntry) -> Result<(), Error> {
        let corrupt =
            |why: &dyn std::fmt::Display| Error::Corrupt(format!("entry#{}: {why}", entry.id.0));
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
                    .map_err(|e| corrupt(&e))?;
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
                    .ok_or_else(|| corrupt(&format!("edge on unknown attribute {attr:?}")))?;
                if def.value_type != ValueType::Ref {
                    return Err(corrupt(&format!(
                        "edge on non-ref attribute {:?}",
                        def.name
                    )));
                }
                for end in [source, target] {
                    if !self.is_defined(&StagedIds::default(), *end) {
                        return Err(corrupt(&format!("edge endpoint {end:?} is undefined")));
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
                    .ok_or_else(|| corrupt(&format!("unknown attribute {attr:?}")))?;
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
                    .map_err(|e| corrupt(&e))?;
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
                // A live write never stores a second entry for one
                // (entity, attr, value, tick), in the same transaction or a
                // later one: the same op again is a no-op, the opposite op a
                // same-tick conflict. One seek at exactly that tick, at every
                // canon (the current transaction's facts are already in).
                let mut prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
                if op_at_tick(&[&self.e_index], &mut prefix, fact.tick, TxId(u64::MAX)).is_some() {
                    return Err(corrupt(&format!(
                        "a second fact for one value at {:?}",
                        fact.tick
                    )));
                }
                self.apply_fact(&fact, self.next_tx());
                Ok(())
            }
            EntryBody::Commit { tx, .. } => {
                if *tx != self.next_tx() {
                    return Err(corrupt(&format!("{tx:?} out of sequence")));
                }
                self.latest_tx = *tx;
                Ok(())
            }
        }
    }

    /// Stores one fact of transaction `tx` in the indexes: its `E` entry
    /// and, for a `ref` fact, its `R` mirror. Commit and replay both apply
    /// facts only through here, so the two indexes always hold the same
    /// facts.
    fn apply_fact(&mut self, fact: &FactDef, tx: TxId) {
        let k = key::e_fact_key(fact.entity, fact.attr, &fact.value, fact.tick, tx);
        self.e_index.insert(k, fact.op);
        if let Some(target) = fact.target {
            let k = key::r_fact_key(target, fact.attr, fact.entity, fact.tick, tx);
            self.r_index.insert(k, fact.op);
        }
        self.max_tick = self.max_tick.max(Some(fact.tick));
    }

    /// Every stored `E` and `R` entry, decoded, in key order: an inspection
    /// hook for tests and (later) doctor's index checks. Reads the two
    /// stored indexes independently; it never derives one from the other.
    #[doc(hidden)]
    pub fn index_dump(&self) -> IndexDump {
        let e = self
            .e_index
            .iter()
            .map(|(k, op)| {
                // Invariant: E keys are only built by `key::e_fact_key` from
                // a validated, encoded value.
                let (entity, attr, value, tick, tx) =
                    key::decode_e_key(k).expect("invariant: stored E keys are well-formed");
                let value = Value::decode(value)
                    .expect("invariant: stored E keys hold a canonical value encoding");
                (entity, attr, value, tick, tx, *op)
            })
            .collect();
        let r = self
            .r_index
            .iter()
            .map(|(k, op)| {
                // Invariant: R keys are only built by `key::r_fact_key`.
                let (target, attr, source, tick, tx) =
                    key::decode_r_key(k).expect("invariant: stored R keys are well-formed");
                (target, attr, source, tick, tx, *op)
            })
            .collect();
        IndexDump { e, r }
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

    /// The world's latest tick: the highest tick of any stored fact (assert
    /// or retract) at the latest canon, or `Tick(0)` if no fact is stored.
    ///
    /// A tick-less write lands here (see [`World::begin`]). It never moves
    /// down: a prequel fact, a no-op or a failed write leaves it as it is.
    pub fn latest_tick(&self) -> Tick {
        self.max_tick.unwrap_or(Tick(0))
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
            facts: Vec::new(),
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
        let res = txn.write(entity, attr, v, tick, op)?;
        txn.commit()?;
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
    pub fn at_canon(&self, tx: TxId) -> View<'_> {
        View {
            world: self,
            canon: tx.min(self.latest_tx),
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

/// Checks an attribute's direction and inverse fit its value type: a `ref`
/// attribute has a direction, nothing else has one or an inverse, and a
/// bidirectional attribute has no inverse (its reverse is itself).
fn check_direction(def: &AttrDef) -> Result<(), Error> {
    let ok = match (def.value_type, def.direction) {
        (ValueType::Ref, Some(RefDirection::Directed)) => true,
        (ValueType::Ref, Some(RefDirection::Bidirectional)) => def.inverse.is_none(),
        (ValueType::Ref, None) => false,
        (_, direction) => direction.is_none() && def.inverse.is_none(),
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

/// Borrows an owned key bound as a slice bound, for `BTreeMap::range`.
fn as_slice(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

/// The result of committing a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitResult {
    /// The transaction's number. If nothing was written (every write was a
    /// no-op and no entity was created), nothing is appended and this is
    /// the unchanged latest tx.
    pub tx: TxId,
    /// Facts stored by the transaction (no-ops not counted).
    pub changed: usize,
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
    /// Staged facts, keyed exactly as they will be in the `E` index (with
    /// `tx`), so judging seeks the committed and staged facts the same way.
    staged: Index,
    /// The staged facts in write order: the order of their log entries.
    facts: Vec<FactDef>,
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
            let layers = [&txn.world.e_index, &txn.staged];
            if judge(&layers, &fact, entity, attr)? == Verdict::Noop {
                // Find-then-act: nothing to change, so nothing is staged.
                return Ok(WriteResult { changed: 0, tick });
            }
            // A stored assert interns its triple's edge if it has none yet.
            // A stored retract never needs to: it requires a stored assert.
            let edge = match fact.target {
                Some(target) if op == Op::Assert => {
                    txn.world
                        .prepare_edge(&txn.ids, fact.entity, fact.attr, target)?
                }
                _ => None,
            };
            if let Some(edge) = edge {
                txn.ids.push_edge(edge);
            }
            let k = key::e_fact_key(fact.entity, fact.attr, &fact.value, tick, txn.tx);
            txn.staged.insert(k, fact.op);
            txn.facts.push(fact);
            Ok(WriteResult { changed: 1, tick })
        })
    }

    /// Makes every staged entity and write visible as one new transaction
    /// and returns its tx and the number of facts stored.
    ///
    /// With nothing staged (only no-ops), nothing is appended and the tx
    /// doesn't advance; a transaction that only creates entities does
    /// advance it. Fails with [`Error::TransactionAborted`], applying
    /// nothing, if a write in the transaction failed.
    ///
    /// In a file-backed world the transaction is appended to the log (entity
    /// `DEFINE`s, then facts in write order, then `COMMIT`) in one write and
    /// fsynced, and only then applied in memory. If that fails, the commit
    /// returns [`Error::Io`], nothing is applied, and the world refuses
    /// further writes.
    pub fn commit(self) -> Result<CommitResult, Error> {
        if self.poisoned {
            return Err(Error::TransactionAborted);
        }
        let world = self.world;
        let changed = self.staged.len();
        if changed == 0 && self.ids.is_empty() {
            return Ok(CommitResult {
                tx: world.latest_tx,
                changed: 0,
            });
        }
        // Write-ahead: the transaction is durable in the log before anything
        // in memory changes, so a failed append leaves the world as it was.
        // Entity DEFINEs, then edge DEFINEs (whose endpoints may be new
        // entities), precede every fact, so no fact uses an ID before the
        // log defines it.
        if let Some(log) = world.log.as_mut() {
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
            let facts = self.facts.iter().map(|f| NewEntry::Fact {
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
                op: f.op,
            });
            log.append(self.tx, entities.chain(edges).chain(facts))?;
        }
        for def in self.ids.defs {
            world.apply_entity(def);
        }
        for def in self.ids.edges {
            world.apply_edge(def);
        }
        // The staged index holds exactly these facts' E keys (it exists so
        // judging can see them); applying the facts rewrites those keys and
        // adds their R mirrors through the same path as replay.
        for fact in &self.facts {
            world.apply_fact(fact, self.tx);
        }
        world.latest_tx = self.tx;
        Ok(CommitResult {
            tx: self.tx,
            changed,
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
}

impl View<'_> {
    /// The canon tx this view reads at.
    pub fn canon(&self) -> TxId {
        self.canon
    }

    /// Whether value `v` of `entity.attr` holds at tick `t` in this canon.
    ///
    /// Takes the latest op with tick `<= t` for that `(entity, attr, v)`
    /// among the visible transactions: it holds if that op is an assert,
    /// and not if it is a retract or there is none. Fails with
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
        Ok(holds(&[&world.e_index], &mut prefix, t, self.canon))
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
            let source_end = |_: &[u8]| key::R_SOURCE_PREFIX_LEN;
            for_each_holding(
                &self.world.r_index,
                &prefix,
                source_end,
                t,
                self.canon,
                |p| f(key::r_source(p).expect("invariant: R index keys are fixed-width")),
            );
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
        // Keys in the E index are only ever built by `key::e_fact_key` from
        // a successful `Value::encode`, so they always parse.
        let value_end = |k: &[u8]| {
            key::E_ATTR_PREFIX_LEN
                + key::e_value_bytes(k)
                    .expect("invariant: E index keys are built only by key::e_fact_key")
                    .len()
        };
        for_each_holding(
            &self.world.e_index,
            &prefix,
            value_end,
            t,
            self.canon,
            |p| {
                f(&p[key::E_ATTR_PREFIX_LEN..]);
            },
        );
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
    /// visible transactions is an assert, so an event holds from its tick
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

    /// The change points of `entity.attr` in this canon: every stored
    /// assert and retract written by a tx `<=` the canon, ordered by tick
    /// ascending, then (same tick) by value encoding ascending.
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
        let world = self.world;
        let (attr_id, _) = world.resolve(&StagedIds::default(), entity, attr)?;
        let visible = |k: &[u8]| key::e_key_tx(k).is_some_and(|tx| tx <= self.canon);
        let attr_prefix = key::e_attr_prefix(entity, attr_id);
        // The scan yields value-ascending, then newest tick first; collect
        // the value bytes (borrowed from E keys) and reorder by
        // (tick, value bytes).
        let mut points: Vec<(Tick, Cow<'_, [u8]>, Op)> = world
            .e_index
            .range::<[u8], _>((Bound::Included(&attr_prefix[..]), Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(&attr_prefix))
            .filter(|(k, _)| visible(k))
            .map(|(k, op)| {
                let (value, tick) = key::e_value_and_tick(k)
                    .expect("invariant: E index keys are built only by key::e_fact_key");
                (tick, Cow::Borrowed(value), *op)
            })
            .collect();
        if world.is_bidirectional(attr_id) {
            // The pairs stored under a smaller partner: their R mirrors.
            // A self pair's R mirror is the E entry already collected.
            let r_prefix = key::r_attr_prefix(entity, attr_id);
            for (k, op) in world
                .r_index
                .range::<[u8], _>((Bound::Included(&r_prefix[..]), Bound::Unbounded))
                .take_while(|(k, _)| k.starts_with(&r_prefix))
                .filter(|(k, _)| visible(k))
            {
                let (_, _, source, tick, _) =
                    key::decode_r_key(k).expect("invariant: R index keys are fixed-width");
                if source != entity {
                    let value = Value::Ref(source)
                        .encode()
                        .expect("invariant: a ref value always encodes");
                    points.push((tick, Cow::Owned(value), *op));
                }
            }
        }
        // (tick, value) is unique among the points: same-tick conflicts are
        // rejected and repeats are no-ops, so each (e, a, v, tick) has at
        // most one stored entry in any canon, and a bidirectional pair is
        // stored under one side only (the self pair's R side is skipped).
        // Corrections (DELETED markers and revisions at the same key,
        // different tx) will break this, and must then keep only the
        // visible revision per (v, tick).
        points.sort_unstable_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        Ok(points
            .into_iter()
            .map(|(tick, value, op)| Change {
                tick,
                op,
                value: Value::decode(&value).expect(
                    "invariant: E index keys are built only by key::e_fact_key \
                     from a successful Value::encode",
                ),
            })
            .collect())
    }
}

/// The distinct-history walk behind `AsOf(entity, attr, T)` and
/// `Neighbors`, for any index whose keys are `attr_prefix | x | ^tick | ^tx`
/// (`E|e|a|value|...`, `R|t|a|source|...`). Calls `f` with the history
/// prefix `attr_prefix | x` of every `x` whose latest entry with tick `<= t`
/// and tx `<= canon` is an assert, in key order.
///
/// `history_end(key)` is the length of a key's history prefix. Each round
/// finds the next distinct history, seeks it at `t`, then jumps past all of
/// its keys, so the cost is one seek per distinct `x` ever stored.
fn for_each_holding(
    index: &Index,
    attr_prefix: &[u8],
    history_end: impl Fn(&[u8]) -> usize,
    t: Tick,
    canon: TxId,
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
        if holds(&[index], &mut prefix, t, canon) {
            f(&prefix);
        }
    }
}

/// The edge of a stored `ref` pair `(source, attr, target)` of `world`.
fn edge_or_panic(world: &World, attr: AttrId, source: EntityId, target: EntityId) -> EdgeId {
    world
        .edge_of(&StagedIds::default(), source, attr, target)
        .expect("invariant: every stored ref fact's triple has an interned edge")
}

/// Decodes the value bytes of a stored `E` key.
fn decode_e_value(value: &[u8]) -> Value {
    Value::decode(value).expect(
        "invariant: E index keys are built only by key::e_fact_key from a successful Value::encode",
    )
}

/// Judges a validated fact against the facts in `layers`, at the fact's own
/// tick (never the latest tick, so out-of-order writes are kept).
///
/// First the same-tick rule: if the opposite op is stored for the same
/// `(entity, attr, value)` at exactly this tick, the write fails with
/// [`Error::SameTickConflict`] (tick order can't decide between them, and
/// letting tx order decide would break "truth follows tick order"). Then
/// the no-op rule: an assert of a value that already holds there, or a
/// retract of one that doesn't, changes nothing. `written` (the subject as
/// the caller wrote it, before any bidirectional normalisation) and `attr`
/// (the attribute's name) are for the error.
fn judge(
    layers: &[&Index],
    fact: &FactDef,
    written: EntityId,
    attr: &str,
) -> Result<Verdict, Error> {
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
    Ok(if noop { Verdict::Noop } else { Verdict::Store })
}

/// One seek: the op stored under `value_prefix` (`E|e|a|v|`) at exactly
/// tick `t` by a tx `<= canon`, if any. Because conflicting ops at one tick
/// are rejected and repeats are no-ops, a tick holds at most one op. Reuses
/// `value_prefix` as the seek key buffer; it is restored before returning.
fn op_at_tick(layers: &[&Index], value_prefix: &mut Vec<u8>, t: Tick, canon: TxId) -> Option<Op> {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    // The prefix to match includes `^t`: only entries at exactly `t`.
    let found = seek(layers, value_prefix, value_prefix, canon);
    value_prefix.truncate(len);
    found
}

/// One seek: whether the latest entry under `value_prefix` (`E|e|a|v|`)
/// with tick `<= t` and tx `<= canon` is an assert. Reuses `value_prefix`
/// as the seek key buffer; it is restored before returning.
fn holds(layers: &[&Index], value_prefix: &mut Vec<u8>, t: Tick, canon: TxId) -> bool {
    let len = value_prefix.len();
    key::push_seek_tick(value_prefix, t);
    let found = seek(layers, value_prefix, &value_prefix[..len], canon);
    value_prefix.truncate(len);
    found == Some(Op::Assert)
}

/// The op of the first key `>= from` that starts with `prefix` and was
/// written by a tx `<= canon`, reading all `layers` as if they were one
/// index: the smallest such key wins.
///
/// Within one `E|e|a|v|` history the smallest key is the latest entry
/// (newest tick first, then newest tx). That holds across layers because a
/// transaction's staged keys carry its own tx, newer than any committed one.
/// Keys order by `^tick` before `^tx`, so a newer-tx entry can sit before
/// the answer at any tick (a later prequel or retract); those are skipped
/// one by one. At the latest canon nothing is skipped: still one seek.
fn seek(layers: &[&Index], from: &[u8], prefix: &[u8], canon: TxId) -> Option<Op> {
    layers
        .iter()
        .filter_map(|index| {
            index
                .range::<[u8], _>((Bound::Included(from), Bound::Unbounded))
                .take_while(|(k, _)| k.starts_with(prefix))
                .find(|(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= canon))
        })
        .min_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, op)| *op)
}
