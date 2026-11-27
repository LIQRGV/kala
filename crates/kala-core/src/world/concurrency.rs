//! Committing over newer commits: what a transaction read, the operations
//! it made, and how both are settled when other handles appended to the log
//! since it began.
//!
//! A transaction works on the world as it was when it began (its
//! *snapshot*). Everything it allocates there is provisional: entity
//! sequence numbers, edge IDs, entry IDs, its tx number, even the IDs of
//! attributes and node types it declares. When its commit takes the writer
//! lock and finds that other transactions were appended meanwhile, it
//!
//! 1. (optimistic mode only) checks every key it **read** against the
//!    entries those transactions appended ([`ReadSet::check`]); a match is
//!    [`Error::Conflict`] and nothing is written;
//! 2. **finalizes**: makes the operations it recorded again, in order, on
//!    the caught-up world ([`Txn::finalize`]). That is by construction what
//!    applying the transaction after the newer ones gives: final IDs, an
//!    edge a newer commit interned reused, the store's rules judged against
//!    the newer facts (with their ordinary errors), a write that became
//!    redundant "0 changed". Operations name attributes and node types by
//!    name and entities by ID; IDs the transaction allocated are translated
//!    as the operations that allocated them are made again;
//! 3. appends the result, still under the lock.
//!
//! Nothing here runs for a single writer: with nothing appended since the
//! transaction began, the commit appends exactly what was staged.

use super::*;
use crate::error::{Conflict, ConflictKey};
use std::cell::RefMut;

/// How many keys a transaction's read set keeps exactly. One key more and
/// the set is kept per attribute instead (a read of `(entity, attribute)`,
/// `(target, attribute)` or `(attribute, value)` becomes a read of the
/// whole attribute, a corrected entry a read of its attribute, and type
/// scans and handle reads become "any new entity"), which conflicts more
/// often but is bounded by the schema, however much a query read.
pub const MAX_READ_KEYS: usize = 1024;

/// A value lookup with an encoding longer than this is recorded as a read
/// of its whole attribute: the read set never holds large values.
const MAX_VALUE_KEY_BYTES: usize = 256;

/// How a transaction's commit treats transactions committed after it began
/// (the `ConcurrencyControl` choice): set per transaction with
/// [`Txn::set_concurrency`].
///
/// Either way the commit takes the writer lock, catches up, re-applies the
/// transaction's operations to the caught-up world under the store's own
/// rules (duplicate handle, same-tick conflict, an entry corrected
/// meanwhile, deprecated attribute, strict schema, value type), assigns
/// final IDs and reports the
/// ID mapping and a moved latest tick. The modes differ only in what the
/// transaction's **reads** mean.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Concurrency {
    /// The default: the transaction records what it reads, and its commit
    /// fails with [`Error::Conflict`] if a transaction committed since it
    /// began wrote through one of those keys. Blind writes never conflict.
    /// Committed transactions are serializable in commit order, provided
    /// every read was recorded ([`Txn::values_at`] and find-or-create
    /// record theirs; query layers use the `note_*` methods).
    #[default]
    Optimistic,
    /// Store rules only: reads aren't recorded and nothing is checked, so
    /// a write decided from a read may overwrite a newer commit's (a lost
    /// update). An explicit per-transaction opt-in; no world setting
    /// selects it.
    Weak,
}

/// The provisional IDs a commit renumbered ([`CommitResult::ids`]): pairs
/// `(provisional, final)`, in the order the transaction allocated them.
/// Only IDs that changed are listed; a transaction that committed with
/// nothing appended since it began has an empty map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdMap {
    /// Entities: the ID [`Txn::create_entity`] (or find-or-create) returned
    /// and the one the entity was committed with.
    pub entities: Vec<(EntityId, EntityId)>,
    /// Edges: the ID [`Txn::edge_id`] returned during the transaction and
    /// the committed one (a new sequence number, or the edge a newer commit
    /// had already interned for the triple).
    pub edges: Vec<(EdgeId, EdgeId)>,
}

impl IdMap {
    /// Whether no ID changed.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.edges.is_empty()
    }

    /// The committed ID of an entity or edge ID the transaction handed out
    /// (`id` itself if it didn't change).
    pub fn resolve(&self, id: EntityId) -> EntityId {
        if let Some(edge) = id.as_edge() {
            return self
                .edges
                .iter()
                .find(|(provisional, _)| *provisional == edge)
                .map_or(id, |(_, fin)| EntityId::from(*fin));
        }
        self.entities
            .iter()
            .find(|(provisional, _)| *provisional == id)
            .map_or(id, |(_, fin)| *fin)
    }

    /// [`IdMap::resolve`] for an edge ID.
    pub fn resolve_edge(&self, id: EdgeId) -> EdgeId {
        self.edges
            .iter()
            .find(|(provisional, _)| *provisional == id)
            .map_or(id, |(_, fin)| *fin)
    }
}

/// The warning of a commit whose tick-less writes stayed on the latest tick
/// the transaction began with although a newer commit moved it
/// ([`CommitResult::latest_tick_moved`]). Displays as
/// `latest tick moved 601 → 610 while this transaction ran`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TickMoved {
    /// The world's latest tick when the transaction began: where its
    /// tick-less writes are.
    pub from: Tick,
    /// The latest tick the transaction was committed over.
    pub to: Tick,
}

impl std::fmt::Display for TickMoved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "latest tick moved {} → {} while this transaction ran",
            self.from.0, self.to.0
        )
    }
}

/// What a commit over newer transactions made of each operation
/// ([`CommitResult::finalized`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finalized {
    /// How many transactions were committed since this one began.
    pub applied: usize,
    /// One per operation the transaction made, in order (index =
    /// [`Txn::operation_count`] before the operation).
    pub ops: Vec<FinalizedOp>,
}

