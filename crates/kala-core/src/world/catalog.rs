//! The catalog: what a world's names and IDs mean.
//!
//! The struct, its construction from a genesis schema and its name lookups
//! are here; its identity rules (entities, handles, edges) are in `ids` and
//! its schema edits in `schema_ops`.

use super::*;

/// A world's schema and ID tables: node types, attributes (with their
/// history per canon), settings, entity handles and interned edges. Rebuilt
/// from the log's `DEFINE` and schema entries on open; it holds no facts
/// (those are in [`Indexes`]).
#[derive(Debug)]
pub(super) struct Catalog {
    /// Indexed by `NodeTypeId.0`.
    pub(super) node_types: Vec<NodeType>,
    pub(super) node_type_by_name: HashMap<String, NodeTypeId>,
    pub(super) entity_by_handle: HashMap<String, EntityId>,
    /// The reverse of `entity_by_handle`, for [`World::handle_of`].
    pub(super) handle_by_entity: HashMap<EntityId, String>,
    /// Indexed by `AttrId.0`.
    pub(super) attributes: Vec<AttrDef>,
    pub(super) attribute_history: Vec<Vec<(TxId, AttrDef)>>,
    pub(super) node_origins: Vec<Vec<(TxId, bool)>>,
    pub(super) settings: Vec<(TxId, String, String)>,
    pub(super) attr_by_name: HashMap<String, AttrId>,
    /// Inverse name -> the directed attribute it reverses. Not attribute
    /// names: every API except [`View::neighbors`] treats them as unknown
    /// attributes.
    pub(super) inverse_by_name: HashMap<String, AttrId>,
    /// Interned edges: entry `i` is the triple of the edge with seq `i + 1`
    /// (edge sequences are assigned without gaps).
    pub(super) edges: Vec<Triple>,
    /// The transaction that defined each edge, parallel to `edges` (so
    /// ascending): what [`View::edge`] checks against its canon.
    pub(super) edges_defined_in: Vec<TxId>,
    pub(super) edge_by_triple: HashMap<Triple, EdgeId>,
}

/// The name tables of a genesis schema's attributes: attribute name -> ID
/// and inverse name -> ID.
type AttrNames = (HashMap<String, AttrId>, HashMap<String, AttrId>);

impl Catalog {
    /// The catalog of a new world declaring `schema`'s node types and
    /// attributes as its genesis (tx#0): no entities, no edges and no
    /// settings yet (settings are schema edits, applied by the caller).
    /// Fails as [`World::in_memory`] documents.
    pub(super) fn from_schema(schema: &Schema) -> Result<Catalog, Error> {
        let names = schema.node_types();
        let defs = schema.attributes();
        let (node_types, node_type_by_name) = genesis_node_types(names)?;
        let (attr_by_name, inverse_by_name) = genesis_attr_names(defs)?;
        Ok(Catalog {
            node_types,
            node_type_by_name,
            entity_by_handle: HashMap::new(),
            handle_by_entity: HashMap::new(),
            attributes: defs.to_vec(),
            attribute_history: defs.iter().map(|d| vec![(TxId(0), d.clone())]).collect(),
            node_origins: names.iter().map(|_| vec![(TxId(0), false)]).collect(),
            settings: Vec::new(),
            attr_by_name,
            inverse_by_name,
            edges: Vec::new(),
            edges_defined_in: Vec::new(),
            edge_by_triple: HashMap::new(),
        })
    }

