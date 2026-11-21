//! The world schema, declared once at world creation.
//!
//! Create-time only (milestone 0): node types and attributes are fixed when
//! the world is created. Schema changes as log entries come later.

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
}

impl AttrDef {
    /// An indexed attribute declaration. A `ref` attribute starts
    /// [`Directed`](RefDirection::Directed), any other has no direction;
    /// neither has an inverse.
    pub fn new(name: &str, value_type: ValueType, kind: Kind, cardinality: Cardinality) -> AttrDef {
        AttrDef {
            name: name.to_owned(),
            value_type,
            kind,
            cardinality,
            indexed: true,
            direction: (value_type == ValueType::Ref).then_some(RefDirection::Directed),
            inverse: None,
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
