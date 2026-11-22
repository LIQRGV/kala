//! The YAML fixture format and its loader.
//!
//! A fixture is a small, operation-oriented YAML description of a world: its
//! schema, its entities, and the write primitives that build its history, in
//! order. [`load_fixture`] builds an in-memory [`World`] from one;
//! [`load_fixture_to`] builds a new log file. The loader only calls the
//! native API ([`World::begin`], [`Txn::create_entity`],
//! [`Txn::assert_with_precision`], [`Txn::retract_with_precision`],
//! [`Txn::correct`], [`Txn::commit`]), so a loaded world is exactly the world
//! the same calls would build by hand: same entry IDs, transactions, ticks and
//! precisions, byte for byte.
//!
//! # Format
//!
//! Every top-level key is optional; an empty document loads an empty world (no
//! node types, no attributes). Unknown keys anywhere are an error.
//!
//! ```yaml
//! schema:
//!   node_types: [CHARACTER, PLACE, ITEM, EVENT]
//!   attributes:
//!     - { name: name,       type: string }                         # defaults: kind state, cardinality one, indexed true
//!     - { name: alias,      type: string, cardinality: many }
//!     - { name: biography,  type: string, indexed: false }
//!     - { name: located_in, type: ref }                            # ref default direction: directed
//!     - { name: ally_of,    type: ref, cardinality: many, direction: bidirectional }
//!     - { name: parent_of,  type: ref, cardinality: many, inverse: child_of }
//!     - { name: ruled_by,   type: ref, reverse: one }              # reverse-side cardinality
//!     - { name: bought,     type: ref, kind: event, cardinality: many }
//!     - { name: disguised,  type: bool }
//! entities:                     # created in one transaction before everything else,
//!   CHARACTER: [edwin, greg, mara]   # per type in list order (types in schema order)
//!   PLACE: [varrow, ashford]
//! transactions:
//!   - tick: -300                # default tick for ops in this transaction without one
//!     ops:
//!       - assert: [greg, located_in, varrow]
//!       - assert: [greg, name, "Greg"]
//!       - assert: [edwin, ally_of, mara]
//!         tick: -208
//!         precision: year
//!         label: edwin-mara
//!   - ops:
//!       - create: [CHARACTER, lisa]                    # an entity introduced later
//!       - retract: [greg, located_in, varrow]
//!         tick: -250
//!         label: greg-leaves
//!       - assert: [[edwin, present, carnath], disguised, true]   # edge subject: [source, attr, target]
//!   - ops:
//!       - correct: greg-leaves
//!         replace: { tick: -280 }                      # value / tick / op / precision, any subset
//!       - correct: edwin-mara
//!         delete: true
//! ```
//!
//! ## `schema`
//!
//! - `node_types`: a list of node type names, in ID order.
//! - `attributes`: a list of attribute declarations, in ID order. `name` and
//!   `type` are required; `type` is `bool | i64 | f64 | string | ref`.
//!   Optional: `kind: state | event` (default `state`), `cardinality: one |
//!   many` (default `one`), `indexed: true | false` (default `true`), and for
//!   `ref` attributes `direction: directed | bidirectional` (default
//!   `directed`), `inverse: <name>` and `reverse: one | many` (reverse-side
//!   cardinality). The store checks the declaration as [`World::in_memory`]
//!   does (duplicate names, a direction on a non-ref attribute...).
//!
//! ## `entities`
//!
//! A mapping from node type to a list of handles. All of them are created in
//! one transaction before any other, per type in list order, types in schema
//! order (whatever their order in the YAML). With no entities, nothing is
//! committed for them.
//!
//! ## `transactions` (or the bare `operations` form)
//!
//! `transactions` is a list of `{ tick?, ops }`; each becomes one native
//! transaction. `tick` is the default tick of its ops that don't give one.
//! The bare form is a top-level `operations:` list of ops instead, each its
//! own transaction (and, in [`load_fixture_to`], its own fsync). A fixture
//! has at most one of the two.
//!
//! An op is exactly one of:
//!
//! - `assert: [subject, attr, value]` / `retract: [subject, attr, value]`,
//!   with optional `tick` (an i64), `precision` (`exact | day | month | year
//!   | decade | century`, default `exact`) and `label`. A missing tick is the
//!   transaction's `tick`, else the world's latest tick when the transaction
//!   started (the CLAUDE.md rule: every tick-less write in one transaction
//!   lands on the same tick).
//! - `create: [NODE_TYPE, handle]`: a new entity, usable by the later ops of
//!   its transaction and every later transaction.
//! - `correct: <label>` with either `delete: true` or `replace: { value?,
//!   tick?, op?, precision? }` (`op` is `assert | retract`). A label may be
//!   corrected several times; each correction applies to the chain's current
//!   revision.
//!
//! Entities are always named by handle. A subject is a handle or an edge
//! triple `[source, attr, target]` naming an edge that already exists (the
//! loader never interns one implicitly). A value of a `ref` attribute is a
//! handle or an edge triple; any other value is a YAML scalar of the
//! attribute's type: a string for `string` (`007`, `true` and `null` are not
//! strings: quote them), an integer in i64 range for `i64`, a number for
//! `f64` (an integer only if exactly representable, `|n| <= 2^53`), `true` /
//! `false` for `bool` (`yes`, `no`, `on`, `off` are not booleans).
//!
//! A `label` (assert / retract only) is unique in the fixture and names the
//! entry its op stored. It can be corrected from the next transaction on
//! (entry IDs exist only after the commit); correcting a label whose op stored
//! nothing (a "0 changed" no-op) is an error.
//!
//! ## Errors and limits
//!
//! A [`FixtureError`] says where ([`OpRef`], 0-based; `None` for the schema
//! and entities sections) and what ([`FixtureErrorKind`]). Everything the YAML
//! parser rejects is [`FixtureErrorKind::Yaml`]: syntax, duplicate keys, merge
//! keys (`<<`), unsupported tags, several documents, and the budgets (input
//! over [`MAX_FIXTURE_BYTES`], nesting deeper than 16, more than 1,000 aliases
//! or anchors, alias expansion). Shape errors after parsing are
//! [`FixtureErrorKind::Invalid`], including the loader's own limits: at most
//! 1,000,000 ops, 100,000 ops per transaction, 1,000,000 entities, 65,536 node
//! types and 65,536 attributes, and handles, labels and node type names of at
//! most 256 bytes. The store's refusals are [`FixtureErrorKind::Store`].
//!
//! Input checks that need no world (size, YAML, shape) run before a world or
//! file exists. If [`load_fixture_to`] fails later, the file holds the
//! transactions committed so far; the caller removes it.
//!
//! The loader never panics, whatever the input.
//!
//! ```
//! use kala_core::{Direction, Tick, load_fixture};
//!
//! let world = load_fixture(
//!     r#"
//! schema:
//!   node_types: [CHARACTER]
//!   attributes:
//!     - { name: ally_of, type: ref, cardinality: many, direction: bidirectional }
//! entities:
//!   CHARACTER: [edwin, mara]
//! operations:
//!   - assert: [edwin, ally_of, mara]
//!     tick: -208
//! "#,
//! )
//! .unwrap();
//! let edwin = world.entity_by_handle("edwin").unwrap();
//! let allies = world.neighbors(edwin, "ally_of", Direction::Out, Tick(-200)).unwrap();
//! assert_eq!(allies.len(), 1);
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_saphyr::options::{DuplicateKeyPolicy, MergeKeyPolicy, NonFiniteFloatPolicy};