/// The outcome of one operation as committed (see [`Finalized`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedOp {
    /// Fact, `DELETED` and revised entries it stored.
    pub changed: usize,
    /// Schema actions it reported.
    pub declared: usize,
    /// For a find-or-create: whether it created the entity.
    pub created: Option<bool>,
    /// For an assert or retract that stored its fact: the fact's precision.
    pub precision: Option<Precision>,
}

/// The sizes of the ID tables when a transaction began: an ID past them is
/// one the transaction allocated itself.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Snapshot {
    attrs: usize,
    node_types: usize,
    edges: usize,
}

impl Snapshot {
    pub(super) fn of(catalog: &Catalog) -> Snapshot {
        Snapshot {
            attrs: catalog.attributes.len(),
            node_types: catalog.node_types.len(),
            edges: catalog.edges.len(),
        }
    }
}

/// One operation of a transaction, as the caller made it: what
/// [`Txn::finalize`] makes again. Attributes and node types are names;
/// entity IDs (subjects and `ref` values) may be provisional.
#[derive(Debug)]
pub(super) enum Call {
    Create {
        node_type: String,
        handle: Option<String>,
        /// The provisional ID it returned.
        id: EntityId,
    },
    FindOrCreate {
        node_type: String,
        handle: String,
        id: EntityId,
        created: bool,
    },
    Write {
        entity: EntityId,
        attr: String,
        value: Value,
        /// The tick the write resolved to.
        tick: Tick,
        op: Op,
        precision: Precision,
    },
    Correct {
        entry: EntryId,
        correction: Correction,
        /// The chain it corrected, by base entry, and the chain's
        /// revision in the world the transaction began on.
        chain: Option<(EntryId, u32)>,
    },
    InferWriteAttribute {
        name: String,
        value: Value,
        op: Op,
    },
    /// [`Txn::write_inferring`]: the inference and the write it guards.
    InferredWrite {
        entity: EntityId,
        attr: String,
        value: Value,
        tick: Tick,
        op: Op,
        precision: Precision,
    },
    InferAttribute {
        name: String,
        value: Value,
    },
    InferAttributeWith {
        name: String,
        value_type: ValueType,
        options: AttributeOptions,
    },
    InferNodeType(String),
    DeclareAttribute(AttrDef),
    DeclareAttributeWith {
        name: String,
        value_type: ValueType,
        options: AttributeOptions,
    },
    DeclareNodeType(String),
    Rename {
        old: String,
        new: String,
    },
    Deprecate(String),
    SetCardinality {
        name: String,
        cardinality: Cardinality,
    },
    WorldSetting {
        key: String,
        value: String,
    },
    Migrate {
        name: String,
        options: MigrationOptions,
    },
}

/// A recorded operation and the edges it interned.
#[derive(Debug)]
pub(super) struct Recorded {
    call: Call,
    /// Each edge the operation interned: its provisional ID and its triple,
    /// the attribute by the name it had when the operation finished.
    edges: Vec<(EdgeId, EntityId, String, EntityId)>,
}

/// What an optimistic transaction read, as keys a newer entry is matched
/// against. Each key remembers how many operations the transaction had made
/// when it was first recorded.
#[derive(Debug, Default)]
pub(super) struct ReadSet {
    /// `note_read_all`: any newer transaction conflicts.
    all: Option<usize>,
    /// `(entity, attribute)`: a newer entry on that entity and attribute.
    entity_attr: HashMap<(EntityId, AttrId), usize>,
    /// `(target, attribute)`: a newer entry whose value is that target.
    reverse: HashMap<(EntityId, AttrId), usize>,
    /// `(attribute, encoded value)`: a newer entry with that value.
    values: HashMap<AttrId, HashMap<Vec<u8>, usize>>,
    value_count: usize,
    /// Whole attributes: a newer entry or schema edit on the attribute.
    attrs: HashMap<AttrId, usize>,
    /// Node types scanned: a newer entity of that type.
    types: HashMap<NodeTypeId, usize>,
    /// Any newer entity definition (coarsened type scans and handles).
    any_define: Option<usize>,
    /// Handles found free: a newer entity with that handle.
    handles: HashMap<String, usize>,
    /// Corrected (or read) chains by base entry, with the entry as the
    /// caller named it and the chain's attribute: a newer correction of it.
    entries: HashMap<EntryId, (usize, EntryId, AttrId)>,
    /// Attribute names the committed schema lacked when the transaction
    /// began, which it found undeclared or declared itself and then read:
    /// a newer declaration of the name (see [`NewAttrRead`]).
    new_attrs: HashMap<String, NewAttrRead>,
    /// Node type names found undeclared in the committed schema: a newer
    /// declaration of the name.
    undeclared_types: HashMap<String, usize>,
    /// Provisional edges whose properties (or incoming refs) were read: a
    /// conflict if the edge turns out to be one a newer commit interned.
    provisional: HashMap<EntityId, (usize, String)>,
    /// Past [`MAX_READ_KEYS`]: only `attrs` and `any_define` grow.
    coarse: bool,
}

/// How many entities one [`NewAttrRead`] keeps exactly; past it the read
/// is of the whole attribute.
const MAX_NEW_ATTR_KEYS: usize = 64;

/// What a transaction read under an attribute name that the committed
/// schema didn't have when it began. Nobody else can have written such an
/// attribute, unless a newer commit declared the same name.
#[derive(Debug)]
struct NewAttrRead {
    /// The operation count at the first read.
    at: usize,
    /// The definition this transaction gave the name (declared or
    /// inferred) before it read under it; `None` if it read the name's
    /// **absence** (a retract dropped because the attribute didn't exist, a
    /// lookup of the definition that found none).
    def: Option<AttrDef>,
    /// The whole attribute was read (a value lookup, a scan, an absence).
    whole: bool,
    /// Entities whose values were read, and targets whose referrers were.
    forward: HashSet<EntityId>,
    reverse: HashSet<EntityId>,
}

/// Which part of a newly declared attribute a read covered.
#[derive(Clone, Copy, Debug)]
enum NewAttrKey {
    Whole,
    Forward(EntityId),
    Reverse(EntityId),
}

