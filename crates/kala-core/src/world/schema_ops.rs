//! Transactional schema definitions. The mutable borrow hides provisional names;
//! dropping or failing a transaction restores only its schema metadata.
use super::concurrency::Call;
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
    if name.is_empty() || name.contains('@') {
        return Err(Error::InvalidName(name.to_owned()));
    }
    if name.starts_with(crate::BUILTIN_NAME_PREFIX) {
        return Err(Error::ReservedName(name.to_owned()));
    }
    Ok(())
}

pub(super) fn check_node_type_name(name: &str) -> Result<(), Error> {
    check_name(name)?;
    if name == "edge" {
        return Err(Error::InvalidName(name.to_owned()));
    }
    Ok(())
}

/// [`check_name`] for a name a schema edit after the genesis introduces:
/// the [`RESERVED_NAME_PREFIX`](crate::RESERVED_NAME_PREFIX) is refused too.
/// Live edits and replay both validate through [`World::apply_schema_edit`],
/// so a log introducing a reserved name opens as corrupt.
pub(super) fn check_new_name(name: &str) -> Result<(), Error> {
    check_name(name)?;
    if name.starts_with(crate::RESERVED_NAME_PREFIX) {
        return Err(Error::ReservedName(name.to_owned()));
    }
    Ok(())
}

fn check_new_node_type_name(name: &str) -> Result<(), Error> {
    check_node_type_name(name)?;
    check_new_name(name)
}

impl SchemaSnapshot {
    fn capture(catalog: &Catalog) -> Self {
        Self {
            attributes: catalog.attributes.clone(),
            history: catalog.attribute_history.clone(),
            origins: catalog.node_origins.clone(),
            settings: catalog.settings.clone(),
            node_count: catalog.node_types.len(),
        }
    }
    /// Whether the schema edits since this snapshot (now applied to `catalog`)
    /// can change overlap warnings beyond the sides the transaction's facts
    /// touch: any change to an existing attribute other than its inferred or
    /// deprecated flag (cardinality and reverse labels decide which sides are
    /// checked, and names label every warning). A migration always renames
    /// its source, and the caller also treats any migration as such. New
    /// attributes and node types, deprecations and settings can't: a new
    /// attribute has no stored facts, so only its staged ones (touched
    /// sides) can overlap.
    pub(super) fn changes_overlaps(&self, catalog: &Catalog) -> bool {
        self.attributes
            .iter()
            .zip(&catalog.attributes)
            .any(|(old, new)| {
                old.name != new.name
                    || old.value_type != new.value_type
                    || old.kind != new.kind
                    || old.cardinality != new.cardinality
                    || old.indexed != new.indexed
                    || old.direction != new.direction
                    || old.inverse != new.inverse
                    || old.reverse_cardinality != new.reverse_cardinality
            })
    }
    pub(super) fn restore(self, catalog: &mut Catalog) {
        catalog.attributes = self.attributes;
        catalog.attribute_history = self.history;
        catalog.node_origins = self.origins;
        catalog.settings = self.settings;
        catalog.node_types.truncate(self.node_count);
        catalog.rebuild_schema_names();
    }
}

/// The schema half of the catalog: validating and applying schema edits.
/// Live edits and replay both go through [`Catalog::apply_schema_edit`].
impl Catalog {
    /// Rebuilds the name tables (attributes, inverses, node types) from the
    /// definitions, after an edit or a restore changed them.
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

    /// The transaction that first defined attribute `name`.
    fn attribute_first_tx(&self, name: &str) -> Option<TxId> {
        let id = self.attr_by_name.get(name)?;
        Some(self.attribute_history[id.0 as usize].first()?.0)
    }

    /// Latest value of a generic setting (see [`World::world_setting`]).
    pub(super) fn setting(&self, key: &str) -> Option<&str> {
        self.settings
            .iter()
            .rev()
            .find(|(_, k, _)| k == key)
            .map(|(_, _, v)| v.as_str())
            .or_else(|| (key == "schema").then_some("infer"))
    }

    /// Whether the world refuses to infer declarations (`schema = strict`).
    fn is_strict(&self) -> bool {
        self.setting("schema") == Some("strict")
    }