use crate::echo::Echo;
use crate::error::Error;
use crate::id::EntityId;
use crate::log::EntryId;
use crate::schema::{AttrDef, Cardinality, Kind, Schema};
use crate::tick::{Precision, Tick};
use crate::value::{Value, ValueType};
use crate::world::{Correction, Op, Txn, World};

/// The largest fixture the loader reads, in bytes (checked before parsing).
pub const MAX_FIXTURE_BYTES: usize = 16 * 1024 * 1024;
/// The most ops in one fixture.
const MAX_OPS: usize = 1_000_000;
/// The most ops in one transaction.
const MAX_OPS_PER_TXN: usize = 100_000;
/// The most entities in the `entities` section.
const MAX_ENTITIES: usize = 1_000_000;
/// The most node types, and the most attributes, in the schema.
const MAX_SCHEMA_NAMES: usize = 65_536;
/// The longest handle, label or node type name, in bytes.
const MAX_NAME_BYTES: usize = 256;
/// The largest integer every f64 holds exactly (2^53).
const MAX_EXACT_F64_INT: i64 = 1 << 53;

// ============================================================ public API

/// The position of an op in a fixture, 0-based.
///
/// In the `transactions` form, `txn` is the transaction's index and `op` the
/// op's index in it; in the bare `operations` form, `txn` is the op's index
/// and `op` is 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRef {
    /// The transaction's index (the op's index in the bare form).
    pub txn: usize,
    /// The op's index in its transaction (0 in the bare form).
    pub op: usize,
    /// The op's own label for an assert / retract, the label it corrects for
    /// a `correct`, `None` for a `create` (or when the op is too malformed to
    /// say).
    pub label: Option<String>,
}

/// Why a fixture failed to load.
#[derive(Clone, Debug, PartialEq)]
pub enum FixtureErrorKind {
    /// The YAML parser refused the input: syntax, duplicate keys, merge keys,
    /// unsupported tags, several documents, or a parser budget or the input
    /// size limit exceeded.
    Yaml(String),
    /// The YAML is well-formed but not a fixture: an unknown key, a missing
    /// field, a scalar of the wrong type, both `transactions` and
    /// `operations`, a misplaced label, a loader limit exceeded...
    Invalid(String),
    /// No entity has this handle (yet).
    UnknownHandle(String),
    /// An edge triple `[source, attr, target]` names no interned edge.
    UnknownEdge(String),
    /// A `correct` names a label no op in the fixture has.
    UnknownLabel(String),
    /// A second op uses a label already used.
    DuplicateLabel(String),
    /// The store refused a write, a create, a correction or the schema.
    Store(Error),
}

/// A fixture load failure: where ([`OpRef`]; `None` for the schema, the
/// entities section and whole-input errors) and why.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureError {
    /// The op the error is about, if any.
    pub at: Option<OpRef>,
    /// What went wrong.
    pub kind: FixtureErrorKind,
}

impl fmt::Display for FixtureErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FixtureErrorKind::Yaml(m) => write!(f, "YAML error: {m}"),
            FixtureErrorKind::Invalid(m) => write!(f, "invalid fixture: {m}"),
            FixtureErrorKind::UnknownHandle(h) => write!(f, "unknown handle `{}`", Echo(h)),
            FixtureErrorKind::UnknownEdge(t) => write!(f, "no edge {}", Echo(t)),
            FixtureErrorKind::UnknownLabel(l) => write!(f, "unknown label `{}`", Echo(l)),
            FixtureErrorKind::DuplicateLabel(l) => write!(f, "label `{}` is used twice", Echo(l)),
            FixtureErrorKind::Store(e) => write!(f, "{e}"),
        }
    }
}

impl fmt::Display for FixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(at) = &self.at {
            write!(f, "transaction {}, op {}", at.txn, at.op)?;
            if let Some(l) = &at.label {
                write!(f, " (label {})", Echo(l))?;
            }
            write!(f, ": ")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for FixtureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            FixtureErrorKind::Store(e) => Some(e),
            _ => None,
        }
    }
}

/// Loads a fixture into a new in-memory world. See the module docs for the
/// format. Never panics; a fixture that fails anywhere returns no world.
pub fn load_fixture(yaml: &str) -> Result<World, FixtureError> {
    let plan = plan(yaml)?;
    let mut world = World::in_memory(plan.schema.clone()).map_err(|e| store(None, e))?;
    run(&plan, &mut world)?;
    Ok(world)
}

/// Loads a fixture into a new log file at `path` (same rules as
/// [`World::create`]: an existing path is refused and left alone). The size,
/// YAML and shape checks run before the file is created; a later failure
/// leaves the file holding the transactions committed so far. The bare
/// `operations` form commits, and so fsyncs, once per op.
pub fn load_fixture_to(yaml: &str, path: &Path) -> Result<World, FixtureError> {
    let plan = plan(yaml)?;
    let mut world = World::create(path, plan.schema.clone()).map_err(|e| store(None, e))?;
    run(&plan, &mut world)?;
    Ok(world)
}

// ============================================================ errors

fn err(at: Option<OpRef>, kind: FixtureErrorKind) -> FixtureError {
    FixtureError { at, kind }
}

fn invalid(at: Option<OpRef>, msg: impl Into<String>) -> FixtureError {
    err(at, FixtureErrorKind::Invalid(msg.into()))
}

fn store(at: Option<OpRef>, e: Error) -> FixtureError {
    err(at, FixtureErrorKind::Store(e))
}

// ============================================================ parsing