impl NewAttrRead {
    fn add(&mut self, key: NewAttrKey) {
        match key {
            NewAttrKey::Whole => self.whole = true,
            NewAttrKey::Forward(e) => {
                self.forward.insert(e);
            }
            NewAttrKey::Reverse(e) => {
                self.reverse.insert(e);
            }
        }
        if self.forward.len() + self.reverse.len() > MAX_NEW_ATTR_KEYS {
            self.whole = true;
            self.forward.clear();
            self.reverse.clear();
        }
    }

    /// Whether a newer commit, which declared the name as `committed` (now
    /// attribute `id`), invalidates the read: the first conflicting key.
    ///
    /// A definition other than the transaction's own does: what it read
    /// (or dropped, or inferred) depended on the name being free, and its
    /// operations would mean something else now. The **same** definition
    /// doesn't by itself: making the transaction's declaration again finds
    /// it done ("0 declared") and its writes land on the one attribute, as
    /// they would for a single writer. Then only facts the newer commits
    /// stored under the attribute matter, where the transaction read
    /// "nothing here": on an entity (or target) it read, or anywhere if it
    /// read the whole attribute.
    fn conflict(
        &self,
        name: &str,
        id: AttrId,
        committed: &AttrDef,
        tail: &[EntryInfo],
    ) -> Option<ConflictKey> {
        if self.def.as_ref() != Some(committed) {
            return Some(ConflictKey::Undeclared {
                name: name.to_owned(),
            });
        }
        tail.iter().find_map(|entry| {
            let (entity, attr, value, ..) = as_fact_or_marker(&entry.body)?;
            if attr != id {
                return None;
            }
            if self.whole {
                return Some(ConflictKey::Attribute {
                    attr: name.to_owned(),
                });
            }
            if self.forward.contains(&entity) {
                return Some(ConflictKey::EntityAttribute {
                    entity,
                    attr: name.to_owned(),
                });
            }
            match value {
                Value::Ref(target) if self.reverse.contains(target) => Some(ConflictKey::Reverse {
                    target: *target,
                    attr: name.to_owned(),
                }),
                _ => None,
            }
        })
    }
}

/// The entity, attribute and value of a `Fact`, `DELETED` or `Revised`
/// entry; `None` for any other entry.
fn as_fact_or_marker(body: &EntryBody) -> Option<(EntityId, AttrId, &Value)> {
    match body {
        EntryBody::Fact {
            entity,
            attr,
            value,
            ..
        }
        | EntryBody::Deleted {
            entity,
            attr,
            value,
            ..
        }
        | EntryBody::Revised {
            entity,
            attr,
            value,
            ..
        } => Some((*entity, *attr, value)),
        _ => None,
    }
}

impl ReadSet {
    fn exact_keys(&self) -> usize {
        self.entity_attr.len()
            + self.reverse.len()
            + self.value_count
            + self.types.len()
            + self.handles.len()
            + self.entries.len()
    }

    /// Past the bound: every exact key becomes its attribute, and type
    /// scans and handles become "any new entity".
    fn bound(&mut self) {
        if self.coarse || self.exact_keys() <= MAX_READ_KEYS {
            return;
        }
        self.coarse = true;
        let attr = |attrs: &mut HashMap<AttrId, usize>, a: AttrId, at: usize| {
            let first = attrs.entry(a).or_insert(at);
            *first = (*first).min(at);
        };
        for ((_, a), at) in self.entity_attr.drain().chain(self.reverse.drain()) {
            attr(&mut self.attrs, a, at);
        }
        for (a, values) in self.values.drain() {
            for at in values.into_values() {
                attr(&mut self.attrs, a, at);
            }
        }
        self.value_count = 0;
        for (_, (at, _, a)) in self.entries.drain() {
            attr(&mut self.attrs, a, at);
        }
        let defines = self
            .types
            .drain()
            .map(|(_, at)| at)
            .chain(self.handles.drain().map(|(_, at)| at))
            .min();
        if let Some(at) = defines {
            self.any_define = Some(self.any_define.map_or(at, |first| first.min(at)));
        }
    }

    fn read_all(&mut self, at: usize) {
        self.all.get_or_insert(at);
    }

    fn read_attr(&mut self, attr: AttrId, at: usize) {
        self.attrs.entry(attr).or_insert(at);
    }

    fn read_entity_attr(&mut self, entity: EntityId, attr: AttrId, at: usize) {
        if self.coarse {
            return self.read_attr(attr, at);
        }
        self.entity_attr.entry((entity, attr)).or_insert(at);
        self.bound();
    }

    fn read_reverse(&mut self, target: EntityId, attr: AttrId, at: usize) {
        if self.coarse {
            return self.read_attr(attr, at);
        }
        self.reverse.entry((target, attr)).or_insert(at);
        self.bound();
    }

    fn read_value(&mut self, attr: AttrId, value: Vec<u8>, at: usize) {
        if self.coarse || value.len() > MAX_VALUE_KEY_BYTES {
            return self.read_attr(attr, at);
        }
        let values = self.values.entry(attr).or_default();
        if let std::collections::hash_map::Entry::Vacant(free) = values.entry(value) {
            free.insert(at);
            self.value_count += 1;
        }
        self.bound();
    }

    fn read_type(&mut self, node_type: NodeTypeId, at: usize) {
        if self.coarse {
            self.any_define.get_or_insert(at);
            return;
        }
        self.types.entry(node_type).or_insert(at);
        self.bound();
    }

    fn read_handle(&mut self, handle: &str, at: usize) {
        if self.coarse {
            self.any_define.get_or_insert(at);
            return;
        }
        if !self.handles.contains_key(handle) {
            self.handles.insert(handle.to_owned(), at);
        }
        self.bound();
    }

    fn read_entry(&mut self, base: EntryId, named: EntryId, attr: AttrId, at: usize) {
        if self.coarse {
            return self.read_attr(attr, at);
        }
        self.entries.entry(base).or_insert((at, named, attr));
        self.bound();
    }

