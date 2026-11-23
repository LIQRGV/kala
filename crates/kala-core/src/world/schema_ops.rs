//! Transactional schema definitions. The mutable borrow hides provisional names;
//! dropping or failing a transaction restores only its schema metadata.
use super::*;

#[derive(Debug)]
pub(super) struct SchemaSnapshot {
    attributes: Vec<AttrDef>,
    history: Vec<Vec<(TxId, AttrDef)>>,
    origins: Vec<Vec<(TxId, bool)>>,
    settings: Vec<(TxId, String, String)>,
    node_count: usize,
}

#[derive(Debug)]
pub(super) enum OrderedEntry {
    Schema(SchemaEdit),
    Entity(usize),
    Edge(usize),
    Fact(usize),
}

pub(super) fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || name.contains('@') || name == "handle" || name == "edge" {
        return Err(Error::InvalidName(name.to_owned()));
    }
    Ok(())
}

impl SchemaSnapshot {
    fn capture(world: &World) -> Self {
        Self {
            attributes: world.attributes.clone(),
            history: world.attribute_history.clone(),
            origins: world.node_origins.clone(),
            settings: world.settings.clone(),
            node_count: world.node_types.len(),
        }
    }
    pub(super) fn restore(self, world: &mut World) {
        world.attributes = self.attributes;
        world.attribute_history = self.history;
        world.node_origins = self.origins;
        world.settings = self.settings;
        world.node_types.truncate(self.node_count);
        world.rebuild_schema_names();
    }
}