/// The parser options and budget approved for the loader
/// (docs/slice-4-decisions.md "Dependencies").
fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_depth: 16,
            flow_nesting_limit: 16,
            max_documents: 1,
            max_aliases: 1_000,
            max_anchors: 1_000,
            max_recorded_anchor_bytes: 1024 * 1024,
            max_total_scalar_bytes: 16 * 1024 * 1024,
            max_events: 8_000_000,
            max_nodes: 4_000_000,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 100_000,
            max_replay_stack_depth: 8,
            max_alias_expansions_per_anchor: 1_000,
        },
        duplicate_keys: DuplicateKeyPolicy::Error,
        strict_booleans: true,
        merge_keys: MergeKeyPolicy::Error,
        reject_unsupported_tags: true,
        // A tag on a struct field name (`!!binary YXNzZXJ0:`) is refused
        // instead of acting as the untagged key (slice-4 finding M1). The
        // parser lets `!!str` through; `UntaggedKeys` refuses every tag on
        // every key.
        reject_tagged_field_names: true,
        legacy_octal_numbers: false,
        // `.nan` reaches the visitor, so it is refused by the store as a value
        // (`Store(Value(NaN))`), like a NaN written through the native API.
        non_finite_float_policy: NonFiniteFloatPolicy::PassThrough,
    }
}

/// Parses `yaml` into the raw document, telling parser errors (`Yaml`) from
/// shape errors (`Invalid`).
fn parse(yaml: &str) -> Result<Option<RawDoc>, FixtureError> {
    if yaml.len() > MAX_FIXTURE_BYTES {
        return Err(err(
            None,
            FixtureErrorKind::Yaml(format!(
                "the input is {} bytes; the limit is {MAX_FIXTURE_BYTES}",
                yaml.len()
            )),
        ));
    }
    // First pass, shape-free: everything that fails here is the YAML itself
    // (syntax, duplicate keys, tags, budgets), whatever the fixture types
    // expect. The later passes can then only fail on the fixture's shape: a
    // tagged key anywhere (`UntaggedKeys`), then the fixture types.
    serde_saphyr::from_str_with_options::<IgnoredAny>(yaml, yaml_options())
        .map_err(|e| err(None, FixtureErrorKind::Yaml(e.to_string())))?;
    serde_saphyr::from_str_with_options::<Option<UntaggedKeys>>(yaml, yaml_options())
        .map_err(|e| invalid(None, shape_message(&e)))?;
    serde_saphyr::from_str_with_options::<Option<NoTag<RawDoc>>>(yaml, yaml_options())
        .map(|d| d.map(|d| d.0))
        .map_err(|e| invalid(None, shape_message(&e)))
}

/// The message of a shape error from the parser. An unknown field echoes
/// the key as written, so it is rebuilt with the key through [`Echo`]
/// (escaped and bounded) and without the parser's source snippet.
fn shape_message(e: &serde_saphyr::Error) -> String {
    let mut inner = e;
    while let serde_saphyr::Error::WithSnippet { error, .. }
    | serde_saphyr::Error::AliasError { error, .. } = inner
    {
        inner = error;
    }
    match inner {
        serde_saphyr::Error::SerdeUnknownField {
            field, expected, ..
        } => {
            let at = match e.location() {
                Some(l) if l.line() > 0 => format!("line {} column {}: ", l.line(), l.column()),
                _ => String::new(),
            };
            format!(
                "{at}unknown field `{}`, expected one of {}",
                Echo(field),
                expected.join(", ")
            )
        }
        _ => e.to_string(),
    }
}

/// Deserializes a field that is present (so an explicit `null` is a value,
/// not a missing field), refusing an explicit tag on it.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    NoTag::<T>::deserialize(d).map(|v| Some(v.0))
}

/// Deserializes a required field, refusing an explicit tag on it.
fn untagged<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<T, D::Error> {
    NoTag::<T>::deserialize(d).map(|v| v.0)
}

/// A node with no explicit YAML tag. A fixture never needs one, and a
/// standard tag the parser accepts (`!!map`, `!!binary`, `!!str`) would
/// otherwise change or hide what the text says; scalars keep the tag as
/// [`Scalar::Other`] instead (so the error names the op).
#[derive(Debug)]
struct NoTag<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for NoTag<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<NoTag<T>, D::Error> {
        let serde_saphyr::Tagged(v, tag) = serde_saphyr::Tagged::<T>::deserialize(d)?;
        match tag {
            Some(_) => Err(de::Error::custom(
                "explicit YAML tags are not allowed in a fixture",
            )),
            None => Ok(NoTag(v)),
        }
    }
}

/// A YAML scalar, read through `deserialize_any` so its YAML type is kept
/// (`5`, `"5"`, `true` and `null` are all different).
#[derive(Clone, Debug, PartialEq)]
enum Scalar {
    Null,
    Bool(bool),
    Int(i128),
    /// An integer outside `i128`.
    BigInt,
    Float(f64),
    Str(String),
    /// Not a scalar (a list, a mapping or binary data), already consumed.
    Other(&'static str),
}

impl Scalar {
    /// What the scalar is, for messages (never echoes its contents, which may
    /// be large).
    fn describe(&self) -> &'static str {
        match self {
            Scalar::Null => "null",
            Scalar::Bool(_) => "a boolean",
            Scalar::Int(_) | Scalar::BigInt => "an integer",
            Scalar::Float(_) => "a float",
            Scalar::Str(_) => "a string",
            Scalar::Other(what) => what,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Scalar::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// The scalar arms shared by every visitor below; `$wrap` turns a [`Scalar`]
/// into the visitor's value.
macro_rules! scalar_visits {
    ($wrap:expr) => {
        fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Bool(v)))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Int(i128::from(v))))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Int(i128::from(v))))
        }
        fn visit_i128<E: de::Error>(self, v: i128) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Int(v)))
        }
        fn visit_u128<E: de::Error>(self, v: u128) -> Result<Self::Value, E> {
            Ok($wrap(i128::try_from(v).map_or(Scalar::BigInt, Scalar::Int)))
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Float(v)))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Str(v.to_owned())))
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Str(v)))
        }
        fn visit_bytes<E: de::Error>(self, _: &[u8]) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Other("binary data")))
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Null))
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($wrap(Scalar::Null))
        }
        fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            d.deserialize_any(self)
        }
    };
}

/// Consumes a nested list or mapping without keeping it. `IgnoredAny`'s
/// recursion is bounded by the parser's depth budget (16).
fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// The whole document, walked only to refuse an explicit tag on any mapping
/// key (`!!str assert:`, `!!binary YXNzZXJ0:`): a tagged key would otherwise
/// match the untagged field, and the parser's `reject_tagged_field_names`
/// lets `!!str` through and covers struct fields only.
struct UntaggedKeys;