    /// Looks up a known attribute and checks the entity is defined: the
    /// validation shared by writes and reads. Reads pass no staged entities.
    pub(super) fn resolve(
        &self,
        staged: &StagedIds,
        entity: EntityId,
        attr: &str,
    ) -> Result<(AttrId, &AttrDef), Error> {
        let attr_id = *self
            .attr_by_name
            .get(attr)
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))?;
        if !self.is_defined_staged(staged, entity) {
            return Err(Error::UnknownEntity(entity));
        }
        Ok((attr_id, &self.attributes[attr_id.0 as usize]))
    }

    /// Resolves a name `neighbors` accepts: an attribute, read in `dir`, or
    /// an inverse name, read as its base attribute with `dir` flipped.
    pub(super) fn resolve_neighbor_attr(
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

    /// The ID a name the planner statistics accept stands for: an
    /// attribute, or an inverse name (its base attribute).
    pub(super) fn attr_or_inverse_id(&self, attr: &str) -> Result<AttrId, Error> {
        self.attr_by_name
            .get(attr)
            .or_else(|| self.inverse_by_name.get(attr))
            .copied()
            .ok_or_else(|| Error::UnknownAttribute(attr.to_owned()))
    }

    /// The declaration of attribute `name`, matched exactly (see
    /// [`World::attribute`]).
    pub(super) fn attribute(&self, name: &str) -> Option<&AttrDef> {
        let id = self.attr_by_name.get(name)?;
        self.attributes.get(id.0 as usize)
    }

    /// The name of attribute `id` (see [`World::attr_name`]).
    pub(super) fn attr_name(&self, id: AttrId) -> Option<&str> {
        let i = usize::try_from(id.0).ok()?;
        self.attributes.get(i).map(|a| a.name.as_str())
    }

    /// Whether `attr`'s entries are mirrored in `V`: it is declared
    /// indexed and isn't a `ref` attribute (`R` is the value index of a
    /// `ref` attribute).
    pub(super) fn value_indexed(&self, attr: AttrId) -> bool {
        let def = &self.attributes[attr.0 as usize];
        def.indexed && def.value_type != ValueType::Ref
    }

    /// Whether `attr`'s `E` keys carry value references: it is an
    /// unindexed string attribute.
    pub(super) fn keys_by_ref(&self, attr: AttrId) -> bool {
        self.attributes
            .get(attr.0 as usize)
            .is_some_and(|def| !def.indexed && def.value_type == ValueType::Str)
    }
}

/// Validates a genesis schema's node type names and numbers them in
/// declaration order.
fn genesis_node_types(
    names: &[String],
) -> Result<(Vec<NodeType>, HashMap<String, NodeTypeId>), Error> {
    let mut node_types = Vec::with_capacity(names.len());
    let mut node_type_by_name = HashMap::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        // IDs are assigned in declaration order, so the first one that
        // doesn't fit is exactly MAX_NODE_TYPE + 1.
        if i > usize::from(MAX_NODE_TYPE) {
            return Err(IdError::NodeTypeOutOfRange(MAX_NODE_TYPE + 1).into());
        }
        schema_ops::check_node_type_name(name)?;
        let id = NodeTypeId(i as u16); // i <= MAX_NODE_TYPE, checked above
        if node_type_by_name.insert(name.clone(), id).is_some() {
            return Err(Error::DuplicateName(name.clone()));
        }
        node_types.push(NodeType {
            name: name.clone(),
            defined_in: Vec::new(),
        });
    }
    Ok((node_types, node_type_by_name))
}

/// Validates a genesis schema's attribute definitions (names, direction,
/// inverse names) and numbers them in declaration order.
fn genesis_attr_names(defs: &[AttrDef]) -> Result<AttrNames, Error> {
    let mut attr_by_name = HashMap::with_capacity(defs.len());
    let mut inverse_by_name = HashMap::new();
    for (i, def) in defs.iter().enumerate() {
        schema_ops::check_name(&def.name)?;
        if def.inferred || def.deprecated {
            return Err(Error::Schema(
                "genesis attributes must be declared and writable".into(),
            ));
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
            schema_ops::check_name(inverse)?;
            // Inverse names share the attribute namespace (in both
            // directions), so a name always means one thing.
            if attr_by_name.contains_key(inverse)
                || inverse_by_name.insert(inverse.clone(), id).is_some()
            {
                return Err(Error::DuplicateName(inverse.clone()));
            }
        }
    }
    Ok((attr_by_name, inverse_by_name))
}
