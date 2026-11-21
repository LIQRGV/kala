//! A Kala world: one story timeline, its schema and its entities.
//!
//! Facts live in the in-memory `E` index (layout in the private `key`
//! module). A world is either in memory only ([`World::in_memory`]) or backed
//! by a log file ([`World::create`], format in [`crate::log`]), which is then
//! the source of truth: a committed [`Txn`] is appended as its `DEFINE`
//! entries, its facts and a `COMMIT`, and fsynced, before the in-memory
//! tables change. Every operation first validates into a definition
//! ([`EntityDef`], [`FactDef`]) without touching the world, then applies it,
//! so a failed operation changes nothing.
//!
//! Every write goes through a [`Txn`]: [`World::create_entity`],
//! [`World::assert`] and [`World::retract`] are one-write transactions that
//! commit at once. Every read goes through a [`View`] pinned to a canon tx;
//! the world's own reads are the view at its latest tx.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Bound;
use std::path::Path;

use crate::error::Error;
use crate::id::{AttrId, EntityId, IdError, MAX_NODE_TYPE, NodeTypeId};
use crate::key;
use crate::log::{EntryBody, LogEntry, LogWriter, NewEntry};
use crate::schema::{AttrDef, Kind, Schema};
use crate::tick::Tick;
use crate::value::Value;

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

/// Entities a transaction has created but not committed. Creating one only
/// stages it; the world's ID tables change on commit.
#[derive(Debug, Default)]
struct StagedEntities {
    /// In creation order (later: the order of their `DEFINE` entries).
    defs: Vec<EntityDef>,
    /// Last staged sequence number per node type. Staged sequences of a type
    /// continue the world's without gaps, so `1..=last` are all defined.
    last_seq: HashMap<NodeTypeId, u64>,
    handles: HashSet<String>,
}

impl StagedEntities {
    fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    fn push(&mut self, def: EntityDef) {
        self.last_seq.insert(def.id.node_type(), def.id.seq());
        if let Some(h) = &def.handle {
            self.handles.insert(h.clone());
        }
        self.defs.push(def);
    }
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
    /// The last committed transaction (0 = none yet).
    latest_tx: TxId,
    /// Forward index `E|entity|attr|value|^tick|^tx -> op` (layout in
    /// [`crate::key`]). In milestone 0 this is the only copy of the facts.
    e_index: Index,
    /// Highest tick of any stored fact (assert or retract), `None` if no
    /// fact is stored. Derived from the facts; maintained on commit (later
    /// rebuilt on log replay, like the indexes).
    max_tick: Option<Tick>,
    /// The log file, for a file-backed world; `None` in memory.
    log: Option<LogWriter>,
}

