//! Identity: node types, entities, handles and interned edges.

use super::*;

/// One declared node type and its sequence counter.
#[derive(Debug)]
pub(super) struct NodeType {
    pub(super) name: String,
    /// The tx that defined each entity of this type: entry `i` is the
    /// entity with sequence `i + 1` (sequences are assigned without gaps,
    /// so the length is the last sequence, 0 = none). Non-decreasing,
    /// because entities are defined in tx order: the entities visible at a
    /// canon are a prefix.
    pub(super) defined_in: Vec<TxId>,
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
pub(super) struct EntityDef {
    pub(super) id: EntityId,
    pub(super) handle: Option<String>,
}

/// The `(source, attr, target)` triple an edge is interned under.
pub(super) type Triple = (EntityId, AttrId, EntityId);

/// A validated edge definition, ready to apply: the payload of the log's
/// `DefineEdge` entry.
#[derive(Debug)]
pub(super) struct EdgeDef {
    pub(super) id: EdgeId,
    pub(super) source: EntityId,
    pub(super) attr: AttrId,
    pub(super) target: EntityId,
}

impl EdgeDef {
    fn triple(&self) -> Triple {
        (self.source, self.attr, self.target)
    }
}

/// IDs (entities and edges) a transaction has defined but not committed.
/// Defining one only stages it; the world's ID tables change on commit.
#[derive(Debug, Default)]
pub(super) struct StagedIds {
    /// In creation order: the order of their `DEFINE` entries.
    pub(super) defs: Vec<EntityDef>,
    /// Last staged sequence number per node type. Staged sequences of a type
    /// continue the world's without gaps, so `1..=last` are all defined.
    last_seq: HashMap<NodeTypeId, u64>,
    handles: HashSet<String>,
    /// Edges interned by this transaction, in interning order: their
    /// sequences continue the world's without gaps.
    pub(super) edges: Vec<EdgeDef>,
    edge_by_triple: HashMap<Triple, EdgeId>,
    /// Unindexed string values this transaction's staged entries use that
    /// the world's value table doesn't hold yet, with the references they
    /// will get: they enter the world's table only when the transaction
    /// commits, so a dropped or poisoned one leaves nothing behind.
    pub(super) values: ValueTable,
}

impl StagedIds {
    pub(super) fn push(&mut self, def: EntityDef) {
        self.last_seq.insert(def.id.node_type(), def.id.seq());
        if let Some(h) = &def.handle {
            self.handles.insert(h.clone());
        }
        self.defs.push(def);
    }

    pub(super) fn push_edge(&mut self, def: EdgeDef) {
        self.edge_by_triple.insert(def.triple(), def.id);
        self.edges.push(def);
    }
}

/// The edge of a stored `ref` pair `(source, attr, target)` of `world`.
pub(super) fn edge_or_panic(
    world: &World,
    attr: AttrId,
    source: EntityId,
    target: EntityId,
) -> EdgeId {
    world
        .catalog
        .edge_of(&StagedIds::default(), source, attr, target)
        .expect("invariant: every stored ref fact's triple has an interned edge")
}

/// The identity half of the catalog: which entity and edge IDs are defined,
/// and how new ones are validated and applied.
impl Catalog {
    /// Every defined entity and edge ID, ascending: entities by node type
    /// then sequence (the type is the ID's high bits), then edges (tagged
    /// with the ID's top bit).
    pub(super) fn defined_ids(&self) -> impl Iterator<Item = EntityId> + '_ {
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

