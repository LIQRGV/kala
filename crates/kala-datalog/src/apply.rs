//! Datalog writes lowered to native primitives, with no adapter or I/O policy.
use crate::ast::{Item, WriteOp, WriteTxn, WriteValue};
use kala_core::write::{StagedWrite, WriteOperation, WriteReport};
use kala_core::{Correction, EntityId, Op, Precision, Tick, Txn, Value, World};
use std::collections::HashMap;

/// A failure's position and typed cause. Adapters choose formatting and exit codes.
#[derive(Debug)]
pub struct WriteError {
    pub operation: Option<(usize, &'static str)>,
    pub cause: Failure,
}
/// Typed write failures, retaining native errors and migration labels for adapters.
#[derive(Debug)]
pub enum Failure {
    Store(kala_core::Error),
    Migration {
        error: kala_core::Error,
        labels: HashMap<EntityId, String>,
    },
    UnknownEntity(String),
    UnknownEdge,
    ExpectedReference,
    ExpectedFact,
    ExpectedCorrection,
    Genesis,
}
impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((index, verb)) = self.operation {
            write!(f, "op {index} (:{verb}): ")?;
        }
        match &self.cause {
            Failure::Store(e) | Failure::Migration { error: e, .. } => write!(f, "{e}"),
            Failure::UnknownEntity(s) => write!(f, "unknown entity `{}`", kala_core::Echo(s)),
            Failure::UnknownEdge => {
                f.write_str("unknown edge triple (the edge must already exist)")
            }
            Failure::ExpectedReference => f.write_str("expected a reference"),
            Failure::ExpectedFact => f.write_str("expected a fact"),
            Failure::ExpectedCorrection => f.write_str("expected a correction"),
            Failure::Genesis => f.write_str(":genesis must be first; use kala transact --create"),
        }
    }
}
impl std::error::Error for WriteError {}
fn migration_failure(tx: &Txn<'_>, error: kala_core::Error) -> Failure {
    let mut labels = HashMap::new();
    let mut remember = |id| {
        labels.insert(
            id,
            tx.handle_of(id)
                .map(str::to_owned)
                .unwrap_or_else(|| tx.format_id(id)),
        );
    };
    match &error {
        kala_core::Error::Unconvertible { examples, .. } => {
            for x in examples {
                remember(x.entity.id);
                if let Value::Ref(id) = x.value {
                    remember(id);
                }
            }
        }
        kala_core::Error::PairsBothWays { pairs, .. } => {
            for (a, b) in pairs {
                remember(a.id);
                remember(b.id);
            }
        }
        _ => return Failure::Store(error),
    }
    Failure::Migration { error, labels }
}

fn kind(op: &WriteOp) -> &'static str {
    match op {
        WriteOp::Attribute { .. } => "attribute",
        WriteOp::NodeType { .. } => "node-type",
        WriteOp::Rename { .. } => "rename",
        WriteOp::Deprecate { .. } => "deprecate",
        WriteOp::Cardinality { .. } => "cardinality",
        WriteOp::Migrate { .. } => "migrate",
        WriteOp::WorldSetting { .. } => "world-setting",
        WriteOp::Create { .. } => "create",
        WriteOp::Fact { op: Op::Assert, .. } => "assert",
        WriteOp::Fact {
            op: Op::Retract, ..
        } => "retract",
        WriteOp::Correct { .. } => "correct",
    }
}
fn reference(tx: &Txn<'_>, i: &Item) -> Result<EntityId, Failure> {
    match i {
        Item::Keyword(s, _) => tx
            .entity_by_handle(s)
            .ok_or_else(|| Failure::UnknownEntity(format!(":{s}"))),
        Item::Id(s, _) => tx
            .parse_id(s)
            .ok_or_else(|| Failure::UnknownEntity(s.clone())),
        _ => Err(Failure::ExpectedReference),
    }
}
fn subject(tx: &Txn<'_>, v: &WriteValue) -> Result<EntityId, Failure> {
    match v {
        WriteValue::Scalar(Item::Id(s, _)) => tx
            .parse_id(s)
            .ok_or_else(|| Failure::UnknownEntity(s.clone())),
        WriteValue::Scalar(i) => reference(tx, i),
        WriteValue::Edge {
            source,
            attr,
            target,
        } => tx
            .edge_id(reference(tx, source)?, &attr.name, reference(tx, target)?)
            .map(EntityId::from)
            .ok_or_else(|| Failure::UnknownEdge),
    }
}
fn value(tx: &Txn<'_>, v: &WriteValue) -> Result<Value, Failure> {
    Ok(match v {
        WriteValue::Scalar(Item::Str(s, _)) => Value::Str(s.clone()),
        WriteValue::Scalar(Item::Int(n, _)) => Value::I64(*n),
        WriteValue::Scalar(Item::Float(n, _)) => {
            Value::f64(*n).map_err(|e| Failure::Store(kala_core::Error::Value(e)))?
        }
        WriteValue::Scalar(Item::Bool(b, _)) => Value::Bool(*b),
        _ => Value::Ref(subject(tx, v)?),
    })
}

