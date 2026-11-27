//! Lower Datalog's write forms to the shared, language-neutral stager.
use crate::ast::{Item, WriteOp, WriteTxn, WriteValue};
use kala_core::write::stage::{Effect, Stager};
pub use kala_core::write::stage::{
    Failure, MAX_SET_READ_KEYS, Occupant, OccupiedBy, SameTick, WriteError, WritePolicy, commit,
};
use kala_core::write::{StagedWrite, WriteReport};
use kala_core::{Concurrency, Correction, EntityId, Op, Precision, Tick, Txn, Value, World};
use std::collections::HashMap;

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
    Failure::Migration {
        error: Box::new(error),
        labels,
    }
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
        WriteOp::Set { .. } => "set",
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
            .ok_or(Failure::UnknownEdge),
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

/// Stage and commit one form under the default [`WritePolicy`] (a system
/// world is refused) and the default concurrency mode (optimistic, unless
/// the form names its own). Authorization and file recovery belong to the
/// caller.
pub fn execute(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
) -> Result<WriteReport, WriteError> {
    execute_with(w, form, default, WritePolicy::default())
}

/// [`execute`] under an explicit policy.
pub fn execute_with(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
    policy: WritePolicy,
) -> Result<WriteReport, WriteError> {
    execute_in(w, form, default, policy, Concurrency::default())
}

/// [`execute_with`] for a caller with its own default concurrency mode:
/// the transaction commits in `mode` unless the form begins with
/// `[:concurrency …]`, which wins.
pub fn execute_in(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
    policy: WritePolicy,
    mode: Concurrency,
) -> Result<WriteReport, WriteError> {
    let mut tx = w.begin();
    // Set once: switching modes on a transaction is for callers who mean
    // it (see `Txn::set_concurrency`).
    tx.set_concurrency(form.concurrency.unwrap_or(mode));
    let staged = stage_with(&mut tx, form, default, policy)?;
    commit(tx, staged)
}

/// Stage into a caller-owned transaction without committing. Every error poisons
/// it, including name-resolution errors, so no partial form can be committed.
///
/// A form that begins with `[:concurrency …]` sets the transaction's mode
/// ([`Txn::set_concurrency`]) before anything is staged; a form that
/// doesn't leaves the mode the caller chose. The mode belongs to the whole
/// transaction: a caller staging several forms into one should let only
/// the first name it (going weak drops the reads recorded so far; going
/// back to optimistic after a read made under weak counts as having read
/// everything).
///
/// Uses the default [`WritePolicy`]: a system world is refused.
pub fn stage(
    tx: &mut Txn<'_>,
    form: &WriteTxn,
    default: Option<Tick>,
) -> Result<StagedWrite, WriteError> {
    stage_with(tx, form, default, WritePolicy::default())
}