impl World {
    /// Creates an empty in-memory world (no file) with the given schema.
    ///
    /// Fails with [`Error::InvalidName`] for an empty node type or attribute
    /// name, [`Error::DuplicateName`] if a node type or an attribute is
    /// declared twice (the two are separate namespaces), or [`Error::Id`] if
    /// there are more node types than fit in an ID.
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
        for (i, def) in defs.iter().enumerate() {
            if def.name.is_empty() {
                return Err(Error::InvalidName(def.name.clone()));
            }
            // More than u32::MAX attributes can't be keyed; report it as a
            // bad schema rather than wrapping into another attribute's ID.
            let id = AttrId(u32::try_from(i).map_err(|_| Error::InvalidName(def.name.clone()))?);
            if attr_by_name.insert(def.name.clone(), id).is_some() {
                return Err(Error::DuplicateName(def.name.clone()));
            }
        }
        Ok(World {
            node_types,
            node_type_by_name,
            entity_by_handle: HashMap::new(),
            attributes: defs.to_vec(),
            attr_by_name,
            latest_tx: TxId(0),
            e_index: BTreeMap::new(),
            max_tick: None,
            log: None,
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
    /// A torn tail (an incomplete or bad-checksum last transaction, left by
    /// a crash mid-append) is cut off the file and fsynced once the rest has
    /// replayed cleanly; the world then continues entry IDs and tx from the
    /// last committed transaction. Before the cut, the cut bytes are saved
    /// and fsynced to `<path>.torn-<valid_len>` (with `-1`, `-2`, ... added
    /// if that exists); if saving them fails, this fails with [`Error::Io`]
    /// and the log is not cut. Damage followed by a committed transaction,
    /// or a tail holding a frame shaped like a later transaction's `Commit`,
    /// is [`Error::Corrupt`] instead, with the file untouched.
    pub fn open(path: &Path) -> Result<World, Error> {
        let (mut world, writer) = LogWriter::open(path, World::replay)?;
        world.log = Some(writer);
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
                    .filter(|_| id.raw() >> 63 == 0)
                    .map(|nt| nt.name.clone())
                    .ok_or_else(|| corrupt(&format!("entity {id:?} has an unknown node type")))?;
                let def = self
                    .prepare_entity(&StagedEntities::default(), &name, handle.as_deref())
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
                let fact = self
                    .prepare_fact(
                        &StagedEntities::default(),
                        *entity,
                        &def.name,
                        value,
                        *tick,
                        *op,
                    )
                    .map_err(|e| corrupt(&e))?;
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
                let k = key::e_fact_key(
                    fact.entity,
                    fact.attr,
                    &fact.value,
                    fact.tick,
                    self.next_tx(),
                );
                self.e_index.insert(k, fact.op);
                self.max_tick = self.max_tick.max(Some(fact.tick));
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

    /// `Err` if the world refuses writes: its log append failed earlier.
    fn writable(&self) -> Result<(), Error> {
        self.log.as_ref().map_or(Ok(()), LogWriter::check)
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
            max_tick: None,
            entities: StagedEntities::default(),
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
    /// defined, both as the subject and as a `ref` value.
    fn prepare_fact(
        &self,
        staged: &StagedEntities,
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
        let value = v.encode()?;
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
        Ok(FactDef {
            entity,
            attr: attr_id,
            value,
            tick,
            op,
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
        staged: &StagedEntities,
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

    /// Whether `id` names an entity created in this world or staged in
    /// `staged` by the transaction asking.
    fn is_defined(&self, staged: &StagedEntities, id: EntityId) -> bool {
        // Bit 63 is the edge tag; there are no edges yet, so any tagged ID
        // is undefined. Otherwise sequences 1..=last_seq of a declared type
        // exist, because sequences are assigned without gaps.
        id.raw() >> 63 == 0
            && id.seq() >= 1
            && self
                .last_seq(staged, id.node_type())
                .is_some_and(|last| id.seq() <= last)
    }

    /// The last sequence number assigned to node type `t`, counting staged
    /// entities; `None` if the type isn't declared.
    fn last_seq(&self, staged: &StagedEntities, t: NodeTypeId) -> Option<u64> {
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
        staged: &StagedEntities,
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

    /// Formats an ID as `TYPE#seq` (`CHARACTER#1`).
    ///
    /// An ID whose node type isn't in this world's schema (e.g. decoded from
    /// foreign bytes) is shown as `#<type id>#seq` rather than failing.
    pub fn format_id(&self, id: EntityId) -> String {
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
    /// `tx`), so a commit is a plain insert and judging seeks both maps the
    /// same way.
    staged: Index,
    /// The staged facts in write order: the order of their log entries.
    facts: Vec<FactDef>,
    /// Highest tick among `staged`, `None` if nothing is staged.
    max_tick: Option<Tick>,
    /// Entities created by this transaction.
    entities: StagedEntities,
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
            let def = txn.world.prepare_entity(&txn.entities, node_type, handle)?;
            let id = def.id;
            txn.entities.push(def);
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
                .prepare_fact(&txn.entities, entity, attr, v, tick, op)?;
            if judge(&[&txn.world.e_index, &txn.staged], &fact, attr)? == Verdict::Noop {
                // Find-then-act: nothing to change, so nothing is staged.
                return Ok(WriteResult { changed: 0, tick });
            }
            let k = key::e_fact_key(fact.entity, fact.attr, &fact.value, tick, txn.tx);
            txn.staged.insert(k, fact.op);
            txn.facts.push(fact);
            txn.max_tick = txn.max_tick.max(Some(tick));
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
        if changed == 0 && self.entities.is_empty() {
            return Ok(CommitResult {
                tx: world.latest_tx,
                changed: 0,
            });
        }
        // Write-ahead: the transaction is durable in the log before anything
        // in memory changes, so a failed append leaves the world as it was.
        // Entity DEFINEs precede every fact, so no fact uses an ID before
        // the log defines it.
        if let Some(log) = world.log.as_mut() {
            let defines = self.entities.defs.iter().map(|d| NewEntry::Entity {
                id: d.id,
                handle: d.handle.as_deref(),
            });
            let facts = self.facts.iter().map(|f| NewEntry::Fact {
                entity: f.entity,
                attr: f.attr,
                value: &f.value,
                tick: f.tick,
                op: f.op,
            });
            log.append(self.tx, defines.chain(facts))?;
        }
        for def in self.entities.defs {
            world.apply_entity(def);
        }
        world.e_index.extend(self.staged);
        world.max_tick = world.max_tick.max(self.max_tick);
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
        let (attr_id, def) = world.resolve(&StagedEntities::default(), entity, attr)?;
        check_type(attr, def, v)?;
        let mut prefix = key::e_value_prefix(entity, attr_id, &v.encode()?);
        Ok(holds(&[&world.e_index], &mut prefix, t, self.canon))
    }

    /// Every value of `entity.attr` that holds at tick `t` in this canon,
    /// sorted by value encoding (the value's natural order within its type;
    /// for `ref`, entity ID order).
    ///
    /// Cost grows with the number of distinct values the attribute has ever
    /// had: one seek per value (plus skipped newer-tx entries at an older
    /// canon). Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn as_of(&self, entity: EntityId, attr: &str, t: Tick) -> Result<Vec<Value>, Error> {
        let world = self.world;
        let (attr_id, _) = world.resolve(&StagedEntities::default(), entity, attr)?;
        let attr_prefix = key::e_attr_prefix(entity, attr_id);
        let mut out = Vec::new();
        let mut from = Bound::Included(attr_prefix.clone());
        // Each round finds the next distinct value under `E|e|a|`, seeks its
        // history at `t`, then jumps past all of that value's keys.
        while let Some((k, _)) = world
            .e_index
            .range::<[u8], _>((as_slice(&from), Bound::Unbounded))
            .next()
        {
            if !k.starts_with(&attr_prefix) {
                break;
            }
            // Keys in the index are only ever built by `key::e_fact_key`
            // from a successful `Value::encode`, so they always parse.
            let value = key::e_value_bytes(k)
                .expect("invariant: E index keys are built only by key::e_fact_key");
            let mut prefix = key::e_value_prefix(entity, attr_id, value);
            from = Bound::Excluded(key::past_value(&prefix));
            if holds(&[&world.e_index], &mut prefix, t, self.canon) {
                out.push(
                    Value::decode(value)
                        .expect("invariant: E index keys are built only by key::e_fact_key from a successful Value::encode"),
                );
            }
        }
        Ok(out)
    }

    /// The change points of `entity.attr` in this canon: every stored
    /// assert and retract written by a tx `<=` the canon, ordered by tick
    /// ascending, then (same tick) by value encoding ascending.
    ///
    /// Only effective writes are stored, so no-ops and failed writes never
    /// appear. A change point is where something changed, not every tick
    /// where a value holds. Cost is one scan of `E|e|a|`: every entry the
    /// attribute has ever had. Fails with [`Error::UnknownAttribute`] or
    /// [`Error::UnknownEntity`].
    pub fn history(&self, entity: EntityId, attr: &str) -> Result<Vec<Change>, Error> {
        let world = self.world;
        let (attr_id, _) = world.resolve(&StagedEntities::default(), entity, attr)?;
        let attr_prefix = key::e_attr_prefix(entity, attr_id);
        // The scan yields value-ascending, then newest tick first; collect
        // borrowed value bytes and reorder by (tick, value bytes).
        let mut points: Vec<(Tick, &[u8], Op)> = world
            .e_index
            .range::<[u8], _>((Bound::Included(&attr_prefix[..]), Bound::Unbounded))
            .take_while(|(k, _)| k.starts_with(&attr_prefix))
            .filter(|(k, _)| key::e_key_tx(k).is_some_and(|tx| tx <= self.canon))
            .map(|(k, op)| {
                let (value, tick) = key::e_value_and_tick(k)
                    .expect("invariant: E index keys are built only by key::e_fact_key");
                (tick, value, *op)
            })
            .collect();
        // (tick, value) is unique among the points: same-tick conflicts are
        // rejected and repeats are no-ops, so each (e, a, v, tick) has at
        // most one stored entry in any canon. Corrections (DELETED markers
        // and revisions at the same key, different tx) will break this, and
        // must then keep only the visible revision per (v, tick).
        points.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        Ok(points
            .into_iter()
            .map(|(tick, value, op)| Change {
                tick,
                op,
                value: Value::decode(value).expect(
                    "invariant: E index keys are built only by key::e_fact_key \
                     from a successful Value::encode",
                ),
            })
            .collect())
    }
}

/// Judges a validated fact against the facts in `layers`, at the fact's own
/// tick (never the latest tick, so out-of-order writes are kept).
///
/// First the same-tick rule: if the opposite op is stored for the same
/// `(entity, attr, value)` at exactly this tick, the write fails with
/// [`Error::SameTickConflict`] (tick order can't decide between them, and
/// letting tx order decide would break "truth follows tick order"). Then
/// the no-op rule: an assert of a value that already holds there, or a
/// retract of one that doesn't, changes nothing. `attr` is the attribute's
/// name, for the error.
fn judge(layers: &[&Index], fact: &FactDef, attr: &str) -> Result<Verdict, Error> {
    let mut prefix = key::e_value_prefix(fact.entity, fact.attr, &fact.value);
    // Writes are judged at the latest canon plus the transaction's own
    // staged facts, so every entry is visible.
    let all = TxId(u64::MAX);
    if let Some(stored) = op_at_tick(layers, &mut prefix, fact.tick, all)
        && stored != fact.op
    {
        return Err(Error::SameTickConflict {
            entity: fact.entity,
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