    /// Whether a read of provisional edge `edge` was recorded, and when.
    fn read_provisional(&self, edge: EntityId) -> Option<&(usize, String)> {
        self.provisional.get(&edge)
    }

    /// Matches the entries of every transaction from `first` on (the ones
    /// committed since the reading transaction began; `applied` of them)
    /// against the set. `world` is caught up with them and has its
    /// committed schema. Costs the tail's entries, each a few map lookups
    /// (a schema edit: one pass over the exact keys).
    fn check(&self, world: &World, first: TxId, applied: usize) -> Result<(), Error> {
        let conflict = |key: ConflictKey, operation: usize| {
            Err(Error::Conflict(Conflict {
                key,
                operation,
                applied,
            }))
        };
        if let Some(at) = self.all {
            return conflict(ConflictKey::All, at);
        }
        let name = |attr: AttrId| world.catalog.attr_name(attr).unwrap_or_default().to_owned();
        let entries = world.journal.entries();
        let tail = &entries[entries.partition_point(|e| e.tx < first)..];
        for entry in tail {
            match &entry.body {
                EntryBody::Fact {
                    entity,
                    attr,
                    value,
                    ..
                }
                | EntryBody::Deleted {
                    entity,
                    attr,
                    value,
                    ..
                }
                | EntryBody::Revised {
                    entity,
                    attr,
                    value,
                    ..
                } => {
                    if let EntryBody::Deleted { of, .. } | EntryBody::Revised { of, .. } =
                        &entry.body
                        && let Some((at, named, _)) = self.entries.get(of)
                    {
                        return conflict(ConflictKey::Entry { entry: *named }, *at);
                    }
                    if let Some(at) = self.attrs.get(attr) {
                        return conflict(ConflictKey::Attribute { attr: name(*attr) }, *at);
                    }
                    if let Some(at) = self.entity_attr.get(&(*entity, *attr)) {
                        return conflict(
                            ConflictKey::EntityAttribute {
                                entity: *entity,
                                attr: name(*attr),
                            },
                            *at,
                        );
                    }
                    if let Value::Ref(target) = value
                        && let Some(at) = self.reverse.get(&(*target, *attr))
                    {
                        return conflict(
                            ConflictKey::Reverse {
                                target: *target,
                                attr: name(*attr),
                            },
                            *at,
                        );
                    }
                    if let Some(values) = self.values.get(attr)
                        && let Ok(encoded) = value.encode()
                        && let Some(at) = values.get(&encoded)
                    {
                        return conflict(ConflictKey::Value { attr: name(*attr) }, *at);
                    }
                }
                EntryBody::DefineEntity { id, handle } => {
                    if let Some(at) = self.any_define {
                        return conflict(ConflictKey::AnyDefine, at);
                    }
                    if let Some(at) = self.types.get(&id.node_type()) {
                        let node_type = world.catalog.node_types[usize::from(id.node_type().0)]
                            .name
                            .clone();
                        return conflict(ConflictKey::TypeScan { node_type }, *at);
                    }
                    if let Some(handle) = handle
                        && let Some(at) = self.handles.get(handle)
                    {
                        return conflict(
                            ConflictKey::Handle {
                                handle: handle.clone(),
                            },
                            *at,
                        );
                    }
                }
                // A schema edit of an attribute (a rename, a relabel, a
                // deprecation, a migration) changes what a read of it
                // meant: every key on the attribute conflicts.
                EntryBody::Schema(SchemaEdit::Attribute { id, .. })
                | EntryBody::Schema(SchemaEdit::Migration { source: id, .. })
                | EntryBody::DefineAttr { id, .. } => {
                    if let Some(at) = self.attr_read(*id) {
                        return conflict(ConflictKey::Attribute { attr: name(*id) }, at);
                    }
                }
                EntryBody::Schema(_)
                | EntryBody::DefineNodeType { .. }
                | EntryBody::DefineEdge { .. }
                | EntryBody::Commit { .. } => {}
            }
        }
        // Names the committed schema lacked when the transaction began:
        // declared by a newer commit?
        for (attr, read) in &self.new_attrs {
            let key = match world.catalog.attr_by_name.get(attr) {
                Some(id) => {
                    let committed = &world.catalog.attributes[id.0 as usize];
                    read.conflict(attr, *id, committed, tail)
                }
                // Taken as another attribute's inverse name.
                None if world.catalog.inverse_by_name.contains_key(attr) => {
                    Some(ConflictKey::Undeclared { name: attr.clone() })
                }
                None => None,
            };
            if let Some(key) = key {
                return conflict(key, read.at);
            }
        }
        for (node_type, at) in &self.undeclared_types {
            if world.catalog.node_type_by_name.contains_key(node_type) {
                return conflict(
                    ConflictKey::Undeclared {
                        name: node_type.clone(),
                    },
                    *at,
                );
            }
        }
        Ok(())
    }

    /// The earliest read of anything on `attr`, exact keys included.
    fn attr_read(&self, attr: AttrId) -> Option<usize> {
        let exact = self
            .entity_attr
            .iter()
            .chain(&self.reverse)
            .filter(|((_, a), _)| *a == attr)
            .map(|(_, at)| *at);
        let values = self
            .values
            .get(&attr)
            .into_iter()
            .flat_map(|values| values.values().copied());
        let entries = self
            .entries
            .values()
            .filter(|(_, _, a)| *a == attr)
            .map(|(at, _, _)| *at);
        exact
            .chain(values)
            .chain(entries)
            .chain(self.attrs.get(&attr).copied())
            .min()
    }
}