/// [`stage`] under an explicit policy.
pub fn stage_with(
    tx: &mut Txn<'_>,
    form: &WriteTxn,
    default: Option<Tick>,
    policy: WritePolicy,
) -> Result<StagedWrite, WriteError> {
    let has_set = form.ops.iter().any(|op| matches!(op, WriteOp::Set { .. }));
    let limit = if has_set {
        crate::MAX_WRITE_OPS
    } else {
        usize::MAX
    };
    let mut stager = Stager::for_form(tx, form.at.or(default), policy, limit)?;
    if let Some(mode) = form.concurrency {
        tx.set_concurrency(mode);
    }
    let result = (|| {
        if form.genesis || (has_set && form.ops.len() > limit) {
            return Err(WriteError {
                operation: None,
                cause: if form.genesis {
                    Failure::Genesis
                } else {
                    Failure::Limit
                },
            });
        }
        for (i, op) in form.ops.iter().enumerate() {
            let error = |cause| WriteError {
                operation: Some((i + 1, kind(op))),
                cause,
            };
            match op {
                WriteOp::Create {
                    node_type,
                    handle,
                    if_missing,
                    ..
                } => {
                    stager.create(tx, node_type, handle.as_deref(), *if_missing)?;
                }
                WriteOp::Fact {
                    subject: s,
                    attr,
                    value: v,
                    at,
                    op,
                    precision,
                    ..
                } => {
                    let e = subject(tx, s).map_err(error)?;
                    let v = value(tx, v).map_err(error)?;
                    stager.fact(tx, e, &attr.name, v, *op, *at, *precision)?;
                }
                WriteOp::Set {
                    subject: s,
                    attr,
                    values,
                    at,
                    precision,
                    ..
                } => {
                    if values.len() > crate::MAX_WRITE_OPS {
                        return Err(error(Failure::Limit));
                    }
                    let e = subject(tx, s).map_err(error)?;
                    // A caller-built AST can exceed the parser's byte limit. Check
                    // before cloning strings into the resolved input buffer.
                    let mut remaining = stager.remaining_bytes();
                    let mut resolved = Vec::with_capacity(values.len());
                    for v in values {
                        if let WriteValue::Scalar(Item::Str(s, _)) = v {
                            remaining = remaining
                                .checked_sub(s.len())
                                .ok_or_else(|| error(Failure::Limit))?;
                        }
                        resolved.push(value(tx, v).map_err(error)?);
                    }
                    stager.set(tx, e, &attr.name, &resolved, *at, *precision)?;
                }
                op => stager.other(tx, kind(op), |tx, fallback| stage_other(tx, op, fallback))?,
            }
        }
        Ok(stager.finish())
    })();
    if result.is_err() {
        tx.abort();
    }
    result
}

fn stage_other(tx: &mut Txn<'_>, op: &WriteOp, fallback: Tick) -> Result<Effect, Failure> {
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
            Ok(Effect::schema(fallback))
        }
        WriteOp::NodeType { name, inferred, .. } => {
            if *inferred {
                tx.infer_node_type(name).map_err(Failure::Store)?;
            } else {
                tx.declare_node_type(name).map_err(Failure::Store)?;
            }
            Ok(Effect::schema(fallback))
        }
        WriteOp::Rename { old, new, .. } => {
            tx.rename_attribute(old, new).map_err(Failure::Store)?;
            Ok(Effect::schema(fallback))
        }
        WriteOp::Deprecate { name, .. } => {
            tx.deprecate_attribute(name).map_err(Failure::Store)?;
            Ok(Effect::schema(fallback))
        }
        WriteOp::Cardinality {
            name, cardinality, ..
        } => {
            tx.set_cardinality(name, *cardinality)
                .map_err(Failure::Store)?;
            Ok(Effect::schema(fallback))
        }
        WriteOp::WorldSetting { key, value, .. } => {
            tx.world_setting(key, value).map_err(Failure::Store)?;
            Ok(Effect::schema(fallback))
        }
        WriteOp::Migrate { name, options, .. } => {
            let report = tx
                .migrate_attribute(name, options.clone())
                .map_err(|e| migration_failure(tx, e))?;
            Ok(Effect {
                changed: report.change_points + report.edge_properties,
                ..Effect::schema(fallback)
            })
        }

        WriteOp::Correct { .. } => stage_correct(tx, op),
        _ => Err(Failure::ExpectedFact),
    }
}
/// `[:correct ENTRY ...]`.
fn stage_correct(tx: &mut Txn<'_>, op: &WriteOp) -> Result<Effect, Failure> {
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
    // The chain's stored precision before the correction, so a no-op or a
    // delete still reports the truth. Read here, as part of this op: the
    // read of the chain is what an optimistic commit conflicts on, and it
    // must be reported at the `:correct` that made it, not at the form's
    // first op.
    let stored = tx.current_change(*entry).ok().map(|c| c.precision);
    let r = tx.correct(*entry, c).map_err(Failure::Store)?;
    Ok(Effect {
        changed: r.changed,
        tick: r.tick,
        precision: Some(
            replacement
                .as_ref()
                .and_then(|r| r.precision)
                .or(stored)
                .unwrap_or(Precision::Exact),
        ),
        entity: None,
        created: None,
    })
}