impl World {
    fn rebuild_schema_names(&mut self) {
        self.attr_by_name.clear();
        self.inverse_by_name.clear();
        for (i, def) in self.attributes.iter().enumerate() {
            let id = AttrId(i as u32);
            self.attr_by_name.insert(def.name.clone(), id);
            if let Some(name) = &def.inverse {
                self.inverse_by_name.insert(name.clone(), id);
            }
        }
        self.node_type_by_name = self
            .node_types
            .iter()
            .enumerate()
            .map(|(i, n)| (n.name.clone(), NodeTypeId(i as u16)))
            .collect();
    }
    /// The transaction that first defined this attribute.
    pub fn attribute_first_tx(&self, name: &str) -> Option<TxId> {
        let id = self.attr_by_name.get(name)?;
        Some(self.attribute_history[id.0 as usize].first()?.0)
    }
    /// Latest value of a generic setting. Schema inference defaults to `infer`.
    pub fn world_setting(&self, key: &str) -> Option<&str> {
        self.settings
            .iter()
            .rev()
            .find(|(_, k, _)| k == key)
            .map(|(_, _, v)| v.as_str())
            .or_else(|| (key == "schema").then_some("infer"))
    }
    fn validate_attr_edit(&self, id: AttrId, def: &AttrDef, migration: bool) -> Result<(), Error> {
        let index = id.0 as usize;
        if index > self.attributes.len() {
            return Err(Error::Schema("attribute ID is not the next ID".into()));
        }
        let old = self.attributes.get(index);
        let same_name = old.is_some_and(|old| old.name == def.name);
        if !same_name && !migration {
            check_name(&def.name)?;
        }
        if def.name.is_empty() {
            return Err(Error::InvalidName(def.name.clone()));
        }
        check_direction(def)?;
        if self
            .attr_by_name
            .get(&def.name)
            .is_some_and(|other| *other != id)
            || self.inverse_by_name.contains_key(&def.name)
        {
            return Err(Error::DuplicateName(def.name.clone()));
        }
        if let Some(inverse) = &def.inverse {
            check_name(inverse)?;
            if inverse == &def.name
                || self.attr_by_name.contains_key(inverse)
                || self
                    .inverse_by_name
                    .get(inverse)
                    .is_some_and(|other| *other != id)
            {
                return Err(Error::DuplicateName(inverse.clone()));
            }
        }
        if let Some(old) = old {
            if !old.inferred && def.inferred {
                return Err(Error::Schema(
                    "a declared attribute cannot become inferred".into(),
                ));
            }
            let mut renamed = old.clone();
            renamed.name = def.name.clone();
            let mut relabelled = old.clone();
            relabelled.cardinality = def.cardinality;
            if old.deprecated && &renamed != def && &relabelled != def {
                return Err(Error::DeprecatedAttribute(old.name.clone()));
            }
            if old.name != def.name && &renamed != def && !migration {
                return Err(Error::Schema(
                    "rename cannot also change the definition".into(),
                ));
            }
            if !migration {
                let mut deprecated = old.clone();
                deprecated.deprecated = true;
                if old.deprecated != def.deprecated && def != &deprecated {
                    return Err(Error::Schema(
                        "deprecation cannot also change the definition".into(),
                    ));
                }
                if def.inferred {
                    let mut cardinality = old.clone();
                    cardinality.cardinality = def.cardinality;
                    if def != &renamed && def != &deprecated && def != &cardinality {
                        return Err(Error::Schema("an inferred definition can only be renamed, deprecated or relabelled; redeclaration must mark it declared".into()));
                    }
                }
            }
            if structural_change(old, def) && self.attr_counts.contains_key(&id) {
                return Err(migrate_hint(&old.name));
            }
            if old.inverse.is_some() && old.inverse != def.inverse && !migration {
                return Err(Error::Schema(
                    "renaming or removing an inverse is not supported; use migrate".into(),
                ));
            }
        } else {
            if def.deprecated {
                return Err(Error::Schema("a new attribute cannot be deprecated".into()));
            }
            if def.inferred {
                let mut expected = AttrDef::new(
                    &def.name,
                    def.value_type,
                    Kind::State,
                    if def.value_type == ValueType::Ref {
                        Cardinality::Many
                    } else {
                        Cardinality::One
                    },
                );
                expected.inferred = true;
                if self.world_setting("schema") == Some("strict") || *def != expected {
                    return Err(Error::Schema(
                        "invalid inferred attribute definition or inference in strict mode".into(),
                    ));
                }
            }
        }
        Ok(())
    }
    pub(super) fn apply_schema_edit(
        &mut self,
        edit: &SchemaEdit,
        tx: TxId,
        migration: bool,
    ) -> Result<(), Error> {
        match edit {
            SchemaEdit::Attribute { id, def } => {
                self.validate_attr_edit(*id, def, migration)?;
                let i = id.0 as usize;
                if i == self.attributes.len() {
                    self.attributes.push(def.clone());
                    self.attribute_history.push(vec![(tx, def.clone())]);
                } else {
                    if self.attributes[i] == *def {
                        return Err(Error::Schema("a schema no-op is not a log entry".into()));
                    }
                    self.attributes[i] = def.clone();
                    // Names resolve through the latest namespace, including old canons.
                    for (_, old) in &mut self.attribute_history[i] {
                        old.name.clone_from(&def.name);
                    }
                    self.attribute_history[i].push((tx, def.clone()));
                }
                self.rebuild_schema_names();
            }
            SchemaEdit::NodeType { id, name, inferred } => {
                check_name(name)?;
                let i = usize::from(id.0);
                if id.0 > MAX_NODE_TYPE || i > self.node_types.len() {
                    return Err(Error::Schema(
                        "node type ID out of sequence or range".into(),
                    ));
                }
                if i == self.node_types.len() {
                    if *inferred && self.world_setting("schema") == Some("strict") {
                        return Err(Error::Schema(
                            "cannot infer a node type in strict mode".into(),
                        ));
                    }
                    if self.node_type_by_name.contains_key(name) {
                        return Err(Error::DuplicateName(name.clone()));
                    }
                    self.node_types.push(NodeType {
                        name: name.clone(),
                        defined_in: Vec::new(),
                    });
                    self.node_origins.push(vec![(tx, *inferred)]);
                    self.node_type_by_name.insert(name.clone(), *id);
                } else {
                    if self.node_types[i].name != *name
                        || *inferred
                        || !self.node_origins[i].last().is_some_and(|(_, v)| *v)
                    {
                        return Err(Error::Schema("invalid node type redefinition".into()));
                    }
                    self.node_origins[i].push((tx, false));
                }
            }
            SchemaEdit::Setting { key, value } => {
                if key.is_empty()
                    || value.is_empty()
                    || (key == "schema" && value != "strict" && value != "infer")
                {
                    return Err(Error::Schema("schema setting must be :strict or :infer; setting keys and values must be nonempty".into()));
                }
                if self.world_setting(key) == Some(value) {
                    return Err(Error::Schema("a setting no-op is not a log entry".into()));
                }
                self.settings.push((tx, key.clone(), value.clone()));
            }
            SchemaEdit::Migration { .. } => {
                return Err(Error::Schema(
                    "migration must be replayed as a validated copy sequence".into(),
                ));
            }
        }
        Ok(())
    }
}