/// Recording: the operations a transaction makes and the keys it reads.
impl Txn<'_> {
    /// Sets how this transaction's commit treats transactions committed
    /// after it began (see [`Concurrency`]; the default is
    /// [`Concurrency::Optimistic`]). Choose before the transaction reads:
    /// under [`Concurrency::Weak`] reads aren't recorded, so going back to
    /// optimistic after a read was made that way counts as having read
    /// everything (any newer commit then conflicts). Reads recorded under
    /// optimistic mode are kept across a switch to weak (a weak commit
    /// doesn't check them) and count again, exactly, after a switch back.
    /// A transaction that has read nothing yet changes mode freely.
    pub fn set_concurrency(&mut self, mode: Concurrency) {
        // Only reads that went unrecorded can't be vouched for.
        if self.s.concurrency == Concurrency::Weak
            && mode == Concurrency::Optimistic
            && self.s.unrecorded_reads.get()
        {
            let at = self.s.ops.len();
            self.s.reads.borrow_mut().read_all(at);
        }
        self.s.concurrency = mode;
    }

    /// This transaction's concurrency mode.
    pub fn concurrency(&self) -> Concurrency {
        self.s.concurrency
    }

    /// Says whether IDs this transaction allocated (created entities,
    /// interned edges) have already been shown to someone who can't be
    /// handed the commit's [`IdMap`] (a server that returned a created node
    /// before `COMMIT`). When set, a commit that would renumber such an ID,
    /// or settle it on an entity or edge a newer commit defined, fails with
    /// [`Error::Conflict`] ([`ConflictKey::ExposedId`]) instead, in either
    /// concurrency mode. Off by default.
    pub fn set_ids_exposed(&mut self, exposed: bool) {
        self.s.ids_exposed = exposed;
    }

    /// Whether [`Txn::set_ids_exposed`] is set.
    pub fn ids_exposed(&self) -> bool {
        self.s.ids_exposed
    }

    /// The latest committed tx of the world this transaction began on: its
    /// snapshot. A caller that read the world before [`World::begin`] can
    /// tell from it whether `begin` caught up with newer commits first.
    pub fn snapshot_tx(&self) -> TxId {
        TxId(self.s.tx.0 - 1)
    }

    /// How many write operations (creates, asserts, retracts, corrections,
    /// schema operations; no-ops included) the transaction has made: the
    /// index the next one gets in [`Finalized::ops`] and
    /// [`Conflict::operation`]. Counted only by a transaction on a
    /// file-backed world that didn't take the lock when it began; else 0.
    pub fn operation_count(&self) -> usize {
        self.s.ops.len()
    }

    /// Records that the transaction read `attr` of `entity` (its values or
    /// history at any tick): under optimistic concurrency the commit then
    /// conflicts with any newer entry on that entity and attribute. An
    /// inverse name records the reverse read of its base attribute; a
    /// bidirectional attribute is recorded on both sides, since its pair is
    /// stored once. [`Txn::values_at`] records this itself.
    ///
    /// Like every `note_*` method: fails with [`Error::UnknownAttribute`]
    /// (or [`Error::UnknownEntity`], [`Error::UnknownNodeType`]) without
    /// poisoning the transaction, and does nothing under
    /// [`Concurrency::Weak`]. A name that isn't declared is still a read,
    /// of its absence: if the transaction goes on to commit, a newer
    /// commit that declared the name conflicts
    /// ([`ConflictKey::Undeclared`]).
    pub fn note_read(&self, entity: EntityId, attr: &str) -> Result<(), Error> {
        let resolved = self
            .world
            .catalog
            .resolve_neighbor_attr(attr, Direction::Out);
        let (attr_id, dir) = self.or_absent(attr, resolved)?;
        self.note_side(entity, attr_id, attr, dir.reads_out())
    }

    /// Records a read through the reverse index: who has `target` as their
    /// `attr` value ("who is in Varrow"). The commit then conflicts with
    /// any newer entry whose value is `target` under `attr`. See
    /// [`Txn::note_read`] for names, errors and bidirectional attributes.
    pub fn note_reverse_read(&self, target: EntityId, attr: &str) -> Result<(), Error> {
        let resolved = self
            .world
            .catalog
            .resolve_neighbor_attr(attr, Direction::In);
        let (attr_id, dir) = self.or_absent(attr, resolved)?;
        self.note_side(target, attr_id, attr, dir.reads_out())
    }

    /// Records a lookup by value: which entities have `value` for `attr`.
    /// The commit then conflicts with any newer entry with that attribute
    /// and value. A `ref` value is the reverse read of its target; a value
    /// with a long encoding is recorded as a read of the whole attribute.
    pub fn note_value_read(&self, attr: &str, value: &Value) -> Result<(), Error> {
        if let Value::Ref(target) = value {
            return self.note_reverse_read(*target, attr);
        }
        let attr_id = self.or_absent(attr, self.attr_id(attr))?;
        let encoded = value.encode()?;
        let Some(mut reads) = self.reads() else {
            return Ok(());
        };
        let at = self.s.ops.len();
        if self.provisional_attr(attr_id) {
            self.note_new_attr(&mut reads, attr_id, NewAttrKey::Whole);
        } else {
            reads.read_value(attr_id, encoded, at);
        }
        Ok(())
    }

    /// Records a scan of every entity of `node_type`: the commit then
    /// conflicts with a newer commit that created an entity of that type
    /// (a phantom).
    pub fn note_type_scan(&self, node_type: &str) -> Result<(), Error> {
        let id = self.world.catalog.node_type_by_name.get(node_type).copied();
        let Some(mut reads) = self.reads() else {
            return id
                .map(|_| ())
                .ok_or_else(|| Error::UnknownNodeType(node_type.to_owned()));
        };
        let at = self.s.ops.len();
        let Some(id) = id else {
            // "No such type" is a read of its absence (no entities of it):
            // a newer declaration of the name invalidates it.
            reads
                .undeclared_types
                .entry(node_type.to_owned())
                .or_insert(at);
            return Err(Error::UnknownNodeType(node_type.to_owned()));
        };
        if usize::from(id.0) >= self.s.snapshot.node_types {
            reads
                .undeclared_types
                .entry(node_type.to_owned())
                .or_insert(at);
        } else {
            reads.read_type(id, at);
        }
        Ok(())
    }

    /// Records a read of everything stored under `attr` (a scan of the
    /// attribute): the commit then conflicts with any newer entry or schema
    /// change on it. An inverse name stands for its base attribute.
    pub fn note_read_attribute(&self, attr: &str) -> Result<(), Error> {
        let attr_id = self.or_absent(attr, self.world.catalog.attr_or_inverse_id(attr))?;
        let Some(mut reads) = self.reads() else {
            return Ok(());
        };
        let at = self.s.ops.len();
        if self.provisional_attr(attr_id) {
            self.note_new_attr(&mut reads, attr_id, NewAttrKey::Whole);
        } else {
            reads.read_attr(attr_id, at);
        }
        Ok(())
    }

    /// Records a scan of entity definitions or the handle table, including
    /// future node types. Fact-only commits do not invalidate this read.
    pub fn note_entity_definitions(&self) {
        if let Some(mut reads) = self.reads() {
            reads.any_define.get_or_insert(self.s.ops.len());
        }
    }

    /// Records that the transaction's writes depend on the whole world:
    /// any transaction committed since this one began invalidates this read.
    pub fn note_read_all(&self) {
        if let Some(mut reads) = self.reads() {
            reads.read_all(self.s.ops.len());
        }
    }

    /// The read set, if this transaction records reads: optimistic, on a
    /// world other handles can write. Every read asks here, so a read made
    /// under [`Concurrency::Weak`] is remembered as one that went
    /// unrecorded (see [`Txn::set_concurrency`]).
    fn reads(&self) -> Option<RefMut<'_, ReadSet>> {
        if !self.s.track {
            return None;
        }
        if self.s.concurrency == Concurrency::Weak {
            self.s.unrecorded_reads.set(true);
            return None;
        }
        Some(self.s.reads.borrow_mut())
    }

    /// Whether the committed schema this transaction began on has `name`,
    /// as an attribute's name or its inverse name. The transaction's own
    /// schema edits (a rename away, a new declaration) don't count.
    fn declared_at_snapshot(&self, name: &str) -> bool {
        match &self.s.schema_before {
            Some(before) => before.declares(name),
            None => {
                self.world.catalog.attr_by_name.contains_key(name)
                    || self.world.catalog.inverse_by_name.contains_key(name)
            }
        }
    }

    /// Records a read under `attr`, an attribute this transaction declared
    /// itself. Nobody else wrote an attribute that didn't exist, unless a
    /// newer commit declared the same name: recorded only if the committed
    /// schema had no such name when the transaction began (a name the
    /// transaction freed by renaming its attribute away and then declared
    /// anew was never "found undeclared" by anyone).
    fn note_new_attr(&self, reads: &mut ReadSet, attr: AttrId, key: NewAttrKey) {
        let def = &self.world.catalog.attributes[attr.0 as usize];
        if self.declared_at_snapshot(&def.name) {
            return;
        }
        reads
            .new_attrs
            .entry(def.name.clone())
            .or_insert_with(|| NewAttrRead {
                at: self.s.ops.len(),
                def: Some(def.clone()),
                whole: false,
                forward: HashSet::new(),
                reverse: HashSet::new(),
            })
            .add(key);
    }

    fn provisional_attr(&self, attr: AttrId) -> bool {
        attr.0 as usize >= self.s.snapshot.attrs
    }

    /// Whether this transaction allocated `id` (an entity it created or an
    /// edge it interned).
    fn provisional(&self, id: EntityId) -> bool {
        match id.as_edge() {
            Some(edge) => edge.seq() > self.s.snapshot.edges as u64,
            None => self.s.ids.created(id),
        }
    }

    /// Records a read of `attr` (resolved, written as `name`) on one side
    /// of `entity`: its values (`forward`) or who refers to it.
    fn note_side(
        &self,
        entity: EntityId,
        attr: AttrId,
        name: &str,
        forward: bool,
    ) -> Result<(), Error> {
        if !self.world.catalog.is_defined_staged(&self.s.ids, entity) {
            return Err(Error::UnknownEntity(entity));
        }
        self.note_key(entity, attr, name, forward);
        Ok(())
    }

    /// [`Txn::note_side`] for an entity known to be defined.
    pub(super) fn note_key(&self, entity: EntityId, attr: AttrId, name: &str, forward: bool) {
        let Some(mut reads) = self.reads() else {
            return;
        };
        let at = self.s.ops.len();
        if self.provisional(entity) {
            // Nobody else wrote an entity only this transaction knows,
            // unless it is an edge a newer commit interned too: then the
            // commit merges the two, and the newer commit may have written
            // the edge's properties, under an attribute this transaction
            // declared as well (both inferred the same definition). An
            // entity that settles on a newer commit's (find-or-create) is
            // caught by the free handle it read.
            if entity.is_edge() {
                reads
                    .provisional
                    .entry(entity)
                    .or_insert_with(|| (at, name.to_owned()));
            }
            return;
        }
        if self.provisional_attr(attr) {
            // Nobody else wrote an attribute that didn't exist, unless a
            // newer commit declared the same name.
            let both = self.world.catalog.is_bidirectional(attr);
            if forward || both {
                self.note_new_attr(&mut reads, attr, NewAttrKey::Forward(entity));
            }
            if !forward || both {
                self.note_new_attr(&mut reads, attr, NewAttrKey::Reverse(entity));
            }
            return;
        }
        let both = self.world.catalog.is_bidirectional(attr);
        if forward || both {
            reads.read_entity_attr(entity, attr, at);
        }
        if !forward || both {
            reads.read_reverse(entity, attr, at);
        }
    }

    /// Passes `resolved` (attribute `name`, looked up for a read) through.
    /// A lookup that failed because the name is undeclared is a read of
    /// its absence, which a caller may decide from ("no such attribute:
    /// nothing there"): recorded, so a newer declaration of the name
    /// conflicts.
    pub(super) fn or_absent<T>(&self, name: &str, resolved: Result<T, Error>) -> Result<T, Error> {
        if matches!(resolved, Err(Error::UnknownAttribute(_))) {
            self.note_undeclared_attr(name);
        }
        resolved
    }

    /// Records that `handle` was found free.
    pub(super) fn note_free_handle(&self, handle: &str) {
        if let Some(mut reads) = self.reads() {
            reads.read_handle(handle, self.s.ops.len());
        }
    }

    /// Records that attribute `name` was found undeclared: a read of its
    /// absence, which any newer declaration of the name invalidates.
    /// Nothing is recorded for a name the committed schema had when the
    /// transaction began (it is undeclared here only because the
    /// transaction itself renamed it away, which no newer commit can
    /// change).
    pub(super) fn note_undeclared_attr(&self, name: &str) {
        let Some(mut reads) = self.reads() else {
            return;
        };
        if self.declared_at_snapshot(name) {
            return;
        }
        let at = self.s.ops.len();
        let read = reads
            .new_attrs
            .entry(name.to_owned())
            .or_insert_with(|| NewAttrRead {
                at,
                def: None,
                whole: true,
                forward: HashSet::new(),
                reverse: HashSet::new(),
            });
        read.def = None;
        read.whole = true;
    }

    /// Records a read of the revision chain of base entry `base` (of
    /// attribute `attr`), which the caller named as `named`.
    pub(super) fn note_chain(&self, base: EntryId, named: EntryId, attr: AttrId) {
        if let Some(mut reads) = self.reads() {
            reads.read_entry(base, named, attr, self.s.ops.len());
        }
    }

    /// Notes that a write with no tick of its own or the transaction's
    /// landed on the latest tick the transaction began with.
    pub(super) fn note_base_tick(&self) {
        self.s.used_base_tick.set(true);
    }

    /// Runs one public write operation `f` and, if it succeeds, records it
    /// (as `call` describes it) for [`Txn::finalize`], with the edges it
    /// interned. An operation made inside another isn't recorded: the outer
    /// one is made again as a whole. Nothing is recorded by a transaction
    /// that can't find the world changed at commit (in memory, or holding
    /// the lock since it began), so `call` isn't run and nothing is cloned.
    pub(super) fn recorded<T>(
        &mut self,
        call: impl FnOnce(&T) -> Call,
        f: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if !self.s.track || self.s.nested {
            return f(self);
        }
        let edges_before = self.s.ids.edges.len();
        self.s.nested = true;
        let result = f(self);
        self.s.nested = false;
        if let Ok(value) = &result {
            let edges = self.s.ids.edges[edges_before..]
                .iter()
                .map(|e| {
                    let attr = self.world.catalog.attributes[e.attr.0 as usize]
                        .name
                        .clone();
                    (e.id, e.source, attr, e.target)
                })
                .collect();
            self.s.ops.push(Recorded {
                call: call(value),
                edges,
            });
        }
        result
    }
}