/// Stage and commit one form. Authorization and file recovery belong to the caller.
pub fn execute(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
) -> Result<WriteReport, WriteError> {
    let mut tx = w.begin();
    let staged = stage(&mut tx, form, default)?;
    let result = tx.commit().map_err(|e| WriteError {
        operation: None,
        cause: Failure::Store(e),
    })?;
    Ok(staged.complete(&result))
}

/// Stage into a caller-owned transaction without committing. Every error poisons
/// it, including name-resolution errors, so no partial form can be committed.
pub fn stage(
    tx: &mut Txn<'_>,
    form: &WriteTxn,
    default: Option<Tick>,
) -> Result<StagedWrite, WriteError> {
    let result = stage_form(tx, form, default);
    if result.is_err() {
        tx.abort();
    }
    result
}
fn stage_form(
    tx: &mut Txn<'_>,
    form: &WriteTxn,
    default: Option<Tick>,
) -> Result<StagedWrite, WriteError> {
    if form.genesis {
        return Err(WriteError {
            operation: None,
            cause: Failure::Genesis,
        });
    }
    let state = FormState {
        fallback: tx.write_tick(form.at.or(default)),
        precisions: form
            .ops
            .iter()
            .map(|op| match op {
                WriteOp::Correct { entry, .. } => {
                    tx.current_change(*entry).ok().map(|c| c.precision)
                }
                _ => None,
            })
            .collect(),
    };
    let mut state = state;
    let mut report = StagedWrite {
        ops: Vec::new(),
        entry_offset: tx.staged_entry_count(),
    };
    for (index, op) in form.ops.iter().enumerate() {
        let schema_before = tx.schema_changes().len();
        let staged = stage_op(tx, op, index, &mut state).map_err(|cause| WriteError {
            operation: Some((index + 1, kind(op))),
            cause,
        })?;
        report.ops.push(WriteOperation {
            position: index + 1,
            kind: kind(op),
            tick: staged.tick,
            precision: staged.precision,
            changed: staged.changed,
            declared: tx.schema_changes().len() - schema_before,
            entries: Vec::new(),
            entity: staged.entity,
        });
    }
    Ok(report)
}
struct FormState {
    fallback: Tick,
    precisions: Vec<Option<Precision>>,
}

/// What staging one op did.
struct Staged {
    changed: usize,
    tick: Tick,
    precision: Option<Precision>,
    /// The entity a `:create` staged.
    entity: Option<EntityId>,
}

impl Staged {
    /// A schema op: no facts, reported at the form's fallback tick.
    fn schema(fallback: Tick) -> Staged {
        Staged {
            changed: 0,
            tick: fallback,
            precision: None,
            entity: None,
        }
    }
}