fn structural_change(a: &AttrDef, b: &AttrDef) -> bool {
    (a.value_type, a.kind, a.direction, a.indexed) != (b.value_type, b.kind, b.direction, b.indexed)
}
fn migrate_hint(name: &str) -> Error {
    Error::Schema(format!(
        "attribute {:?} already has fact entries; use [:migrate :{} …]",
        name, name
    ))
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if let Some(before) = self.schema_before.take() {
            before.restore(self.world);
        }
    }
}

impl Txn<'_> {
    pub(super) fn schema_edit(
        &mut self,
        edit: SchemaEdit,
        report: SchemaChange,
        migration: bool,
    ) -> Result<(), Error> {
        if let SchemaEdit::Attribute { id, def } = &edit
            && let Some(old) = self.world.attributes.get(id.0 as usize)
            && structural_change(old, def)
            && self.entries.iter().any(|e| e.key_and_mark().0.attr == *id)
        {
            return Err(migrate_hint(&old.name));
        }
        if self.schema_before.is_none() {
            self.schema_before = Some(SchemaSnapshot::capture(self.world));
        }
        self.world.apply_schema_edit(&edit, self.tx, migration)?;
        self.order.push(OrderedEntry::Schema(edit));
        self.schema_changes.push(report);
        Ok(())
    }
    /// Shared write inference. An unknown retract in infer mode changes nothing
    /// and returns false; strict mode still requires an explicit declaration.
    pub fn infer_write_attribute(
        &mut self,
        name: &str,
        value: &Value,
        op: Op,
    ) -> Result<bool, Error> {
        if op == Op::Retract
            && self.world.attribute(name).is_none()
            && self.world.world_setting("schema") != Some("strict")
        {
            return self.guard(|_| Ok(false));
        }
        self.infer_attribute(name, value)?;
        Ok(true)
    }
    /// The current staged definition, including earlier operations in this transaction.
    pub fn attribute(&self, name: &str) -> Option<&AttrDef> {
        self.world.attribute(name)
    }
    /// Resolves a handle including entities created earlier in this transaction.
    pub fn entity_by_handle(&self, name: &str) -> Option<EntityId> {
        self.ids
            .defs
            .iter()
            .find(|d| d.handle.as_deref() == Some(name))
            .map(|d| d.id)
            .or_else(|| self.world.entity_by_handle(name))
    }
    /// Parses an entity ID using the staged node type namespace.
    pub fn parse_id(&self, name: &str) -> Option<EntityId> {
        self.world.parse_id(name)
    }
    /// Infers a definition from a first value unless the world is strict.
    pub fn infer_attribute(&mut self, name: &str, value: &Value) -> Result<(), Error> {
        self.guard(|txn| {
            if txn.world.attribute(name).is_some() { return Ok(()); }
            check_name(name)?;
            let ty = value.value_type();
            if txn.world.world_setting("schema") == Some("strict") { return Err(Error::Schema(format!("attribute {:?} is not in the schema; declare it first: [:attribute :{} :{}]", name,name,type_name(ty)))); }
            if let Value::Str(value) = value && value.len() > crate::MAX_INDEXED_STRING_BYTES { return Err(Error::Schema(format!("string over 256 bytes for an indexed attribute; declare it first: [:attribute :{} :string :unindexed]", name))); }
            let mut def = AttrDef::new(name,ty,Kind::State,if ty == ValueType::Ref { Cardinality::Many } else { Cardinality::One });
            def.inferred = true;
            txn.declare_attr_inner(def)
        })
    }
    /// Infers a node type unless the world is strict.
    pub fn infer_node_type(&mut self, name: &str) -> Result<(), Error> {
        self.guard(|txn| {
            if txn.world.node_type_by_name.contains_key(name) {
                return Ok(());
            }
            if txn.world.world_setting("schema") == Some("strict") {
                return Err(Error::Schema(format!(
                    "node type {:?} is not in the schema; declare it first: [:node-type :{}]",
                    name, name
                )));
            }
            txn.node_type_inner(name, true)
        })
    }
    /// Declares or revises an attribute. Origin and deprecation are managed by the store.
    pub fn declare_attribute(&mut self, mut def: AttrDef) -> Result<(), Error> {
        self.guard(|txn| {
            if def.deprecated {
                return Err(Error::Schema(
                    "use deprecate to deprecate an attribute".into(),
                ));
            }
            def.inferred = false;
            txn.declare_attr_inner(def)
        })
    }
    fn declare_attr_inner(&mut self, def: AttrDef) -> Result<(), Error> {
        check_name(&def.name)?;
        let id = match self.world.attr_by_name.get(&def.name) {
            Some(id) => {
                let old = &self.world.attributes[id.0 as usize];
                if old.deprecated {
                    return Err(Error::DeprecatedAttribute(old.name.clone()));
                }
                if old == &def {
                    return Ok(());
                }
                *id
            }
            None => AttrId(
                u32::try_from(self.world.attributes.len())
                    .map_err(|_| Error::InvalidName(def.name.clone()))?,
            ),
        };
        let report = SchemaChange::Attribute {
            name: def.name.clone(),
            value_type: def.value_type,
            inferred: def.inferred,
        };
        self.schema_edit(SchemaEdit::Attribute { id, def }, report, false)
    }
    /// Declares a node type, or confirms an inferred one.
    pub fn declare_node_type(&mut self, name: &str) -> Result<(), Error> {
        self.guard(|txn| txn.node_type_inner(name, false))
    }
    fn node_type_inner(&mut self, name: &str, inferred: bool) -> Result<(), Error> {
        let id = if let Some(id) = self.world.node_type_by_name.get(name) {
            if inferred
                || !self.world.node_origins[usize::from(id.0)]
                    .last()
                    .is_some_and(|(_, i)| *i)
            {
                return Ok(());
            }
            *id
        } else {
            if self.world.node_types.len() > usize::from(MAX_NODE_TYPE) {
                return Err(IdError::NodeTypeOutOfRange(MAX_NODE_TYPE + 1).into());
            }
            NodeTypeId(self.world.node_types.len() as u16)
        };
        self.schema_edit(
            SchemaEdit::NodeType {
                id,
                name: name.into(),
                inferred,
            },
            SchemaChange::NodeType {
                name: name.into(),
                inferred,
            },
            false,
        )
    }
    /// Changes the name at every canon, preserving the attribute ID.
    pub fn rename_attribute(&mut self, old: &str, new: &str) -> Result<(), Error> {
        self.guard(|txn| {
            check_name(new)?;
            let id = txn.attr_id(old)?;
            if old == new {
                return Ok(());
            }
            let mut def = txn.world.attributes[id.0 as usize].clone();
            def.name = new.into();
            txn.schema_edit(
                SchemaEdit::Attribute { id, def },
                SchemaChange::Rename {
                    old: old.into(),
                    new: new.into(),
                },
                false,
            )
        })
    }
    pub(super) fn attr_id(&self, name: &str) -> Result<AttrId, Error> {
        self.world
            .attr_by_name
            .get(name)
            .copied()
            .ok_or_else(|| Error::UnknownAttribute(name.into()))
    }
    /// Refuses subsequent writes while preserving reads.
    pub fn deprecate_attribute(&mut self, name: &str) -> Result<(), Error> {
        self.guard(|txn| {
            let id = txn.attr_id(name)?;
            let mut def = txn.world.attributes[id.0 as usize].clone();
            if def.deprecated {
                return Ok(());
            }
            def.deprecated = true;
            txn.schema_edit(
                SchemaEdit::Attribute { id, def },
                SchemaChange::Deprecate { name: name.into() },
                false,
            )
        })
    }
    /// Revises the cardinality label, with overlap warnings at commit.
    pub fn set_cardinality(&mut self, name: &str, cardinality: Cardinality) -> Result<(), Error> {
        self.guard(|txn| {
            let id = txn.attr_id(name)?;
            let mut def = txn.world.attributes[id.0 as usize].clone();
            let from = def.cardinality;
            if from == cardinality {
                return Ok(());
            }
            def.cardinality = cardinality;
            txn.schema_edit(
                SchemaEdit::Attribute { id, def },
                SchemaChange::Cardinality {
                    name: name.into(),
                    from,
                    to: cardinality,
                },
                false,
            )
        })
    }
    /// Changes a generic world setting, effective for the next operation.
    pub fn world_setting(&mut self, key: &str, value: &str) -> Result<(), Error> {
        self.guard(|txn| {
            if txn.world.world_setting(key) == Some(value) {
                return Ok(());
            }
            txn.schema_edit(
                SchemaEdit::Setting {
                    key: key.into(),
                    value: value.into(),
                },
                SchemaChange::Setting {
                    key: key.into(),
                    value: value.into(),
                },
                false,
            )
        })
    }
}