impl<'de> Deserialize<'de> for UntaggedKeys {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<UntaggedKeys, D::Error> {
        d.deserialize_any(UntaggedKeysVisitor)
    }
}

struct UntaggedKeysVisitor;

impl<'de> Visitor<'de> for UntaggedKeysVisitor {
    type Value = UntaggedKeys;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a YAML value")
    }
    scalar_visits!(|_: Scalar| UntaggedKeys);
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<UntaggedKeys, A::Error> {
        while seq.next_element::<UntaggedKeys>()?.is_some() {}
        Ok(UntaggedKeys)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UntaggedKeys, A::Error> {
        while let Some(serde_saphyr::Tagged(UntaggedKeys, tag)) =
            map.next_key::<serde_saphyr::Tagged<UntaggedKeys>>()?
        {
            if tag.is_some() {
                return Err(de::Error::custom(
                    "explicit YAML tags are not allowed on a key in a fixture",
                ));
            }
            map.next_value::<UntaggedKeys>()?;
        }
        Ok(UntaggedKeys)
    }
}

struct ScalarVisitor;

impl<'de> Visitor<'de> for ScalarVisitor {
    type Value = Scalar;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a YAML value")
    }
    scalar_visits!(|s| s);
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Scalar, A::Error> {
        drain_seq(seq)?;
        Ok(Scalar::Other("a list"))
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Scalar, A::Error> {
        drain_map(map)?;
        Ok(Scalar::Other("a mapping"))
    }
}

/// Reads a node through `deserialize_any` with visitor `$visitor`, as
/// `$raw`, then refuses an explicit tag (`!!binary`, `!!str 5`, `!custom`):
/// a fixture never needs one, and a standard tag the parser accepts would
/// otherwise change a value silently (`!!binary YQ==` reads as `a`).
macro_rules! untagged_any {
    ($ty:ident, $raw:ident, $visitor:ident, $tagged:expr) => {
        struct $raw($ty);

        impl<'de> Deserialize<'de> for $raw {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<$raw, D::Error> {
                d.deserialize_any($visitor).map($raw)
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
                let serde_saphyr::Tagged($raw(v), tag) = Deserialize::deserialize(d)?;
                Ok(if tag.is_some() { $tagged } else { v })
            }
        }
    };
}

untagged_any!(
    Scalar,
    RawScalar,
    ScalarVisitor,
    Scalar::Other("a tagged value")
);

/// A scalar or a flat list of scalars (an edge triple, a `create` pair, a
/// list of handles). Deeper nesting is kept as [`Scalar::Other`].
#[derive(Clone, Debug, PartialEq)]
enum Item {
    Scalar(Scalar),
    List(Vec<Scalar>),
}

struct ItemVisitor;