    /// Checks that defining attribute `id` as `def` is a valid edit: the
    /// next ID or an existing one, valid and unused names, and then the
    /// rules for a redefinition or a new attribute. `migration` relaxes the
    /// rules a migration's own edits are exempt from; `has_facts` says
    /// whether the attribute already has stored fact entries.
    fn validate_attr_edit(
        &self,
        id: AttrId,
        def: &AttrDef,
        migration: bool,
        has_facts: bool,
    ) -> Result<(), Error> {
        let index = id.0 as usize;
        if index > self.attributes.len() {
            return Err(Error::Schema("attribute ID is not the next ID".into()));
        }
        let old = self.attributes.get(index);
        self.check_attr_name(id, old, def, migration)?;
        self.check_inverse_name(id, old, def)?;
        match old {
            Some(old) => check_attr_redefinition(old, def, migration, has_facts),
            None => self.check_new_attr(def),
        }
    }

    /// The name and direction rules of an attribute edit: a name the edit
    /// introduces is a valid new name (a migration's rename to `x@old` is
    /// exempt), and no other attribute or inverse has it.
    fn check_attr_name(
        &self,
        id: AttrId,
        old: Option<&AttrDef>,
        def: &AttrDef,
        migration: bool,
    ) -> Result<(), Error> {
        let same_name = old.is_some_and(|old| old.name == def.name);
        if !same_name && !migration {
            check_new_name(&def.name)?;
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
        Ok(())
    }

    /// The inverse-name rules of an attribute edit: an inverse the edit
    /// introduces is a valid new name, and it is no attribute's name (its
    /// own included) and no other attribute's inverse.
    fn check_inverse_name(
        &self,
        id: AttrId,
        old: Option<&AttrDef>,
        def: &AttrDef,
    ) -> Result<(), Error> {
        let Some(inverse) = &def.inverse else {
            return Ok(());
        };
        if old.is_some_and(|old| old.inverse.as_ref() == Some(inverse)) {
            check_name(inverse)?;
        } else {
            check_new_name(inverse)?;
        }
        if inverse == &def.name
            || self.attr_by_name.contains_key(inverse)
            || self
                .inverse_by_name
                .get(inverse)
                .is_some_and(|other| *other != id)
        {
            return Err(Error::DuplicateName(inverse.clone()));
        }
        Ok(())
    }

    /// The rules for a new attribute: never born deprecated, and an
    /// inferred one has exactly the inference defaults and needs a world
    /// that isn't strict.
    fn check_new_attr(&self, def: &AttrDef) -> Result<(), Error> {
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
            if self.is_strict() || *def != expected {
                return Err(Error::Schema(
                    "invalid inferred attribute definition or inference in strict mode".into(),
                ));
            }
        }
        Ok(())
    }

    /// Validates and applies one schema edit of transaction `tx`.
    /// `migration` marks an edit that is part of a migration's own
    /// sequence; `has_facts` says whether an attribute already has stored
    /// fact entries (the indexes know, not the catalog).
    pub(super) fn apply_schema_edit(
        &mut self,
        edit: &SchemaEdit,
        tx: TxId,
        migration: bool,
        has_facts: impl Fn(AttrId) -> bool,
    ) -> Result<(), Error> {
        match edit {
            SchemaEdit::Attribute { id, def } => {
                self.apply_attr_edit(*id, def, tx, migration, has_facts(*id))
            }
            SchemaEdit::NodeType { id, name, inferred } => {
                self.apply_node_type_edit(*id, name, *inferred, tx)
            }
            SchemaEdit::Setting { key, value } => self.apply_setting(key, value, tx),
            SchemaEdit::Migration { .. } => Err(Error::Schema(
                "migration must be replayed as a validated copy sequence".into(),
            )),
        }
    }

    /// Applies an attribute edit: a new attribute (the next ID) or a
    /// redefinition, which must change something.
    fn apply_attr_edit(
        &mut self,
        id: AttrId,
        def: &AttrDef,
        tx: TxId,
        migration: bool,
        has_facts: bool,
    ) -> Result<(), Error> {
        self.validate_attr_edit(id, def, migration, has_facts)?;
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
        Ok(())
    }