/// The stable spelling shared by write reports and explicit schema forms.
pub fn type_name(ty: ValueType) -> &'static str {
    match ty {
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::F64 => "f64",
        ValueType::Str => "string",
        ValueType::Ref => "ref",
    }
}

impl<'w> View<'w> {
    pub(super) fn definition(&self, id: AttrId) -> Option<&'w AttrDef> {
        if self.latest {
            return self.world.attributes.get(id.0 as usize);
        }
        let history = self.world.attribute_history.get(id.0 as usize)?;
        let n = history.partition_point(|(tx, _)| *tx <= self.canon);
        n.checked_sub(1).map(|i| &history[i].1)
    }
    pub(super) fn require_definition(&self, id: AttrId) -> Result<&'w AttrDef, Error> {
        self.definition(id)
            .ok_or_else(|| Error::AttributeNotDefined {
                name: self.world.attr_name(id).unwrap_or("?").into(),
                canon: self.canon,
            })
    }
    /// Definition at this canon, resolved through the latest name table.
    pub fn attribute(&self, name: &str) -> Option<&'w AttrDef> {
        self.definition(*self.world.attr_by_name.get(name)?)
    }
    /// Resolves a name or returns a canon-specific error if it is too new.
    pub fn require_attribute(&self, name: &str) -> Result<&'w AttrDef, Error> {
        let id = self
            .world
            .attr_by_name
            .get(name)
            .ok_or_else(|| Error::UnknownAttribute(name.into()))?;
        self.require_definition(*id)
    }
    /// Resolves an attribute or inverse through the latest namespace, then
    /// checks its definition at this canon. The bool identifies an inverse.
    pub fn resolve_attribute(&self, name: &str) -> Result<(&'w AttrDef, bool), Error> {
        let (id, inverse) = if let Some(id) = self.world.attr_by_name.get(name) {
            (*id, false)
        } else if let Some(id) = self.world.inverse_by_name.get(name) {
            (*id, true)
        } else {
            return Err(Error::UnknownAttribute(name.into()));
        };
        let def = self
            .definition(id)
            .filter(|d| !inverse || d.inverse.as_deref() == Some(name))
            .ok_or_else(|| Error::AttributeNotDefined {
                name: name.into(),
                canon: self.canon,
            })?;
        Ok((def, inverse))
    }
    /// Every attribute defined at this canon, in ID order, with latest names.
    pub fn attributes(&self) -> impl Iterator<Item = &'w AttrDef> + use<'w> {
        let view = *self;
        (0..self.world.attributes.len()).filter_map(move |i| view.definition(AttrId(i as u32)))
    }
    /// Node type name, inferred origin, and first definition transaction.
    pub fn node_type_definitions(&self) -> impl Iterator<Item = (&'w str, bool, TxId)> + use<'w> {
        let view = *self;
        self.world
            .node_types
            .iter()
            .zip(&self.world.node_origins)
            .filter_map(move |(nt, h)| {
                let n = h.partition_point(|(tx, _)| *tx <= view.canon);
                n.checked_sub(1).map(|i| (nt.name.as_str(), h[i].1, h[0].0))
            })
    }
    /// Settings at this canon; the default schema mode is `infer`.
    pub fn settings(&self) -> BTreeMap<&'w str, &'w str> {
        let mut out = BTreeMap::from([("schema", "infer")]);
        for (tx, key, value) in &self.world.settings {
            if *tx <= self.canon {
                out.insert(key, value);
            }
        }
        out
    }
}

