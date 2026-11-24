//! Validate fixture shape before opening a world.
use std::collections::{HashMap, HashSet};

use kala_core::schema::{AttrDef, Cardinality, Kind, Schema};
use kala_core::{Echo, Error, Op, Precision, Tick, ValueType};

use crate::error::{FixtureError, FixtureErrorKind, OpRef, err, invalid, store};
use crate::yaml::{
    Args, Item, NoTag, RawAttr, RawDoc, RawEntities, RawOp, RawSchema, Scalar, parse,
};

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

/// A fixture checked for shape: everything that needs no world.
#[derive(Debug)]
pub(super) struct Plan {
    pub(super) schema: Schema,
    /// Declared attributes' value types, by name.
    pub(super) attr_types: HashMap<String, ValueType>,
    /// The entities section, per node type in schema order.
    pub(super) entities: Vec<(String, Vec<String>)>,
    pub(super) txns: Vec<PTxn>,
    /// Every label an op defines (so a `correct` can tell "not yet" from
    /// "never").
    pub(super) labels: HashSet<String>,
}

#[derive(Debug)]
pub(super) struct PTxn {
    pub(super) tick: Option<Tick>,
    /// `(position, op)`.
    pub(super) ops: Vec<(OpRef, POp)>,
}

#[derive(Debug)]
pub(super) enum POp {
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
pub(super) enum PFix {
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
pub(super) enum Subject {
    Handle(String),
    Edge(String, String, String),
}

pub(super) fn plan(yaml: &str) -> Result<Plan, FixtureError> {
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
pub(super) fn to_name(s: &Scalar, what: &str) -> Result<String, String> {
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
pub(super) fn to_triple(l: &[Scalar]) -> Result<(String, String, String), String> {
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