    /// Applies a node type edit: a new node type (the next ID), or an
    /// inferred one being confirmed as declared.
    fn apply_node_type_edit(
        &mut self,
        id: NodeTypeId,
        name: &str,
        inferred: bool,
        tx: TxId,
    ) -> Result<(), Error> {
        // Genesis node types are declared, so they never reach the
        // redefinition branch: every edit here is a new name or an
        // inferred one being confirmed.
        check_new_node_type_name(name)?;
        let i = usize::from(id.0);
        if id.0 > MAX_NODE_TYPE || i > self.node_types.len() {
            return Err(Error::Schema(
                "node type ID out of sequence or range".into(),
            ));
        }
        if i == self.node_types.len() {
            if inferred && self.is_strict() {
                return Err(Error::Schema(
                    "cannot infer a node type in strict mode".into(),
                ));
            }
            if self.node_type_by_name.contains_key(name) {
                return Err(Error::DuplicateName(name.to_owned()));
            }
            self.node_types.push(NodeType {
                name: name.to_owned(),
                defined_in: Vec::new(),
            });
            self.node_origins.push(vec![(tx, inferred)]);
            self.node_type_by_name.insert(name.to_owned(), id);
        } else {
            if self.node_types[i].name != *name
                || inferred
                || !self.node_origins[i].last().is_some_and(|(_, v)| *v)
            {
                return Err(Error::Schema("invalid node type redefinition".into()));
            }
            self.node_origins[i].push((tx, false));
        }
        Ok(())
    }

    /// Applies a world setting, which must be valid and change something.
    fn apply_setting(&mut self, key: &str, value: &str, tx: TxId) -> Result<(), Error> {
        if key.is_empty()
            || value.is_empty()
            || (key == "schema" && value != "strict" && value != "infer")
        {
            return Err(Error::Schema("schema setting must be :strict or :infer; setting keys and values must be nonempty".into()));
        }
        if self.setting(key) == Some(value) {
            return Err(Error::Schema("a setting no-op is not a log entry".into()));
        }
        self.settings.push((tx, key.to_owned(), value.to_owned()));
        Ok(())
    }

    /// Whether the two catalogs declare the same schema (doctor compares a
    /// world with its rebuild).
    fn schema_matches(&self, other: &Self) -> bool {
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
}

/// The rules for redefining existing attribute `old` as `def`: what one
/// edit may change, and what is refused once the attribute has facts.
fn check_attr_redefinition(
    old: &AttrDef,
    def: &AttrDef,
    migration: bool,
    has_facts: bool,
) -> Result<(), Error> {
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
        check_single_purpose_edit(old, def, &renamed)?;
    }
    if structural_change(old, def) && has_facts {
        return Err(migrate_hint(&old.name));
    }
    if old.inverse.is_some() && old.inverse != def.inverse && !migration {
        return Err(Error::Schema(
            "renaming or removing an inverse is not supported; use migrate".into(),
        ));
    }
    Ok(())
}

/// Outside a migration, one edit does one thing: a deprecation changes
/// nothing else, and an inferred definition is only renamed (`renamed`:
/// `old` under `def`'s name), deprecated or relabelled.
fn check_single_purpose_edit(old: &AttrDef, def: &AttrDef, renamed: &AttrDef) -> Result<(), Error> {
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
        if def != renamed && def != &deprecated && def != &cardinality {
            return Err(Error::Schema("an inferred definition can only be renamed, deprecated or relabelled; redeclaration must mark it declared".into()));
        }
    }
    Ok(())
}