impl World {
    pub(super) fn all_overlaps(&self) -> Vec<Overlap> {
        self.at_canon(self.latest_tx)
            .attributes()
            .flat_map(|def| self.overlaps(&def.name).unwrap_or_default())
            .collect()
    }
}
impl Txn<'_> {
    pub(super) fn encode_ordered(&self) -> Result<EncodedTxn, Error> {
        let items = self.order.iter().map(|entry| match entry {
            OrderedEntry::Schema(edit) => NewEntry::Schema(edit),
            OrderedEntry::Entity(i) => {
                let d = &self.ids.defs[*i];
                NewEntry::Entity {
                    id: d.id,
                    handle: d.handle.as_deref(),
                }
            }
            OrderedEntry::Edge(i) => {
                let d = &self.ids.edges[*i];
                NewEntry::Edge {
                    id: d.id,
                    source: d.source,
                    attr: d.attr,
                    target: d.target,
                }
            }
            OrderedEntry::Fact(i) => match &self.entries[*i] {
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
            },
        });
        log::encode_txn(self.world.next_entry(), self.tx, items)
    }
}

impl<'w> MeteredView<'w> {
    /// Attribute definition at the pinned canon.
    pub fn attribute(&self, name: &str) -> Option<&'w AttrDef> {
        self.view.attribute(name)
    }
    /// Attributes defined at the pinned canon.
    pub fn attributes(&self) -> impl Iterator<Item = &'w AttrDef> + use<'w> {
        self.view.attributes()
    }
}

