//! World schema definitions and append-only revisions, read at a canon.

use crate::value::ValueType;

/// Whether an attribute holds over time or happens once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Holds from its assert until an explicit retract ("Greg is in Varrow").
    State,
    /// Happens at one tick and is true forever after ("Edwin bought a sword");
    /// never retracted in story time.
    Event,
}

/// The author's intended number of simultaneous values.
///
/// A label, not a rule: writes are never rejected for breaking it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cardinality {
    /// At most one value at a time is intended.
    One,
    /// Any number of values, each asserted and retracted on its own.
    Many,
}

/// Which way a `ref` attribute reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RefDirection {
    /// One-sided: `Mara trusts Edwin` says nothing about `Edwin trusts Mara`.
    Directed,
    /// Genuinely mutual (`ally_of`, `married_to`): `Edwin ally_of Mara`
    /// means `Mara ally_of Edwin`. Declared, never inferred from writes.
    Bidirectional,
}

/// One attribute declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrDef {
    /// Whether this definition originated from schema-on-write.
    pub inferred: bool,
    /// Deprecated attributes remain readable but refuse new writes.
    pub deprecated: bool,
    /// Exact (case-sensitive) attribute name.
    pub name: String,
    /// The one type every value of this attribute has.
    pub value_type: ValueType,
    /// State or event.
    pub kind: Kind,
    /// Cardinality label.
    pub cardinality: Cardinality,
    /// Whether values get `V` index entries. Indexed strings are limited to
    /// [`MAX_INDEXED_STRING_BYTES`](crate::value::MAX_INDEXED_STRING_BYTES).
    pub indexed: bool,
    /// Direction of a `ref` attribute; `None` for every other value type.
    /// A `ref` attribute must have one and a non-ref attribute must not
    /// (checked when the schema is given to a world).
    pub direction: Option<RefDirection>,
    /// Name the reverse side of a directed `ref` attribute reads under
    /// (`parent_of` -> `child_of`). It shares the attribute namespace for
    /// duplicate checks but isn't an attribute: only `neighbors` accepts it.
    pub inverse: Option<String>,
    /// Cardinality label of the reverse side of a directed `ref` attribute
    /// ("a city has one ruler": `rules` with reverse `one`), checked through
    /// the `R` index. `None` means no label. Only a directed `ref` attribute
    /// may have one (checked when the schema is given to a world): a
    /// bidirectional attribute's [`cardinality`](AttrDef::cardinality)
    /// already covers both sides.
    pub reverse_cardinality: Option<Cardinality>,
}

impl AttrDef {
    /// An indexed attribute declaration. A `ref` attribute starts
    /// [`Directed`](RefDirection::Directed), any other has no direction;
    /// neither has an inverse or a reverse cardinality.
    pub fn new(name: &str, value_type: ValueType, kind: Kind, cardinality: Cardinality) -> AttrDef {
        AttrDef {
            inferred: false,
            deprecated: false,
            name: name.to_owned(),
            value_type,
            kind,
            cardinality,
            indexed: true,
            direction: (value_type == ValueType::Ref).then_some(RefDirection::Directed),
            inverse: None,
            reverse_cardinality: None,
        }
    }

    /// Marks a `ref` attribute as directed (the default for `ref`).
    pub fn directed(mut self) -> AttrDef {
        self.direction = Some(RefDirection::Directed);
        self
    }

    /// Marks a `ref` attribute as bidirectional (mutual).
    pub fn bidirectional(mut self) -> AttrDef {
        self.direction = Some(RefDirection::Bidirectional);
        self
    }

    /// Names the reverse side of a directed `ref` attribute
    /// (`parent_of` with inverse `child_of`).
    pub fn inverse(mut self, name: &str) -> AttrDef {
        self.inverse = Some(name.to_owned());
        self
    }

    /// Labels the reverse side of a directed `ref` attribute as cardinality
    /// one: each target is meant to have one source at a time (a city has
    /// one ruler). A label, like [`cardinality`](AttrDef::cardinality):
    /// overlaps are reported, never refused.
    pub fn reverse_one(mut self) -> AttrDef {
        self.reverse_cardinality = Some(Cardinality::One);
        self
    }

    /// Marks the attribute as not indexed: its values have no length limit
    /// and get no `V` entries.
    pub fn unindexed(mut self) -> AttrDef {
        self.indexed = false;
        self
    }
}

/// A world's schema, built with a builder chain:
///
/// ```
/// use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType};
/// let schema = Schema::new()
///     .node_type("CHARACTER")
///     .node_type("PLACE")
///     .attribute(AttrDef::new("located_in", ValueType::Ref, Kind::State, Cardinality::One));
/// ```
///
/// Node type and attribute IDs are assigned in declaration order starting at
/// 0. Node types and attributes are separate namespaces. Names are checked
/// (empty, duplicates, too many) when the schema is given to a world, not here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema {
    node_types: Vec<String>,
    attributes: Vec<AttrDef>,
}