/// Stages one op in the native transaction.
fn stage_op(
    tx: &mut Txn<'_>,
    op: &WriteOp,
    index: usize,
    state: &mut FormState,
) -> Result<Staged, Failure> {
    let fallback = state.fallback;
    match op {
        WriteOp::Attribute {
            name,
            value_type,
            options,
            inferred,
            ..
        } => {
            if *inferred {
                tx.infer_attribute_with(name, *value_type, options)
                    .map_err(Failure::Store)?;
            } else {
                tx.declare_attribute_with(name, *value_type, options)
                    .map_err(Failure::Store)?;
            }
            Ok(Staged::schema(fallback))
        }
        WriteOp::NodeType { name, inferred, .. } => {
            if *inferred {
                tx.infer_node_type(name).map_err(Failure::Store)?;
            } else {
                tx.declare_node_type(name).map_err(Failure::Store)?;
            }
            Ok(Staged::schema(fallback))
        }
        WriteOp::Rename { old, new, .. } => {
            tx.rename_attribute(old, new).map_err(Failure::Store)?;
            Ok(Staged::schema(fallback))
        }
        WriteOp::Deprecate { name, .. } => {
            tx.deprecate_attribute(name).map_err(Failure::Store)?;
            Ok(Staged::schema(fallback))
        }
        WriteOp::Cardinality {
            name, cardinality, ..
        } => {
            tx.set_cardinality(name, *cardinality)
                .map_err(Failure::Store)?;
            Ok(Staged::schema(fallback))
        }
        WriteOp::WorldSetting { key, value, .. } => {
            tx.world_setting(key, value).map_err(Failure::Store)?;
            Ok(Staged::schema(fallback))
        }
        WriteOp::Migrate { name, options, .. } => {
            let report = tx
                .migrate_attribute(name, options.clone())
                .map_err(|e| migration_failure(tx, e))?;
            Ok(Staged {
                changed: report.change_points + report.edge_properties,
                ..Staged::schema(fallback)
            })
        }
        WriteOp::Create {
            node_type, handle, ..
        } => stage_create(tx, node_type, handle.as_deref(), state),
        WriteOp::Fact { .. } => stage_fact(tx, op, state),
        WriteOp::Correct { .. } => stage_correct(tx, op, index, state),
    }
}

/// `[:create TYPE handle]`: later ops of the form may refer to the entity.
fn stage_create(
    tx: &mut Txn<'_>,
    node_type: &str,
    handle: Option<&str>,
    state: &mut FormState,
) -> Result<Staged, Failure> {
    tx.infer_node_type(node_type).map_err(Failure::Store)?;
    let id = tx
        .create_entity(node_type, handle)
        .map_err(Failure::Store)?;
    Ok(Staged {
        entity: Some(id),
        ..Staged::schema(state.fallback)
    })
}

/// `[:assert ...]` / `[:retract ...]`.
fn stage_fact(tx: &mut Txn<'_>, op: &WriteOp, state: &FormState) -> Result<Staged, Failure> {
    let WriteOp::Fact {
        op,
        subject: s,
        attr,
        value: v,
        at,
        precision,
        ..
    } = op
    else {
        return Err(Failure::ExpectedFact);
    };
    let e = subject(tx, s)?;
    let v = value(tx, v)?;
    if !tx
        .infer_write_attribute(&attr.name, &v, *op)
        .map_err(Failure::Store)?
    {
        return Ok(Staged {
            tick: at.unwrap_or(state.fallback),
            ..Staged::schema(state.fallback)
        });
    }
    let r = match op {
        Op::Assert => tx.assert_with_precision(
            e,
            &attr.name,
            v,
            Some(at.unwrap_or(state.fallback)),
            *precision,
        ),
        Op::Retract => tx.retract_with_precision(
            e,
            &attr.name,
            v,
            Some(at.unwrap_or(state.fallback)),
            *precision,
        ),
    }
    .map_err(Failure::Store)?;
    // A no-op stores nothing, so it reports no precision (changing a
    // held fact's precision needs `:correct`).
    Ok(Staged {
        changed: r.changed,
        tick: r.tick,
        precision: (r.changed > 0).then_some(*precision),
        entity: None,
    })
}

/// `[:correct ENTRY ...]`.
fn stage_correct(
    tx: &mut Txn<'_>,
    op: &WriteOp,
    index: usize,
    state: &FormState,
) -> Result<Staged, Failure> {
    let WriteOp::Correct {
        entry, replacement, ..
    } = op
    else {
        return Err(Failure::ExpectedCorrection);
    };
    let c = match replacement {
        None => Correction::Delete,
        Some(r) => Correction::Replace {
            value: r.value.as_ref().map(|v| value(tx, v)).transpose()?,
            tick: r.at,
            op: r.op,
            precision: r.precision,
        },
    };
    let r = tx.correct(*entry, c).map_err(Failure::Store)?;
    // The chain's stored precision, so a no-op or delete
    // still reports the truth.
    Ok(Staged {
        changed: r.changed,
        tick: r.tick,
        precision: Some(
            replacement
                .as_ref()
                .and_then(|r| r.precision)
                .or(state.precisions[index])
                .unwrap_or(Precision::Exact),
        ),
        entity: None,
    })
}