impl World {
    /// The transaction that first defined this attribute.
    pub fn attribute_first_tx(&self, name: &str) -> Option<TxId> {
        self.catalog.attribute_first_tx(name)
    }
    /// Latest value of a generic setting. Schema inference defaults to `infer`.
    pub fn world_setting(&self, key: &str) -> Option<&str> {
        self.catalog.setting(key)
    }
    /// Validates and applies one schema edit of transaction `tx` (see
    /// [`Catalog::apply_schema_edit`]); the indexes say which attributes
    /// already have facts.
    pub(super) fn apply_schema_edit(
        &mut self,
        edit: &SchemaEdit,
        tx: TxId,
        migration: bool,
    ) -> Result<(), Error> {
        let indexes = &self.indexes;
        self.catalog
            .apply_schema_edit(edit, tx, migration, |attr| indexes.has_facts(attr))
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
            before.restore(&mut self.world.catalog);
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
            && let Some(old) = self.world.catalog.attributes.get(id.0 as usize)
            && structural_change(old, def)
            && self.entries.iter().any(|e| e.key_and_mark().0.attr == *id)
        {
            return Err(migrate_hint(&old.name));
        }
        if self.schema_before.is_none() {
            self.schema_before = Some(SchemaSnapshot::capture(&self.world.catalog));
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
        self.recorded(
            |_| Call::InferWriteAttribute {
                name: name.to_owned(),
                value: value.clone(),
                op,
            },
            |txn| txn.infer_write_attribute_op(name, value, op),
        )
    }
    fn infer_write_attribute_op(
        &mut self,
        name: &str,
        value: &Value,
        op: Op,
    ) -> Result<bool, Error> {
        if op == Op::Retract
            && self.world.attribute(name).is_none()
            && self.world.world_setting("schema") != Some("strict")
        {
            // The write is dropped because the name is undeclared: a read.
            self.note_undeclared_attr(name);
            return self.guard(|_| Ok(false));
        }
        self.infer_attribute(name, value)?;
        Ok(true)
    }
    /// Schema actions staged so far, in operation order, excluding no-ops.
    /// Includes declarations inferred by writes and each migration action.
    pub fn schema_changes(&self) -> &[SchemaChange] {
        &self.schema_changes
    }
    /// The current staged definition, including earlier operations in this transaction.
    pub fn attribute(&self, name: &str) -> Option<&AttrDef> {
        let def = self.world.attribute(name);
        if def.is_none() {
            self.note_undeclared_attr(name);
        }
        def
    }
    /// Whether the transaction writes a system world ([`World::is_system`]).
    pub fn is_system(&self) -> bool {
        self.world.is_system()
    }
    /// Resolves a handle including entities created earlier in this transaction.
    ///
    /// Finding no entity is a read the caller may decide from (find-or-
    /// create): under optimistic concurrency a newer commit that creates
    /// the handle conflicts with this transaction's commit.
    pub fn entity_by_handle(&self, name: &str) -> Option<EntityId> {
        let found = self
            .ids
            .entity_by_handle(name)
            .or_else(|| self.world.entity_by_handle(name));
        if found.is_none() {
            self.note_free_handle(name);
        }
        found
    }
    /// The entity's handle, including entities created earlier in this
    /// transaction (the reverse of [`Txn::entity_by_handle`]).
    pub fn handle_of(&self, id: EntityId) -> Option<&str> {
        self.ids.handle_of(id).or_else(|| self.world.handle_of(id))
    }
    /// `CHARACTER#1`, `edge#9`, as [`World::format_id`] formats it, with
    /// node types this transaction declared.
    pub fn format_id(&self, id: EntityId) -> String {
        self.world.format_id(id)
    }
    /// Parses an entity ID using the staged node type namespace.
    pub fn parse_id(&self, name: &str) -> Option<EntityId> {
        self.world.parse_id(name)
    }
    /// Infers a definition from a first value unless the world is strict.
    pub fn infer_attribute(&mut self, name: &str, value: &Value) -> Result<(), Error> {
        self.recorded(
            |_| Call::InferAttribute {
                name: name.to_owned(),
                value: value.clone(),
            },
            |txn| txn.infer_attribute_op(name, value),
        )
    }
    fn infer_attribute_op(&mut self, name: &str, value: &Value) -> Result<(), Error> {
        self.guard(|txn| {
            if txn.world.attribute(name).is_some() { return Ok(()); }
            check_new_name(name)?;
            let ty = value.value_type();
            if txn.world.world_setting("schema") == Some("strict") { return Err(Error::Schema(format!("attribute {:?} is not in the schema; declare it first: [:attribute :{} :{}]", name,name,ty.name()))); }
            if let Value::Str(value) = value && value.len() > crate::MAX_INDEXED_STRING_BYTES { return Err(Error::Schema(format!("string over 256 bytes for an indexed attribute; declare it first: [:attribute :{} :string :unindexed]", name))); }
            let mut def = AttrDef::new(name,ty,Kind::State,if ty == ValueType::Ref { Cardinality::Many } else { Cardinality::One });
            def.inferred = true;
            txn.declare_attr_inner(def)
        })
    }
    /// Reproduce an inferred declaration from a script without a dummy fact.
    /// Only the defaults that live inference can produce are accepted, and
    /// strict mode and reserved-name checks remain in force.
    pub fn infer_attribute_with(
        &mut self,
        name: &str,
        ty: ValueType,
        options: &AttributeOptions,
    ) -> Result<(), Error> {
        self.recorded(
            |_| Call::InferAttributeWith {
                name: name.to_owned(),
                value_type: ty,
                options: options.clone(),
            },
            |txn| txn.infer_attribute_with_op(name, ty, options),
        )
    }
    fn infer_attribute_with_op(
        &mut self,
        name: &str,
        ty: ValueType,
        options: &AttributeOptions,
    ) -> Result<(), Error> {
        self.guard(|txn| {
            let cardinality = if ty == ValueType::Ref {
                Cardinality::Many
            } else {
                Cardinality::One
            };
            if options.kind.is_some_and(|v| v != Kind::State)
                || options.cardinality.is_some_and(|v| v != cardinality)
                || options
                    .direction
                    .is_some_and(|v| ty != ValueType::Ref || v != RefDirection::Directed)
                || options.indexed == Some(false)
                || options.inverse.is_some()
                || options.reverse_cardinality.is_some()
                || txn.world.attribute(name).is_some()
            {
                return Err(Error::Schema(
                    "an inferred declaration must be new and use inference defaults".into(),
                ));
            }
            let sample = match ty {
                ValueType::Bool => Value::Bool(false),
                ValueType::I64 => Value::I64(0),
                ValueType::F64 => Value::F64(0.0),
                ValueType::Str => Value::Str(String::new()),
                ValueType::Ref => Value::Ref(EntityId::new(NodeTypeId(0), 1)?),
            };
            txn.infer_attribute(name, &sample)
        })
    }

    /// Infers a node type unless the world is strict.
    pub fn infer_node_type(&mut self, name: &str) -> Result<(), Error> {
        self.recorded(
            |_| Call::InferNodeType(name.to_owned()),
            |txn| txn.infer_node_type_op(name),
        )
    }
    fn infer_node_type_op(&mut self, name: &str) -> Result<(), Error> {
        self.guard(|txn| {
            if txn.world.catalog.node_type_by_name.contains_key(name) {
                return Ok(());
            }
            check_new_node_type_name(name)?;
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
    pub fn declare_attribute(&mut self, def: AttrDef) -> Result<(), Error> {
        let recorded = self.track.then(|| def.clone());
        self.recorded(
            |_| Call::DeclareAttribute(recorded.expect("recorded only when tracking")),
            |txn| txn.declare_attribute_op(def),
        )
    }
    fn declare_attribute_op(&mut self, mut def: AttrDef) -> Result<(), Error> {
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
    /// Declares `name` as `value_type` with only the labels `options` names
    /// (the `[:attribute NAME TYPE OPTION*]` form). An existing attribute of
    /// the same type keeps every label the options leave out, so confirming
    /// an inferred attribute marks it declared and changes nothing else; a
    /// new attribute, or a type change (allowed only while the attribute has
    /// no facts), starts from the [`AttrDef::new`] defaults. The result is
    /// validated like [`Txn::declare_attribute`].
    pub fn declare_attribute_with(
        &mut self,
        name: &str,
        value_type: ValueType,
        options: &AttributeOptions,
    ) -> Result<(), Error> {
        self.recorded(
            |_| Call::DeclareAttributeWith {
                name: name.to_owned(),
                value_type,
                options: options.clone(),
            },
            |txn| txn.declare_attribute_with_op(name, value_type, options),
        )
    }
    fn declare_attribute_with_op(
        &mut self,
        name: &str,
        value_type: ValueType,
        options: &AttributeOptions,
    ) -> Result<(), Error> {
        self.guard(|txn| {
            let existing = txn
                .world
                .catalog
                .attr_by_name
                .get(name)
                .map(|id| &txn.world.catalog.attributes[id.0 as usize])
                .filter(|old| old.value_type == value_type);
            let mut def = match existing {
                Some(old) => {
                    let mut def = old.clone();
                    // Origin and deprecation are the store's: a deprecated
                    // attribute is refused by `declare_attr_inner`.
                    def.inferred = false;
                    def.deprecated = false;
                    def
                }
                None => AttrDef::new(name, value_type, Kind::State, Cardinality::One),
            };
            if let Some(kind) = options.kind {
                def.kind = kind;
            }
            if let Some(cardinality) = options.cardinality {
                def.cardinality = cardinality;
            }
            if let Some(direction) = options.direction {
                def.direction = Some(direction);
            }
            if let Some(indexed) = options.indexed {
                def.indexed = indexed;
            }
            if let Some(inverse) = &options.inverse {
                def.inverse = Some(inverse.clone());
            }
            if options.clear_reverse_cardinality {
                if options.reverse_cardinality.is_some() {
                    return Err(Error::Schema(
                        "conflicting reverse cardinality options".into(),
                    ));
                }
                def.reverse_cardinality = None;
            } else if let Some(reverse) = options.reverse_cardinality {
                def.reverse_cardinality = Some(reverse);
            }
            txn.declare_attr_inner(def)
        })
    }
    fn declare_attr_inner(&mut self, def: AttrDef) -> Result<(), Error> {
        // The previous definition's cardinality and whether anything but the
        // cardinality changes, for the report.
        let mut cardinality_from = None;
        let id = match self.world.catalog.attr_by_name.get(&def.name) {
            Some(id) => {
                let old = &self.world.catalog.attributes[id.0 as usize];
                if old.deprecated {
                    return Err(Error::DeprecatedAttribute(old.name.clone()));
                }
                if old == &def {
                    return Ok(());
                }
                // A type change (allowed only while the attribute has no
                // facts) is one redefinition, so its cardinality is part of
                // that entry, not a separate relabel.
                if old.value_type == def.value_type && old.cardinality != def.cardinality {
                    let mut same = def.clone();
                    same.cardinality = old.cardinality;
                    cardinality_from = Some((old.cardinality, &same == old));
                }
                *id
            }
            None => {
                check_new_name(&def.name)?;
                AttrId(
                    u32::try_from(self.world.catalog.attributes.len())
                        .map_err(|_| Error::InvalidName(def.name.clone()))?,
                )
            }
        };
        let name = def.name.clone();
        let to = def.cardinality;
        let report = SchemaChange::Attribute {
            name: name.clone(),
            value_type: def.value_type,
            inferred: def.inferred,
        };
        match cardinality_from {
            // A pure cardinality relabel reports exactly like `[:cardinality]`.
            Some((from, true)) => self.schema_edit(
                SchemaEdit::Attribute { id, def },
                SchemaChange::Cardinality { name, from, to },
                false,
            ),
            // Other changes as well: two edits (so every report line is one
            // log entry), the declaration with the old cardinality, then the
            // relabel exactly like `[:cardinality]`.
            Some((from, false)) => {
                let mut first = def.clone();
                first.cardinality = from;
                self.schema_edit(SchemaEdit::Attribute { id, def: first }, report, false)?;
                self.schema_edit(
                    SchemaEdit::Attribute { id, def },
                    SchemaChange::Cardinality { name, from, to },
                    false,
                )
            }
            None => self.schema_edit(SchemaEdit::Attribute { id, def }, report, false),
        }
    }
    /// Declares a node type, or confirms an inferred one.
    pub fn declare_node_type(&mut self, name: &str) -> Result<(), Error> {
        self.recorded(
            |_| Call::DeclareNodeType(name.to_owned()),
            |txn| txn.guard(|txn| txn.node_type_inner(name, false)),
        )
    }
    fn node_type_inner(&mut self, name: &str, inferred: bool) -> Result<(), Error> {
        let id = if let Some(id) = self.world.catalog.node_type_by_name.get(name) {
            if inferred
                || !self.world.catalog.node_origins[usize::from(id.0)]
                    .last()
                    .is_some_and(|(_, i)| *i)
            {
                return Ok(());
            }
            *id
        } else {
            check_new_node_type_name(name)?;
            if self.world.catalog.node_types.len() > usize::from(MAX_NODE_TYPE) {
                return Err(IdError::NodeTypeOutOfRange(MAX_NODE_TYPE + 1).into());
            }
            NodeTypeId(self.world.catalog.node_types.len() as u16)
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
        self.recorded(
            |_| Call::Rename {
                old: old.to_owned(),
                new: new.to_owned(),
            },
            |txn| txn.rename_attribute_op(old, new),
        )
    }
    fn rename_attribute_op(&mut self, old: &str, new: &str) -> Result<(), Error> {
        self.guard(|txn| {
            check_new_name(new)?;
            let id = txn.attr_id(old)?;
            if old == new {
                return Ok(());
            }
            let mut def = txn.world.catalog.attributes[id.0 as usize].clone();
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
            .catalog
            .attr_by_name
            .get(name)
            .copied()
            .ok_or_else(|| Error::UnknownAttribute(name.into()))
    }
    /// Refuses subsequent writes while preserving reads.
    pub fn deprecate_attribute(&mut self, name: &str) -> Result<(), Error> {
        self.recorded(
            |_| Call::Deprecate(name.to_owned()),
            |txn| txn.deprecate_attribute_op(name),
        )
    }
    fn deprecate_attribute_op(&mut self, name: &str) -> Result<(), Error> {
        self.guard(|txn| {
            let id = txn.attr_id(name)?;
            let mut def = txn.world.catalog.attributes[id.0 as usize].clone();
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
        self.recorded(
            |_| Call::SetCardinality {
                name: name.to_owned(),
                cardinality,
            },
            |txn| txn.set_cardinality_op(name, cardinality),
        )
    }
    fn set_cardinality_op(&mut self, name: &str, cardinality: Cardinality) -> Result<(), Error> {
        self.guard(|txn| {
            let id = txn.attr_id(name)?;
            let mut def = txn.world.catalog.attributes[id.0 as usize].clone();
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
        self.recorded(
            |_| Call::WorldSetting {
                key: key.to_owned(),
                value: value.to_owned(),
            },
            |txn| txn.world_setting_op(key, value),
        )
    }
    fn world_setting_op(&mut self, key: &str, value: &str) -> Result<(), Error> {
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

impl<'w> View<'w> {
    pub(super) fn definition(&self, id: AttrId) -> Option<&'w AttrDef> {
        if self.latest {
            return self.world.catalog.attributes.get(id.0 as usize);
        }
        let history = self.world.catalog.attribute_history.get(id.0 as usize)?;
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
        self.definition(*self.world.catalog.attr_by_name.get(name)?)
    }
    /// Resolves a name or returns a canon-specific error if it is too new.
    pub fn require_attribute(&self, name: &str) -> Result<&'w AttrDef, Error> {
        let id = self
            .world
            .catalog
            .attr_by_name
            .get(name)
            .ok_or_else(|| Error::UnknownAttribute(name.into()))?;
        self.require_definition(*id)
    }
    /// Resolves an attribute or inverse through the latest namespace, then
    /// checks its definition at this canon. The bool identifies an inverse.
    pub fn resolve_attribute(&self, name: &str) -> Result<(&'w AttrDef, bool), Error> {
        let (id, inverse) = if let Some(id) = self.world.catalog.attr_by_name.get(name) {
            (*id, false)
        } else if let Some(id) = self.world.catalog.inverse_by_name.get(name) {
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
        (0..self.world.catalog.attributes.len())
            .filter_map(move |i| view.definition(AttrId(i as u32)))
    }
    /// Node type name, inferred origin, and first definition transaction.
    pub fn node_type_definitions(&self) -> impl Iterator<Item = (&'w str, bool, TxId)> + use<'w> {
        let view = *self;
        self.world
            .catalog
            .node_types
            .iter()
            .zip(&self.world.catalog.node_origins)
            .filter_map(move |(nt, h)| {
                let n = h.partition_point(|(tx, _)| *tx <= view.canon);
                n.checked_sub(1).map(|i| (nt.name.as_str(), h[i].1, h[0].0))
            })
    }
    /// Settings at this canon; the default schema mode is `infer`.
    pub fn settings(&self) -> BTreeMap<&'w str, &'w str> {
        let mut out = BTreeMap::from([("schema", "infer")]);
        for (tx, key, value) in &self.world.catalog.settings {
            if *tx <= self.canon {
                out.insert(key, value);
            }
        }
        out
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
        log::encode_txn(
            self.world.journal.next_entry(),
            self.tx,
            self.world.journal.chain_checksum(),
            items,
        )
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
        self.catalog.schema_matches(&other.catalog)
    }
}