/// Committing over newer transactions.
impl<'w> Txn<'w> {
    /// What a commit does when `applied` (> 0) transactions were appended
    /// to the log since this transaction began. It holds the writer lock
    /// `held` (released on every path out of here), the world is caught up
    /// with those transactions and has its committed schema, and
    /// everything this transaction staged is relative to the world it
    /// began on, so none of it is used: the recorded operations are.
    ///
    /// Optimistic mode first checks the read set against the new entries.
    /// Then the operations are made again, in order, in a new transaction
    /// on the caught-up world that holds the lock: exactly what a caller
    /// making them now would stage, except that tick-less writes keep the
    /// latest tick this transaction began with (every write was recorded
    /// with the tick it resolved to). IDs this transaction allocated are
    /// translated as the operations that allocated them are made again. An
    /// operation that now fails returns its ordinary error; nothing is
    /// written then.
    pub(super) fn finalize(
        mut self,
        held: WriterLock,
        applied: usize,
        warn: bool,
    ) -> Result<CommitResult, Error> {
        let conflict = |key: ConflictKey, operation: usize| {
            Error::Conflict(Conflict {
                key,
                operation,
                applied,
            })
        };
        let reads = std::mem::take(&mut *self.s.reads.borrow_mut());
        let optimistic = self.s.concurrency == Concurrency::Optimistic;
        if optimistic {
            reads.check(self.world, self.s.tx, applied)?;
        }
        let ops = std::mem::take(&mut self.s.ops);
        let exposed = self.s.ids_exposed;
        let base_tick = self.s.base_tick;
        let moved_to = self.world.latest_tick();
        let tick_moved = (self.s.used_base_tick.get()
            && moved_to != base_tick
            && ops.iter().any(|r| {
                matches!(
                    r.call,
                    Call::Write { tick, .. } | Call::InferredWrite { tick, .. }
                        if tick == base_tick
                )
            }))
        .then_some(TickMoved {
            from: base_tick,
            to: moved_to,
        });
        // The edges every handle knows now: one found among them for a
        // triple this transaction interned is a newer commit's.
        let committed_edges = self.world.catalog.edges.len() as u64;

        let deadline = self.s.deadline;
        let mut fresh = self.world.begin_without_catch_up();
        fresh.s.deadline = deadline;
        fresh.s.held = Some(held);
        fresh.s.track = false;
        fresh.s.base_tick = base_tick;
        // Provisional ID -> final ID, for every ID this transaction
        // allocated (edges as entity IDs).
        let mut map: HashMap<EntityId, EntityId> = HashMap::new();
        let mut ids = IdMap::default();
        let mut outcomes = Vec::with_capacity(ops.len());
        for (index, recorded) in ops.iter().enumerate() {
            fresh.check_deadline()?;
            let translate = |id: EntityId| map.get(&id).copied().unwrap_or(id);
            let value_of = |v: &Value| match v {
                Value::Ref(id) => Value::Ref(translate(*id)),
                other => other.clone(),
            };
            let entries_before = fresh.s.entries.len();
            let schema_before = fresh.s.schema_changes.len();
            let mut outcome = FinalizedOp {
                changed: 0,
                declared: 0,
                created: None,
                precision: None,
            };
            // The entity an operation allocated: its provisional ID, the
            // final one, and whether that is an entity another commit made.
            let mut allocated = None;
            match &recorded.call {
                Call::Create {
                    node_type,
                    handle,
                    id,
                } => {
                    let fin = fresh.create_entity(node_type, handle.as_deref())?;
                    allocated = Some((*id, fin, false));
                }
                Call::FindOrCreate {
                    node_type,
                    handle,
                    id,
                    created,
                } => {
                    let (fin, created_now) = fresh.find_or_create_entity(node_type, handle)?;
                    outcome.created = Some(created_now);
                    if *created {
                        allocated = Some((*id, fin, !created_now));
                    }
                }
                Call::Write {
                    entity,
                    attr,
                    value,
                    tick,
                    op,
                    precision,
                } => {
                    let written = fresh.write(
                        translate(*entity),
                        attr,
                        &value_of(value),
                        Some(*tick),
                        *op,
                        *precision,
                    )?;
                    outcome.precision = (written.changed > 0).then_some(*precision);
                }
                Call::Correct {
                    entry,
                    correction,
                    chain,
                } => {
                    // The correction was decided on the chain's current
                    // fact as the transaction saw it. If a newer commit
                    // corrected the chain since (optimistic mode has
                    // already conflicted on it), making it again would
                    // stack a revision on a fact the caller never saw, or
                    // override the other writer's correction: refused.
                    if let Some((base, rev)) = chain {
                        let now = fresh.world.journal.chain(*base).rev;
                        if now != *rev {
                            return Err(Error::AlreadyCorrected {
                                entry: *entry,
                                rev: now,
                            });
                        }
                    }
                    let correction = match correction {
                        Correction::Replace {
                            value,
                            tick,
                            op,
                            precision,
                        } => Correction::Replace {
                            value: value.as_ref().map(value_of),
                            tick: *tick,
                            op: *op,
                            precision: *precision,
                        },
                        Correction::Delete => Correction::Delete,
                    };
                    fresh.correct(*entry, correction)?;
                }
                Call::InferWriteAttribute { name, value, op } => {
                    fresh.infer_write_attribute(name, &value_of(value), *op)?;
                }
                // Made again as a whole: a retract that was dropped because
                // the attribute didn't exist is judged against the
                // attribute a newer commit declared, as one writer's would.
                Call::InferredWrite {
                    entity,
                    attr,
                    value,
                    tick,
                    op,
                    precision,
                } => {
                    let written = fresh.write_inferring(
                        translate(*entity),
                        attr,
                        &value_of(value),
                        Some(*tick),
                        *op,
                        *precision,
                    )?;
                    outcome.precision =
                        written.is_some_and(|w| w.changed > 0).then_some(*precision);
                }
                Call::InferAttribute { name, value } => {
                    fresh.infer_attribute(name, &value_of(value))?;
                }
                Call::InferAttributeWith {
                    name,
                    value_type,
                    options,
                } => fresh.infer_attribute_with(name, *value_type, options)?,
                Call::InferNodeType(name) => fresh.infer_node_type(name)?,
                Call::DeclareAttribute(def) => fresh.declare_attribute(def.clone())?,
                Call::DeclareAttributeWith {
                    name,
                    value_type,
                    options,
                } => fresh.declare_attribute_with(name, *value_type, options)?,
                Call::DeclareNodeType(name) => fresh.declare_node_type(name)?,
                Call::Rename { old, new } => fresh.rename_attribute(old, new)?,
                Call::Deprecate(name) => fresh.deprecate_attribute(name)?,
                Call::SetCardinality { name, cardinality } => {
                    fresh.set_cardinality(name, *cardinality)?;
                }
                Call::WorldSetting { key, value } => fresh.world_setting(key, value)?,
                Call::Migrate { name, options } => {
                    fresh.migrate_attribute(name, options.clone())?;
                }
            }
            if let Some((provisional, fin, merged)) = allocated {
                if exposed && (merged || fin != provisional) {
                    return Err(conflict(ConflictKey::ExposedId { id: provisional }, index));
                }
                map.insert(provisional, fin);
                if fin != provisional {
                    ids.entities.push((provisional, fin));
                }
            }
            for (provisional, source, attr, target) in &recorded.edges {
                let as_entity = EntityId::from(*provisional);
                let translate = |id: EntityId| map.get(&id).copied().unwrap_or(id);
                // The same operation, made again, stored or found the same
                // triple, so its edge exists now. If it somehow doesn't,
                // nothing later can name it: refuse rather than guess.
                let Some(fin) = fresh.edge_id(translate(*source), attr, translate(*target)) else {
                    return Err(conflict(ConflictKey::ExposedId { id: as_entity }, index));
                };
                let merged = fin.seq() <= committed_edges;
                if exposed && (merged || fin != *provisional) {
                    return Err(conflict(ConflictKey::ExposedId { id: as_entity }, index));
                }
                // The transaction read this edge's properties (or what
                // refers to it) as a new edge's: empty. It isn't new.
                if optimistic
                    && merged
                    && let Some((at, attr)) = reads.read_provisional(as_entity)
                {
                    // Named as the committed edge the triple turned out
                    // to be: the provisional number means nothing any
                    // more, or names another committed edge by now.
                    let key = ConflictKey::EntityAttribute {
                        entity: EntityId::from(fin),
                        attr: attr.clone(),
                    };
                    return Err(conflict(key, *at));
                }
                map.insert(as_entity, EntityId::from(fin));
                if fin != *provisional {
                    ids.edges.push((*provisional, fin));
                }
            }
            outcome.changed = fresh.s.entries.len() - entries_before;
            outcome.declared = fresh.s.schema_changes.len() - schema_before;
            outcomes.push(outcome);
        }
        let mut result = fresh.commit_with(warn)?;
        result.ids = ids;
        result.latest_tick_moved = tick_moved;
        result.finalized = Some(Finalized {
            applied,
            ops: outcomes,
        });
        Ok(result)
    }
}