impl Schema {
    /// An empty schema.
    pub fn new() -> Schema {
        Schema::default()
    }

    /// Declares a node type. Names are exact (case-sensitive).
    pub fn node_type(mut self, name: &str) -> Schema {
        self.node_types.push(name.to_owned());
        self
    }

    /// Declares an attribute. Names are exact (case-sensitive).
    pub fn attribute(mut self, def: AttrDef) -> Schema {
        self.attributes.push(def);
        self
    }

    /// Declared node type names, in declaration (= ID) order.
    pub(crate) fn node_types(&self) -> &[String] {
        &self.node_types
    }

    /// Declared attributes, in declaration (= ID) order.
    pub(crate) fn attributes(&self) -> &[AttrDef] {
        &self.attributes
    }
}

/// One schema change reported by a committed transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaChange {
    /// A new or explicitly confirmed attribute definition.
    Attribute {
        name: String,
        value_type: ValueType,
        inferred: bool,
    },
    /// A new or explicitly confirmed node type.
    NodeType { name: String, inferred: bool },
    /// A name change, preserving the attribute ID.
    Rename { old: String, new: String },
    /// New writes to the attribute are forbidden.
    Deprecate { name: String },
    /// A cardinality label change.
    Cardinality {
        name: String,
        from: Cardinality,
        to: Cardinality,
    },
    /// A world setting, effective for the next operation.
    Setting { key: String, value: String },
    /// A copy-forward migration.
    Migration {
        name: String,
        from: ValueType,
        to: ValueType,
        report: MigrationReport,
    },
}

/// The labels an explicit attribute declaration (`[:attribute NAME TYPE
/// OPTION*]`) names. `None` means "not given": an existing attribute of the
/// same type keeps its current value, and a new one (or one whose type
/// changes) gets the [`AttrDef::new`] default (state, one, directed for
/// `ref`, indexed, no inverse or reverse cardinality). See
/// [`Txn::declare_attribute_with`](crate::Txn::declare_attribute_with).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AttributeOptions {
    /// State or event.
    pub kind: Option<Kind>,
    /// Forward cardinality label.
    pub cardinality: Option<Cardinality>,
    /// Reference direction (`ref` only).
    pub direction: Option<RefDirection>,
    /// Whether values get `V` index entries.
    pub indexed: Option<bool>,
    /// Inverse name (directed `ref` only).
    pub inverse: Option<String>,
    /// Reverse cardinality label (directed `ref` only).
    pub reverse_cardinality: Option<Cardinality>,
}

/// Options omitted from a migration keep the old definition's value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationOptions {
    /// Replacement value type; omitted preserves the source type.
    pub value_type: Option<ValueType>,
    /// Replacement state/event kind.
    pub kind: Option<Kind>,
    /// Replacement forward cardinality label.
    pub cardinality: Option<Cardinality>,
    /// Replacement reference direction.
    pub direction: Option<RefDirection>,
    /// Whether the replacement has a value index.
    pub indexed: Option<bool>,
    /// Replacement inverse name; omitted preserves the source inverse.
    pub inverse: Option<String>,
    /// Replacement reverse cardinality label.
    pub reverse_cardinality: Option<Cardinality>,
    /// Skip and count unconvertible change points instead of aborting.
    pub skip_unconvertible: bool,
}

/// Node type and attribute names starting with this prefix are reserved for
/// Kala's own worlds: a genesis [`Schema`] may use them (the `system` world
/// does), but no later schema edit (inference, an explicit declaration or a
/// rename) may introduce one, and replay refuses a log where one does.
pub const RESERVED_NAME_PREFIX: &str = "_KALA_";

/// Counts of history copied by a migration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Copied source history points, counting both orientations when expanded.
    pub change_points: usize,
    /// New edges interned by the copy, including reference properties.
    pub edges: usize,
    /// Property history points copied onto the new edges.
    pub edge_properties: usize,
    /// Unconvertible or colliding source change points skipped.
    pub skipped: usize,
    /// The inverse name the source had and a bidirectional target can't
    /// keep: dropped (and freed) by the migration.
    pub dropped_inverse: Option<String>,
    /// The reverse cardinality label the source had and a bidirectional
    /// target can't keep: dropped by the migration.
    pub dropped_reverse_cardinality: Option<Cardinality>,
}

/// The payload of a schema log entry. IDs are assigned consecutively; an
/// existing attribute ID revises its definition, never its stored facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaEdit {
    Attribute {
        id: crate::AttrId,
        def: AttrDef,
    },
    NodeType {
        id: crate::NodeTypeId,
        name: String,
        inferred: bool,
    },
    Setting {
        key: String,
        value: String,
    },
    /// Announces a migration; replay regenerates and validates its exact copies.
    Migration {
        source: crate::AttrId,
        def: AttrDef,
        skip_unconvertible: bool,
    },
}
