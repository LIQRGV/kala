//! Bounded YAML parsing into unvalidated fixture data.
use std::fmt;

use kala_core::Echo;
use serde::Deserialize;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_saphyr::options::{DuplicateKeyPolicy, MergeKeyPolicy, NonFiniteFloatPolicy};

use crate::MAX_FIXTURE_BYTES;
use crate::error::{FixtureError, FixtureErrorKind, err, invalid};

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
pub(super) fn parse(yaml: &str) -> Result<Option<RawDoc>, FixtureError> {
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
pub(super) struct NoTag<T>(pub(super) T);

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
pub(super) enum Scalar {
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
    pub(super) fn describe(&self) -> &'static str {
        match self {
            Scalar::Null => "null",
            Scalar::Bool(_) => "a boolean",
            Scalar::Int(_) | Scalar::BigInt => "an integer",
            Scalar::Float(_) => "a float",
            Scalar::Str(_) => "a string",
            Scalar::Other(what) => what,
        }
    }

    pub(super) fn as_str(&self) -> Option<&str> {
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
pub(super) enum Item {
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
pub(super) enum Args {
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
pub(super) struct RawEntities(pub(super) Vec<(Scalar, Item)>);

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
pub(super) struct RawDoc {
    #[serde(default, deserialize_with = "present")]
    pub(super) schema: Option<RawSchema>,
    #[serde(default, deserialize_with = "present")]
    pub(super) entities: Option<RawEntities>,
    #[serde(default, deserialize_with = "present")]
    pub(super) transactions: Option<Vec<NoTag<RawTxn>>>,
    #[serde(default, deserialize_with = "present")]
    pub(super) operations: Option<Vec<NoTag<RawOp>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawSchema {
    #[serde(default, deserialize_with = "present")]
    pub(super) node_types: Option<Vec<Scalar>>,
    #[serde(default, deserialize_with = "present")]
    pub(super) attributes: Option<Vec<NoTag<RawAttr>>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawAttr {
    #[serde(default, deserialize_with = "present")]
    pub(super) name: Option<Scalar>,
    #[serde(default, rename = "type", deserialize_with = "present")]
    pub(super) value_type: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) kind: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) cardinality: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) direction: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) inverse: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) reverse: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) indexed: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTxn {
    #[serde(default, deserialize_with = "present")]
    pub(super) tick: Option<Scalar>,
    #[serde(deserialize_with = "untagged")]
    pub(super) ops: Vec<NoTag<RawOp>>,
}

/// One op as written: every key optional, "exactly one verb" checked in code.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawOp {
    #[serde(default, deserialize_with = "present")]
    pub(super) assert: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    pub(super) retract: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    pub(super) create: Option<Args>,
    #[serde(default, deserialize_with = "present")]
    pub(super) correct: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) delete: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) replace: Option<RawReplace>,
    #[serde(default, deserialize_with = "present")]
    pub(super) tick: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) precision: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) label: Option<Scalar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawReplace {
    #[serde(default, deserialize_with = "present")]
    pub(super) value: Option<Item>,
    #[serde(default, deserialize_with = "present")]
    pub(super) tick: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) op: Option<Scalar>,
    #[serde(default, deserialize_with = "present")]
    pub(super) precision: Option<Scalar>,
}