impl<'de> Visitor<'de> for ItemVisitor {
    type Value = Item;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a YAML value")
    }
    scalar_visits!(Item::Scalar);
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Item, A::Error> {
        let mut out = Vec::new();
        while let Some(s) = seq.next_element::<Scalar>()? {
            out.push(s);
        }
        Ok(Item::List(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Item, A::Error> {
        drain_map(map)?;
        Ok(Item::Scalar(Scalar::Other("a mapping")))
    }
}

untagged_any!(
    Item,
    RawItem,
    ItemVisitor,
    Item::Scalar(Scalar::Other("a tagged value"))
);

/// The argument list of `assert` / `retract` / `create`: a list of items, or
/// something that isn't a list.
#[derive(Clone, Debug, PartialEq)]
enum Args {
    List(Vec<Item>),
    NotAList(&'static str),
}

struct ArgsVisitor;

impl<'de> Visitor<'de> for ArgsVisitor {
    type Value = Args;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list")
    }
    scalar_visits!(|s: Scalar| Args::NotAList(s.describe()));
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Args, A::Error> {
        let mut out = Vec::new();
        while let Some(i) = seq.next_element::<Item>()? {
            out.push(i);
        }
        Ok(Args::List(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Args, A::Error> {
        drain_map(map)?;
        Ok(Args::NotAList("a mapping"))
    }
}

untagged_any!(Args, RawArgs, ArgsVisitor, Args::NotAList("a tagged value"));

/// The `entities` mapping, in YAML order.
#[derive(Debug)]
struct RawEntities(Vec<(Scalar, Item)>);

struct EntitiesVisitor;

impl<'de> Visitor<'de> for EntitiesVisitor {
    type Value = RawEntities;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a mapping from node type to a list of handles")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawEntities, A::Error> {
        let mut out = Vec::new();
        while let Some(entry) = map.next_entry::<Scalar, Item>()? {
            out.push(entry);
        }
        Ok(RawEntities(out))
    }
}

impl<'de> Deserialize<'de> for RawEntities {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<RawEntities, D::Error> {
        d.deserialize_map(EntitiesVisitor)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDoc {
    #[serde(default, deserialize_with = "present")]
    schema: Option<RawSchema>,
    #[serde(default, deserialize_with = "present")]
    entities: Option<RawEntities>,
    #[serde(default, deserialize_with = "present")]
    transactions: Option<Vec<NoTag<RawTxn>>>,
    #[serde(default, deserialize_with = "present")]
    operations: Option<Vec<NoTag<RawOp>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[serde(default, deserialize_with = "present")]
    node_types: Option<Vec<Scalar>>,
    #[serde(default, deserialize_with = "present")]
    attributes: Option<Vec<NoTag<RawAttr>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAttr {
    #[serde(default, deserialize_with = "present")]
    name: Option<Scalar>,
    #[serde(default, rename = "type", deserialize_with = "present")]
    value_type: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    kind: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    cardinality: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    direction: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    inverse: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    reverse: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    indexed: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTxn {
    #[serde(default, deserialize_with = "present")]
    tick: Option<Scalar>,
    #[serde(deserialize_with = "untagged")]
    ops: Vec<NoTag<RawOp>>,
}

/// One op as written: every key optional, "exactly one verb" checked in code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOp {
    #[serde(default, deserialize_with = "present")]
    assert: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    retract: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    create: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    correct: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    delete: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    replace: Option<RawReplace>,
    #[serde(default, deserialize_with = "present")]
    tick: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    precision: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    label: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReplace {
    #[serde(default, deserialize_with = "present")]
    value: Option<Item>,
    #[serde(default, deserialize_with = "present")]
    tick: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    op: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    precision: Option<Scalar>,
}

// ============================================================ the plan

/// A fixture checked for shape: everything that needs no world.
#[derive(Debug)]
struct Plan {
    schema: Schema,
    /// Declared attributes' value types, by name.
    attr_types: HashMap<String, ValueType>,
    /// The entities section, per node type in schema order.
    entities: Vec<(String, Vec<String>)>,
    txns: Vec<PTxn>,
    /// Every label an op defines (so a `correct` can tell "not yet" from
    /// "never").
    labels: HashSet<String>,
}

#[derive(Debug)]
struct PTxn {
    tick: Option<Tick>,
    /// `(position, op)`.
    ops: Vec<(OpRef, POp)>,
}

#[derive(Debug)]
enum POp {
    Write {
        op: Op,
        subject: Subject,
        attr: String,
        value: Item,
        tick: Option<Tick>,
        precision: Precision,
        label: Option<String>,
    },
    Create {
        node_type: String,
        handle: String,
    },
    Correct {
        label: String,
        fix: PFix,
    },
}

#[derive(Debug)]
enum PFix {
    Delete,
    Replace {
        value: Option<Item>,
        tick: Option<Tick>,
        op: Option<Op>,
        precision: Option<Precision>,
    },
}

/// A write's subject: a handle or an edge triple.
#[derive(Debug)]
enum Subject {
    Handle(String),
    Edge(String, String, String),
}

fn plan(yaml: &str) -> Result<Plan, FixtureError> {
    let doc = parse(yaml)?;
    let RawDoc {
        schema,
        entities,
        transactions,
        operations,
    } = doc.unwrap_or(RawDoc {
        schema: None,
        entities: None,
        transactions: None,
        operations: None,
    });
    let (schema, node_types, attr_types) = plan_schema(schema)?;
    let entities = plan_entities(entities, &node_types)?;
    let raw_txns: Vec<(Option<Scalar>, Vec<RawOp>, bool)> = match (transactions, operations) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                None,
                "a fixture has `transactions` or `operations`, not both",
            ));
        }
        (Some(t), None) => t
            .into_iter()
            .map(|NoTag(t)| (t.tick, t.ops.into_iter().map(|o| o.0).collect(), false))
            .collect(),
        // The bare form: each op its own transaction, with no default tick.
        (None, Some(o)) => o.into_iter().map(|op| (None, vec![op.0], true)).collect(),
        (None, None) => Vec::new(),
    };
    let total: usize = raw_txns.iter().map(|t| t.1.len()).sum();
    if total > MAX_OPS {
        return Err(invalid(
            None,
            format!("{total} ops; the limit is {MAX_OPS}"),
        ));
    }
    let mut labels = HashSet::new();
    let mut txns = Vec::with_capacity(raw_txns.len());
    for (ti, (tick, ops, _bare)) in raw_txns.into_iter().enumerate() {
        if ops.len() > MAX_OPS_PER_TXN {
            return Err(invalid(
                Some(OpRef {
                    txn: ti,
                    op: MAX_OPS_PER_TXN,
                    label: None,
                }),
                format!(
                    "{} ops in one transaction; the limit is {MAX_OPS_PER_TXN}",
                    ops.len()
                ),
            ));
        }
        let tick = match tick {
            None => None,
            Some(s) => Some(
                to_tick(&s).map_err(|m| invalid(None, format!("transaction {ti}: `tick` {m}")))?,
            ),
        };
        let mut planned = Vec::with_capacity(ops.len());
        for (oi, raw) in ops.into_iter().enumerate() {
            let (at, op) = plan_op(ti, oi, raw)?;
            if let POp::Write { label: Some(l), .. } = &op
                && !labels.insert(l.clone())
            {
                return Err(err(Some(at), FixtureErrorKind::DuplicateLabel(l.clone())));
            }
            planned.push((at, op));
        }
        txns.push(PTxn { tick, ops: planned });
    }
    Ok(Plan {
        schema,
        attr_types,
        entities,
        txns,
        labels,
    })
}

/// A name (handle, label, node type) from a scalar: a string of at most
/// [`MAX_NAME_BYTES`] bytes.
fn to_name(s: &Scalar, what: &str) -> Result<String, String> {
    match s {
        Scalar::Str(v) if v.len() <= MAX_NAME_BYTES => Ok(v.clone()),
        Scalar::Str(v) => Err(format!(
            "{what} is {} bytes; the limit is {MAX_NAME_BYTES}",
            v.len()
        )),
        other => Err(format!("{what} must be a string, not {}", other.describe())),
    }
}

/// A plain string (attribute name, schema keyword) from a scalar.
fn to_string(s: &Scalar, what: &str) -> Result<String, String> {
    s.as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{what} must be a string, not {}", s.describe()))
}

fn to_tick(s: &Scalar) -> Result<Tick, String> {
    match s {
        Scalar::Int(i) => i64::try_from(*i)
            .map(Tick)
            .map_err(|_| "is outside the i64 range".to_owned()),
        Scalar::BigInt => Err("is outside the i64 range".to_owned()),
        other => Err(format!("must be an integer, not {}", other.describe())),
    }
}

fn to_precision(s: &Scalar) -> Result<Precision, String> {
    Ok(match s.as_str() {
        Some("exact") => Precision::Exact,
        Some("day") => Precision::Day,
        Some("month") => Precision::Month,
        Some("year") => Precision::Year,
        Some("decade") => Precision::Decade,
        Some("century") => Precision::Century,
        _ => {
            return Err(
                "`precision` must be one of exact, day, month, year, decade, century".to_owned(),
            );
        }
    })
}

fn to_op(s: &Scalar) -> Result<Op, String> {
    match s.as_str() {
        Some("assert") => Ok(Op::Assert),
        Some("retract") => Ok(Op::Retract),
        _ => Err("`op` must be assert or retract".to_owned()),
    }
}

/// Picks one of several keyword spellings, or says what was expected.
fn keyword<T: Copy>(s: &Scalar, field: &str, options: &[(&str, T)]) -> Result<T, String> {
    let names: Vec<&str> = options.iter().map(|o| o.0).collect();
    s.as_str()
        .and_then(|v| options.iter().find(|o| o.0 == v).map(|o| o.1))
        .ok_or_else(|| format!("`{field}` must be one of {}", names.join(", ")))
}

type SchemaPlan = (Schema, Vec<String>, HashMap<String, ValueType>);

fn plan_schema(raw: Option<RawSchema>) -> Result<SchemaPlan, FixtureError> {
    let bad = |m: String| invalid(None, format!("schema: {m}"));
    let mut schema = Schema::new();
    let mut node_types = Vec::new();
    let mut attr_types = HashMap::new();
    let Some(raw) = raw else {
        return Ok((schema, node_types, attr_types));
    };
    let types = raw.node_types.unwrap_or_default();
    if types.len() > MAX_SCHEMA_NAMES {
        return Err(bad(format!(
            "{} node types; the limit is {MAX_SCHEMA_NAMES}",
            types.len()
        )));
    }
    for t in &types {
        let name = to_name(t, "a node type name").map_err(bad)?;
        schema = schema.node_type(&name);
        node_types.push(name);
    }
    let attrs = raw.attributes.unwrap_or_default();
    if attrs.len() > MAX_SCHEMA_NAMES {
        return Err(bad(format!(
            "{} attributes; the limit is {MAX_SCHEMA_NAMES}",
            attrs.len()
        )));
    }
    for (i, a) in attrs.iter().enumerate() {
        let def = plan_attr(&a.0).map_err(|m| bad(format!("attribute {i}: {m}")))?;
        attr_types.entry(def.name.clone()).or_insert(def.value_type);
        schema = schema.attribute(def);
    }
    Ok((schema, node_types, attr_types))
}

fn plan_attr(a: &RawAttr) -> Result<AttrDef, String> {
    let name = to_string(a.name.as_ref().ok_or("`name` is missing")?, "`name`")?;
    let value_type = keyword(
        a.value_type.as_ref().ok_or("`type` is missing")?,
        "type",
        &[
            ("bool", ValueType::Bool),
            ("i64", ValueType::I64),
            ("f64", ValueType::F64),
            ("string", ValueType::Str),
            ("ref", ValueType::Ref),
        ],
    )?;
    let kind = match &a.kind {
        None => Kind::State,
        Some(s) => keyword(s, "kind", &[("state", Kind::State), ("event", Kind::Event)])?,
    };
    let card = [("one", Cardinality::One), ("many", Cardinality::Many)];
    let cardinality = match &a.cardinality {
        None => Cardinality::One,
        Some(s) => keyword(s, "cardinality", &card)?,
    };
    let mut def = AttrDef::new(&name, value_type, kind, cardinality);
    if let Some(s) = &a.direction {
        // Set whatever the type: the store refuses a direction on a non-ref
        // attribute when the schema is given to the world.
        def = match keyword(
            s,
            "direction",
            &[("directed", false), ("bidirectional", true)],
        )? {
            false => def.directed(),
            true => def.bidirectional(),
        };
    }
    if let Some(s) = &a.inverse {
        def = def.inverse(&to_string(s, "`inverse`")?);
    }
    if let Some(s) = &a.reverse {
        def.reverse_cardinality = Some(keyword(s, "reverse", &card)?);
    }
    if let Some(s) = &a.indexed {
        match s {
            Scalar::Bool(true) => {}
            Scalar::Bool(false) => def = def.unindexed(),
            other => {
                return Err(format!(
                    "`indexed` must be true or false, not {}",
                    other.describe()
                ));
            }
        }
    }
    Ok(def)
}

fn plan_entities(
    raw: Option<RawEntities>,
    node_types: &[String],
) -> Result<Vec<(String, Vec<String>)>, FixtureError> {
    let bad = |m: String| invalid(None, format!("entities: {m}"));
    let Some(RawEntities(raw)) = raw else {
        return Ok(Vec::new());
    };
    let mut by_type: HashMap<String, Vec<String>> = HashMap::new();
    let mut total = 0usize;
    for (key, item) in &raw {
        let ty = to_name(key, "a node type name").map_err(bad)?;
        let Item::List(handles) = item else {
            return Err(bad(format!("`{}` must be a list of handles", Echo(&ty))));
        };
        total = total.saturating_add(handles.len());
        if total > MAX_ENTITIES {
            return Err(bad(format!(
                "more than {MAX_ENTITIES} entities; the limit is {MAX_ENTITIES}"
            )));
        }
        let handles = handles
            .iter()
            .map(|h| to_name(h, "a handle"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(bad)?;
        if by_type.insert(ty.clone(), handles).is_some() {
            return Err(bad(format!("`{}` is listed twice", Echo(&ty))));
        }
    }
    // A type the schema doesn't declare is the store's refusal, as a create
    // of that type would be.
    for (key, _) in &raw {
        if let Some(ty) = key.as_str()
            && !node_types.iter().any(|t| t == ty)
        {
            return Err(store(None, Error::UnknownNodeType(ty.to_owned())));
        }
    }
    // Schema order, whatever the YAML order.
    Ok(node_types
        .iter()
        .filter_map(|t| by_type.remove(t).map(|hs| (t.clone(), hs)))
        .collect())
}

fn plan_op(txn: usize, op: usize, raw: RawOp) -> Result<(OpRef, POp), FixtureError> {
    let mut at = OpRef {
        txn,
        op,
        label: None,
    };
    let verbs = [
        raw.assert.is_some(),
        raw.retract.is_some(),
        raw.create.is_some(),
        raw.correct.is_some(),
    ];
    if verbs.iter().filter(|v| **v).count() != 1 {
        return Err(invalid(
            Some(at),
            "an op is exactly one of `assert`, `retract`, `create`, `correct`",
        ));
    }
    let RawOp {
        assert,
        retract,
        create,
        correct,
        delete,
        replace,
        tick,
        precision,
        label,
    } = raw;
    if let Some(args) = assert
        .map(|a| (Op::Assert, a))
        .or(retract.map(|r| (Op::Retract, r)))
    {
        // The op's own label names its position, once it is a valid label.
        let label = match &label {
            None => None,
            Some(s) => Some(to_name(s, "`label`").map_err(|m| invalid(Some(at.clone()), m))?),
        };
        at.label.clone_from(&label);
        let bad = |m: String| invalid(Some(at.clone()), m);
        if delete.is_some() || replace.is_some() {
            return Err(bad(
                "`delete` and `replace` belong to a `correct`".to_owned()
            ));
        }
        let (op, args) = args;
        let Args::List(mut items) = args else {
            return Err(bad(format!(
                "`{}` takes a list [subject, attr, value]",
                verb(op)
            )));
        };
        if items.len() != 3 {
            return Err(bad(format!(
                "`{}` takes a list [subject, attr, value], not {} items",
                verb(op),
                items.len()
            )));
        }
        let value = items.pop().ok_or_else(|| bad("missing value".to_owned()))?;
        let attr = items.pop().ok_or_else(|| bad("missing attr".to_owned()))?;
        let subject = items
            .pop()
            .ok_or_else(|| bad("missing subject".to_owned()))?;
        let subject = to_subject(&subject).map_err(&bad)?;
        let attr = match attr {
            Item::Scalar(Scalar::Str(a)) => a,
            Item::Scalar(s) => {
                return Err(bad(format!(
                    "the attribute must be a string, not {}",
                    s.describe()
                )));
            }
            Item::List(_) => return Err(bad("the attribute must be a string".to_owned())),
        };
        let tick = match &tick {
            None => None,
            Some(s) => Some(to_tick(s).map_err(|m| bad(format!("`tick` {m}")))?),
        };
        let precision = match &precision {
            None => Precision::Exact,
            Some(s) => to_precision(s).map_err(&bad)?,
        };
        return Ok((
            at.clone(),
            POp::Write {
                op,
                subject,
                attr,
                value,
                tick,
                precision,
                label,
            },
        ));
    }
    if let Some(args) = create {
        let bad = |m: &str| invalid(Some(at.clone()), m.to_owned());
        if label.is_some() {
            return Err(bad("a `create` has no label"));
        }
        if tick.is_some() || precision.is_some() {
            return Err(bad("`tick` and `precision` belong to an assert or retract"));
        }
        if delete.is_some() || replace.is_some() {
            return Err(bad("`delete` and `replace` belong to a `correct`"));
        }
        let Args::List(items) = args else {
            return Err(bad("`create` takes a list [NODE_TYPE, handle]"));
        };
        let [Item::Scalar(t), Item::Scalar(h)] = items.as_slice() else {
            return Err(bad("`create` takes a list [NODE_TYPE, handle]"));
        };
        let node_type = to_name(t, "the node type").map_err(|m| invalid(Some(at.clone()), m))?;
        let handle = to_name(h, "the handle").map_err(|m| invalid(Some(at.clone()), m))?;
        return Ok((at, POp::Create { node_type, handle }));
    }
    let Some(target) = correct else {
        // Unreachable: exactly one verb is present and it isn't any of the
        // others. Reported, never a panic.
        return Err(invalid(Some(at), "an op needs a verb"));
    };
    let target =
        to_name(&target, "the corrected label").map_err(|m| invalid(Some(at.clone()), m))?;
    at.label = Some(target.clone());
    let bad = |m: &str| invalid(Some(at.clone()), m.to_owned());
    if label.is_some() {
        return Err(bad("a `correct` has no label of its own"));
    }
    if tick.is_some() || precision.is_some() {
        return Err(bad(
            "`tick` and `precision` of a correction go inside `replace`",
        ));
    }
    let fix = match (delete, replace) {
        (Some(Scalar::Bool(true)), None) => PFix::Delete,
        (Some(_), None) => return Err(bad("`delete` must be true")),
        (None, Some(r)) => {
            let bad = |m: String| invalid(Some(at.clone()), format!("`replace`: {m}"));
            PFix::Replace {
                value: r.value,
                tick: match &r.tick {
                    None => None,
                    Some(s) => Some(to_tick(s).map_err(|m| bad(format!("`tick` {m}")))?),
                },
                op: match &r.op {
                    None => None,
                    Some(s) => Some(to_op(s).map_err(&bad)?),
                },
                precision: match &r.precision {
                    None => None,
                    Some(s) => Some(to_precision(s).map_err(&bad)?),
                },
            }
        }
        _ => {
            return Err(bad(
                "a `correct` takes exactly one of `delete: true` or `replace: {...}`",
            ));
        }
    };
    Ok((at, POp::Correct { label: target, fix }))
}

fn verb(op: Op) -> &'static str {
    match op {
        Op::Assert => "assert",
        Op::Retract => "retract",
    }
}

/// A subject: a handle, or an edge triple of three strings.
fn to_subject(item: &Item) -> Result<Subject, String> {
    match item {
        Item::Scalar(s) => Ok(Subject::Handle(to_name(s, "the subject handle")?)),
        Item::List(l) => {
            let (s, a, t) = to_triple(l)?;
            Ok(Subject::Edge(s, a, t))
        }
    }
}

/// An edge triple `[source, attr, target]` of strings (handles bounded).
fn to_triple(l: &[Scalar]) -> Result<(String, String, String), String> {
    let [s, a, t] = l else {
        return Err(format!(
            "an edge triple is [source, attr, target], not {} items",
            l.len()
        ));
    };
    Ok((
        to_name(s, "the edge source")?,
        to_string(a, "the edge attribute")?,
        to_name(t, "the edge target")?,
    ))
}

// ============================================================ running

/// What a label names once its transaction committed.
#[derive(Clone, Copy, Debug)]
enum Labelled {
    /// The base entry its op stored, and its attribute's value type.
    Stored(EntryId, ValueType),
    /// Its op was a "0 changed" no-op.
    Nothing,
}

/// What one op did: how many entries it stored and, for a labelled write,
/// its label and attribute type.
type Outcome<'p> = (usize, Option<(&'p str, ValueType)>);

struct Runner<'p> {
    plan: &'p Plan,
    /// Every handle created so far (committed or staged in the current
    /// transaction): the loader builds the world from empty, so this is the
    /// world's handle table.
    handles: HashMap<String, EntityId>,
    /// Labels of committed transactions.
    labels: HashMap<String, Labelled>,
}

fn run(plan: &Plan, world: &mut World) -> Result<(), FixtureError> {
    let mut r = Runner {
        plan,
        handles: HashMap::new(),
        labels: HashMap::new(),
    };
    let mut tx = world.begin();
    for (ty, hs) in &plan.entities {
        for h in hs {
            let id = tx.create_entity(ty, Some(h)).map_err(|e| store(None, e))?;
            r.handles.insert(h.clone(), id);
        }
    }
    tx.commit_quiet().map_err(|e| store(None, e))?;
    for t in &plan.txns {
        r.txn(world, t)?;
    }
    Ok(())
}

impl Runner<'_> {
    fn txn(&mut self, world: &mut World, t: &PTxn) -> Result<(), FixtureError> {
        let mut tx = world.begin();
        if let Some(d) = t.tick {
            tx.set_default_tick(d);
        }
        // Per op: the entries it stored and, for a labelled write, the label.
        let mut stored: Vec<Outcome<'_>> = Vec::with_capacity(t.ops.len());
        for (at, op) in &t.ops {
            let s = self.op(&mut tx, at, op)?;
            stored.push(s);
        }
        let last = t.ops.last().map(|(at, _)| at.clone());
        let res = tx.commit_quiet().map_err(|e| store(last.clone(), e))?;
        // Entries are listed in write order, `changed` per op.
        let mut next = res.entries.iter();
        for (changed, label) in stored {
            // The op's first entry is its base fact (a correction's entries
            // are skipped: labels name only writes).
            let mut mine = next.by_ref().take(changed);
            let first = mine.next().copied();
            mine.for_each(drop);
            let Some((l, vt)) = label else { continue };
            let named = match (changed, first) {
                (0, _) => Labelled::Nothing,
                (_, Some(e)) => Labelled::Stored(e, vt),
                (_, None) => {
                    return Err(invalid(
                        last,
                        "internal: the commit listed fewer entries than its writes stored",
                    ));
                }
            };
            self.labels.insert(l.to_owned(), named);
        }
        Ok(())
    }

    /// Runs one op; returns the number of entries it stored and its label.
    fn op<'p>(
        &mut self,
        tx: &mut Txn<'_>,
        at: &OpRef,
        op: &'p POp,
    ) -> Result<Outcome<'p>, FixtureError> {
        let pos = || Some(at.clone());
        match op {
            POp::Create { node_type, handle } => {
                let id = tx
                    .create_entity(node_type, Some(handle))
                    .map_err(|e| store(pos(), e))?;
                self.handles.insert(handle.clone(), id);
                Ok((0, None))
            }
            POp::Write {
                op,
                subject,
                attr,
                value,
                tick,
                precision,
                label,
            } => {
                let e = self.subject(tx, at, subject)?;
                let Some(&vt) = self.plan.attr_types.get(attr) else {
                    return Err(store(pos(), Error::UnknownAttribute(attr.clone())));
                };
                let v = self.value(tx, at, attr, vt, value)?;
                let res = match op {
                    Op::Assert => tx.assert_with_precision(e, attr, v, *tick, *precision),
                    Op::Retract => tx.retract_with_precision(e, attr, v, *tick, *precision),
                }
                .map_err(|e| store(pos(), e))?;
                Ok((res.changed, label.as_deref().map(|l| (l, vt))))
            }
            POp::Correct { label, fix } => {
                let (entry, vt) = match self.labels.get(label) {
                    Some(Labelled::Stored(entry, vt)) => (*entry, *vt),
                    Some(Labelled::Nothing) => {
                        return Err(invalid(
                            pos(),
                            format!(
                                "label `{}` names an op that stored nothing (0 changed)",
                                Echo(label)
                            ),
                        ));
                    }
                    None if self.plan.labels.contains(label) => {
                        return Err(invalid(
                            pos(),
                            format!(
                                "label `{}` is corrected before its transaction committed",
                                Echo(label)
                            ),
                        ));
                    }
                    None => {
                        return Err(err(pos(), FixtureErrorKind::UnknownLabel(label.clone())));
                    }
                };
                let c = match fix {
                    PFix::Delete => Correction::Delete,
                    PFix::Replace {
                        value,
                        tick,
                        op,
                        precision,
                    } => Correction::Replace {
                        value: match value {
                            None => None,
                            Some(item) => {
                                Some(self.value(tx, at, "the replaced value", vt, item)?)
                            }
                        },
                        tick: *tick,
                        op: *op,
                        precision: *precision,
                    },
                };
                let res = tx.correct(entry, c).map_err(|e| store(pos(), e))?;
                Ok((res.changed, None))
            }
        }
    }

    fn entity(&self, at: &OpRef, handle: &str) -> Result<EntityId, FixtureError> {
        self.handles.get(handle).copied().ok_or_else(|| {
            err(
                Some(at.clone()),
                FixtureErrorKind::UnknownHandle(handle.to_owned()),
            )
        })
    }

    fn edge(
        &self,
        tx: &Txn<'_>,
        at: &OpRef,
        (s, a, t): (&str, &str, &str),
    ) -> Result<EntityId, FixtureError> {
        let (source, target) = (self.entity(at, s)?, self.entity(at, t)?);
        tx.edge_id(source, a, target)
            .map(EntityId::from)
            .ok_or_else(|| {
                err(
                    Some(at.clone()),
                    FixtureErrorKind::UnknownEdge(format!("[{s}, {a}, {t}]")),
                )
            })
    }

    fn subject(&self, tx: &Txn<'_>, at: &OpRef, s: &Subject) -> Result<EntityId, FixtureError> {
        match s {
            Subject::Handle(h) => self.entity(at, h),
            Subject::Edge(s, a, t) => self.edge(tx, at, (s, a, t)),
        }
    }

    /// A value of type `vt` from the YAML item (a handle or an edge triple
    /// for `ref`).
    fn value(
        &self,
        tx: &Txn<'_>,
        at: &OpRef,
        attr: &str,
        vt: ValueType,
        item: &Item,
    ) -> Result<Value, FixtureError> {
        let bad = |want: &str, got: &str| {
            invalid(
                Some(at.clone()),
                format!("the value of `{}` must be {want}, not {got}", Echo(attr)),
            )
        };
        let s = match item {
            Item::Scalar(s) => s,
            Item::List(l) if vt == ValueType::Ref => {
                let (s, a, t) = to_triple(l).map_err(|m| invalid(Some(at.clone()), m))?;
                return Ok(Value::Ref(self.edge(tx, at, (&s, &a, &t))?));
            }
            Item::List(_) => return Err(bad(want(vt), "a list")),
        };
        Ok(match (vt, s) {
            (ValueType::Bool, Scalar::Bool(b)) => Value::Bool(*b),
            (ValueType::I64, Scalar::Int(i)) => match i64::try_from(*i) {
                Ok(i) => Value::I64(i),
                Err(_) => return Err(bad(want(vt), "an integer outside the i64 range")),
            },
            (ValueType::F64, Scalar::Float(f)) => Value::F64(*f),
            (ValueType::F64, Scalar::Int(i)) => match exact_f64(*i) {
                Some(f) => Value::F64(f),
                None => {
                    return Err(bad(
                        want(vt),
                        "an integer no f64 holds exactly (|n| > 2^53)",
                    ));
                }
            },
            (ValueType::Str, Scalar::Str(v)) => Value::Str(v.clone()),
            (ValueType::Ref, Scalar::Str(_)) => {
                let h = to_name(s, "a handle").map_err(|m| invalid(Some(at.clone()), m))?;
                Value::Ref(self.entity(at, &h)?)
            }
            (_, other) => return Err(bad(want(vt), other.describe())),
        })
    }
}

/// What a value of type `vt` is written as, for messages.
fn want(vt: ValueType) -> &'static str {
    match vt {
        ValueType::Bool => "true or false",
        ValueType::I64 => "an integer",
        ValueType::F64 => "a number",
        ValueType::Str => "a string",
        ValueType::Ref => "a handle or an edge triple",
    }
}

/// `i` as an f64 if every f64 conversion of it is exact (`|i| <= 2^53`).
fn exact_f64(i: i128) -> Option<f64> {
    let i = i64::try_from(i).ok()?;
    // Exact: integers up to 2^53 in magnitude are all representable, so this
    // conversion never rounds.
    (-MAX_EXACT_F64_INT..=MAX_EXACT_F64_INT)
        .contains(&i)
        .then_some(i as f64)
}