    /// Whether `id` names an entity or edge defined in this world or staged
    /// in `staged` by the transaction asking.
    pub(super) fn is_defined_staged(&self, staged: &StagedIds, id: EntityId) -> bool {
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
    pub(super) fn last_edge_seq(&self, staged: &StagedIds) -> u64 {
        // A Vec's length always fits a u64.
        (self.edges.len() + staged.edges.len()) as u64
    }

    /// Whether `attr` is a bidirectional `ref` attribute.
    pub(super) fn is_bidirectional(&self, attr: AttrId) -> bool {
        self.attributes[attr.0 as usize].direction == Some(RefDirection::Bidirectional)
    }

    /// The order a `ref` pair `(source, target)` of `attr` is stored and
    /// interned in: `(min, max)` by entity ID for a bidirectional attribute
    /// (one edge, one history, whichever spelling was written), as given
    /// for a directed one.
    pub(super) fn stored_pair(
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
    pub(super) fn edge_of(
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
    pub(super) fn prepare_edge(
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

    /// Interns edge `def`, defined by (committed or replayed) transaction
    /// `tx`. Infallible.
    pub(super) fn apply_edge(&mut self, def: EdgeDef, tx: TxId) {
        debug_assert_eq!(
            def.id.seq(),
            self.edges.len() as u64 + 1,
            "invariant: edges are applied in sequence order"
        );
        self.edge_by_triple.insert(def.triple(), def.id);
        self.edges.push(def.triple());
        self.edges_defined_in.push(tx);
    }

    /// The triple `(source, attribute name, target)` of a defined edge (see
    /// [`World::edge`]).
    fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
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

    /// Validates a create without changing the world, counting the
    /// entities and handles `staged` by the creating transaction.
    pub(super) fn prepare_entity(
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

    /// Applies a definition produced by [`Catalog::prepare_entity`], as
    /// part of the commit (or replay) of transaction `tx`. Infallible.
    pub(super) fn apply_entity(&mut self, def: EntityDef, tx: TxId) {
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

    /// Parses `TYPE#seq` or `edge#seq` (see [`World::parse_id`]).
    fn parse_id(&self, s: &str) -> Option<EntityId> {
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

    /// Formats an ID as `TYPE#seq` or `edge#seq` (see [`World::format_id`]).
    fn format_id(&self, id: EntityId) -> String {
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

impl World {
    /// Whether the entity or edge `id` is defined at the latest canon (its
    /// `DEFINE` is committed). Same as [`View::is_defined`] on
    /// `at_canon(latest_tx())`. Constant time: sequences are assigned without
    /// gaps, so no walk over the type's entities is needed.
    pub fn is_defined(&self, id: EntityId) -> bool {
        self.catalog.is_defined_staged(&StagedIds::default(), id)
    }

    /// The edge interned for `(source, attr, target)`, if any: the ID the
    /// first stored assert of that `ref` fact gave it. `None` for a triple
    /// never asserted, an unknown attribute, an inverse name (inverse names
    /// are only read by `neighbors`), a non-ref attribute or an undefined
    /// entity; never an error.
    pub fn edge_id(&self, source: EntityId, attr: &str, target: EntityId) -> Option<EdgeId> {
        let attr = *self.catalog.attr_by_name.get(attr)?;
        self.catalog
            .edge_of(&StagedIds::default(), source, attr, target)
    }

    /// The triple `(source, attribute name, target)` of a defined edge, or
    /// `None` if `id` isn't defined in this world.
    pub fn edge(&self, id: EdgeId) -> Option<(EntityId, String, EntityId)> {
        self.catalog.edge(id)
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

    /// The entity with this handle, if any. Handles are matched exactly.
    pub fn entity_by_handle(&self, handle: &str) -> Option<EntityId> {
        self.catalog.entity_by_handle.get(handle).copied()
    }

    /// The handle of entity `id`, if it was created with one: the inverse
    /// of [`World::entity_by_handle`]. `None` for an entity without a
    /// handle, an edge or an ID that isn't defined in this world.
    pub fn handle_of(&self, id: EntityId) -> Option<&str> {
        self.catalog.handle_by_entity.get(&id).map(String::as_str)
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
        self.catalog.parse_id(s)
    }

    /// Formats an ID as `TYPE#seq` (`CHARACTER#1`), or an edge ID as
    /// `edge#seq` (`edge#9`).
    ///
    /// An ID whose node type isn't in this world's schema (e.g. decoded from
    /// foreign bytes) is shown as `#<type id>#seq` rather than failing; an
    /// edge-tagged ID is shown as `edge#seq` whether or not it is defined.
    pub fn format_id(&self, id: EntityId) -> String {
        self.catalog.format_id(id)
    }
}