impl World {
    pub(crate) fn schema_matches(&self, other: &Self) -> bool {
        self.attributes == other.attributes
            && self.attribute_history == other.attribute_history
            && self.node_origins == other.node_origins
            && self.settings == other.settings
            && self.attr_by_name == other.attr_by_name
            && self.inverse_by_name == other.inverse_by_name
            && self.node_type_by_name == other.node_type_by_name
            && self
                .node_types
                .iter()
                .map(|n| &n.name)
                .eq(other.node_types.iter().map(|n| &n.name))
    }
    /// Test hook: alter derived metadata without writing the source log.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn tamper_schema(&mut self) {
        self.settings
            .push((self.latest_tx, "schema".into(), "strict".into()));
    }
}

pub(super) fn schema_warnings(before: Vec<Vec<Overlap>>, after: Vec<Vec<Overlap>>) -> Warnings {
    // The ordinary write diff receives one list per touched side. Schema edits
    // can add/remove entire sides, so align those lists before using that diff.
    let group = |lists: Vec<Vec<Overlap>>| {
        let mut groups: BTreeMap<(EntityId, String, u8), Vec<Overlap>> = BTreeMap::new();
        for overlap in lists.into_iter().flatten() {
            let key = (
                overlap.entity,
                overlap.attr.clone(),
                if overlap.side == Side::Forward { 0 } else { 1 },
            );
            groups.entry(key).or_default().push(overlap);
        }
        groups
    };
    let mut old = group(before);
    let mut new = group(after);
    let keys: std::collections::BTreeSet<_> = old.keys().chain(new.keys()).cloned().collect();
    let (before, after) = keys
        .into_iter()
        .map(|key| {
            (
                old.remove(&key).unwrap_or_default(),
                new.remove(&key).unwrap_or_default(),
            )
        })
        .unzip();
    overlap_diff(before, after)
}
