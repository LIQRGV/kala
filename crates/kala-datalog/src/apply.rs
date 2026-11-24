//! Datalog writes lowered to native primitives, with no adapter or I/O policy.
use crate::ast::{Item, WriteOp, WriteTxn, WriteValue};
use kala_core::write::{StagedWrite, WriteOperation, WriteReport};
use kala_core::{
    Concurrency, Correction, Echo, EntityId, EntryId, Op, Precision, Tick, TickOccupant, Txn,
    Value, World,
};
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
        error: Box<kala_core::Error>,
        labels: HashMap<EntityId, String>,
    },
    UnknownEntity(String),
    UnknownEdge,
    ExpectedReference,
    ExpectedFact,
    ExpectedCorrection,
    Genesis,
    /// The world is Kala's `system` database and the caller's [`WritePolicy`]
    /// does not allow writing it.
    SystemWorld,
    /// Empty replacements cannot infer an attribute's type.
    EmptyUnknownAttribute(String),
    /// Events can only be removed from canon through a correction.
    SetEvent(String),
    /// Replacement work exceeded the documented per-form budget.
    Limit,
    /// The attribute holds more values at the tick than one `:set` reads.
    SetTooManyHeld {
        attr: String,
        tick: Tick,
    },
    /// The store's same-tick conflict: the opposite op is already stored for
    /// this value at this tick.
    SameTick(Box<SameTick>),
}
/// A same-tick conflict as an author can read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SameTick {
    /// The subject as an author names it: its handle, else `TYPE#n` /
    /// `edge#n`. Safe to print.
    pub subject: String,
    pub attr: String,
    pub tick: Tick,
    /// What holds the tick, when `:set` could find it within its read
    /// budget. `None` for plain `:assert` / `:retract`.
    pub occupant: Option<Occupant>,
}
/// The stored op a `:set` collided with at its tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occupant {
    /// The value, as text safe to print (strings quoted, refs by label).
    pub value: String,
    /// What is stored for it at the tick.
    pub op: Op,
    pub by: OccupiedBy,
}
/// Who wrote the op a `:set` collided with. Only a committed entry has an ID
/// that may be named in a `[:correct …]`: a staged one has none yet, and the
/// ID it would get is reused by an unrelated entry when the form aborts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccupiedBy {
    /// A committed log entry.
    Entry(EntryId),
    /// An earlier operation of this form (1-based position).
    Operation(usize),
    /// A form staged earlier into the same caller-owned transaction.
    EarlierForm,
}
impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some((index, verb)) = self.operation {
            write!(f, "op {index} (:{verb}): ")?;
        }
        match &self.cause {
            Failure::Store(e) => write!(f, "{e}"),
            Failure::Migration { error, .. } => write!(f, "{error}"),
            Failure::UnknownEntity(s) => write!(f, "unknown entity `{}`", kala_core::Echo(s)),
            Failure::UnknownEdge => {
                f.write_str("unknown edge triple (the edge must already exist)")
            }
            Failure::ExpectedReference => f.write_str("expected a reference"),
            Failure::ExpectedFact => f.write_str("expected a fact"),
            Failure::ExpectedCorrection => f.write_str("expected a correction"),
            Failure::EmptyUnknownAttribute(name) => write!(
                f,
                "cannot set unknown attribute :{} to []; declare it first",
                kala_core::Echo(name)
            ),
            Failure::SetEvent(name) => write!(
                f,
                "cannot set event :{}: events are never retracted; use [:correct …]",
                kala_core::Echo(name)
            ),
            Failure::Limit => write!(
                f,
                ":set exceeds the {}-operation/value limit, 16 MiB per-form value budget, or {}-key read budget",
                crate::MAX_WRITE_OPS,
                MAX_SET_READ_KEYS
            ),
            Failure::SetTooManyHeld { attr, tick } => write!(
                f,
                "cannot set :{}: it holds more than {} values at tick {} (the :set limit); use :retract/:assert",
                Echo(attr),
                crate::MAX_WRITE_OPS,
                tick.0
            ),
            Failure::SameTick(conflict) => {
                let SameTick {
                    subject,
                    attr,
                    tick,
                    occupant,
                } = conflict.as_ref();
                write!(
                    f,
                    "same-tick conflict: the opposite op is already stored for {subject} \"{}\" at tick {}",
                    Echo(attr),
                    tick.0
                )?;
                let Some(Occupant { value, op, by }) = occupant else {
                    return Ok(());
                };
                let (did, noun) = match op {
                    Op::Assert => ("asserted", "assert"),
                    Op::Retract => ("retracted", "retract"),
                };
                write!(f, "; {value} was {did} at that tick by ")?;
                match by {
                    // Deleting the entry always resolves the conflict: the
                    // value then keeps the state it had before that tick.
                    // Replacing the value only helps for an assert (it was a
                    // different value that started at that tick).
                    OccupiedBy::Entry(id) => {
                        let id = id.0;
                        write!(
                            f,
                            "entry#{id}: use [:correct {id} :delete] to remove that {noun}"
                        )?;
                        if *op == Op::Assert {
                            write!(
                                f,
                                ", or [:correct {id} :replace :value …] to change its value"
                            )?;
                        }
                        Ok(())
                    }
                    OccupiedBy::Operation(n) => write!(
                        f,
                        "op {n} of this transaction: change that operation instead"
                    ),
                    OccupiedBy::EarlierForm => f.write_str(
                        "an earlier operation of this transaction: change that operation instead",
                    ),
                }
            }
            Failure::SystemWorld => {
                f.write_str("the system database is written only through account management")
            }
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
    Failure::Migration {
        error: Box::new(error),
        labels,
    }
}

/// An entity as an author names it: its handle, else `TYPE#n` / `edge#n`.
fn label(tx: &Txn<'_>, id: EntityId) -> String {
    match tx.handle_of(id) {
        Some(handle) => Echo(handle).to_string(),
        None => tx.format_id(id),
    }
}
/// A value for a one-line message: strings quoted and escaped (long ones
/// cut), refs by label.
fn value_text(tx: &Txn<'_>, value: &Value) -> String {
    match value {
        Value::Str(s) => format!(
            "\"{}\"",
            Echo(&s.replace('\\', "\\\\").replace('"', "\\\""))
        ),
        Value::Ref(id) => label(tx, *id),
        Value::I64(n) => n.to_string(),
        Value::F64(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
    }
}
/// The store's same-tick conflict as the applier reports it, with the subject
/// the author wrote. Other errors pass through.
fn fact_failure(tx: &Txn<'_>, subject: EntityId, error: kala_core::Error) -> Failure {
    match error {
        kala_core::Error::SameTickConflict { attr, tick, .. } => {
            Failure::SameTick(Box::new(SameTick {
                subject: label(tx, subject),
                attr,
                tick,
                occupant: None,
            }))
        }
        error => Failure::Store(error),
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

/// What the caller lets a write form touch. The default refuses Kala's own
/// `system` database: accounts change only through account management
/// (`kala user`), whatever grants an identity holds, so an adapter that
/// forgets its own check still cannot write them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WritePolicy {
    /// Apply forms to a system-marked world ([`World::is_system`]). Only for
    /// a caller that owns the file, such as restoring an accounts dump into a
    /// new private file.
    pub allow_system: bool,
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
    tx.set_concurrency(mode);
    let staged = stage_with(&mut tx, form, default, policy)?;
    commit(tx, staged)
}

/// Commit a transaction holding one staged form and report it.
///
/// A commit refused because a newer transaction wrote something this one
/// read ([`kala_core::Error::Conflict`]) is reported at the op that made
/// the read, when the form made it ([`StagedWrite::operation_of`]):
/// `op 2 (:set): conflict: …`. Nothing was written; the caller may stage
/// and commit the same form again on the caught-up world.
pub fn commit(tx: Txn<'_>, staged: StagedWrite) -> Result<WriteReport, WriteError> {
    match tx.commit() {
        Ok(result) => Ok(staged.complete(&result)),
        Err(error) => {
            let operation = match &error {
                kala_core::Error::Conflict(conflict) => staged
                    .operation_of(conflict.operation)
                    .map(|op| (op.position, op.kind)),
                _ => None,
            };
            Err(WriteError {
                operation,
                cause: Failure::Store(error),
            })
        }
    }
}

/// Stage into a caller-owned transaction without committing. Every error poisons
/// it, including name-resolution errors, so no partial form can be committed.
///
/// A form that begins with `[:concurrency …]` sets the transaction's mode
/// ([`Txn::set_concurrency`]) before anything is staged; a form that
/// doesn't leaves the mode the caller chose. The mode belongs to the whole
/// transaction: a caller staging several forms into one should let only
/// the first name it (going weak drops the reads recorded so far; going
/// back to optimistic counts as having read everything).
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
    // Checked before anything is resolved or staged, so a refusal reveals
    // nothing about the accounts (not even whether a handle exists).
    let result = if tx.is_system() && !policy.allow_system {
        Err(WriteError {
            operation: None,
            cause: Failure::SystemWorld,
        })
    } else {
        if let Some(mode) = form.concurrency {
            tx.set_concurrency(mode);
        }
        stage_form(tx, form, default)
    };
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
    let has_set = form.ops.iter().any(|op| matches!(op, WriteOp::Set { .. }));
    if has_set && form.ops.len() > crate::MAX_WRITE_OPS {
        return Err(WriteError {
            operation: None,
            cause: Failure::Limit,
        });
    }
    let state = FormState {
        remaining: crate::MAX_WRITE_OPS,
        remaining_bytes: kala_core::MAX_ENTRY_BYTES,
        read_meter: kala_core::WorkMeter::bounded(MAX_SET_READ_KEYS, None),
        form_tick: form.at.or(default),
        fallback: tx.report_tick(form.at.or(default)),
        entry_offset: tx.staged_entry_count(),
        op_starts: Vec::with_capacity(form.ops.len()),
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
        spans: Vec::with_capacity(form.ops.len()),
    };
    for (index, op) in form.ops.iter().enumerate() {
        let schema_before = tx.schema_changes().len();
        let first_operation = tx.operation_count();
        state.op_starts.push(tx.staged_entry_count());
        let staged = stage_op(tx, op, index, &mut state).map_err(|cause| WriteError {
            operation: Some((index + 1, kind(op))),
            cause,
        })?;
        if has_set {
            let cost = staged.changed.max(1);
            if cost > state.remaining {
                return Err(WriteError {
                    operation: Some((index + 1, kind(op))),
                    cause: Failure::Limit,
                });
            }
            state.remaining -= cost;
        }
        report.ops.push(WriteOperation {
            position: index + 1,
            kind: kind(op),
            tick: staged.tick,
            precision: staged.precision,
            changed: staged.changed,
            declared: tx.schema_changes().len() - schema_before,
            entries: Vec::new(),
            entity: staged.entity,
            created: staged.created,
        });
        report.spans.push(first_operation..tx.operation_count());
    }
    Ok(report)
}
/// A form's replacements share a key-work budget, including historical values
/// and staged reverse edges. The write/value limit is MAX_WRITE_OPS (256).
pub const MAX_SET_READ_KEYS: u64 = crate::MAX_WRITE_OPS as u64 * 256;
struct FormState {
    remaining: usize,
    remaining_bytes: usize,
    read_meter: kala_core::WorkMeter<'static>,
    /// The form's `:at`, else the caller's default tick.
    form_tick: Option<Tick>,
    /// The tick an op with no `:at` is reported at: `form_tick`, else the
    /// transaction's own default (its starting latest tick). Ops that store
    /// a tick resolve it with [`FormState::fact_tick`] instead.
    fallback: Tick,
    /// Staged entries of the transaction before this form.
    entry_offset: usize,
    /// For each op begun so far, the transaction's staged entry count when
    /// it began: maps a staged entry back to the op that wrote it.
    op_starts: Vec<usize>,
    precisions: Vec<Option<Precision>>,
}
impl FormState {
    /// The tick of a fact op: its own `:at`, else the form's, else the
    /// transaction's default. Resolved through the transaction, so a fact
    /// that lands on the latest tick the transaction began with is known
    /// to it (a commit over a newer latest tick then warns); creates and
    /// schema ops, which store no tick, are only *reported* at
    /// `fallback` and never raise that warning.
    fn fact_tick(&self, tx: &Txn<'_>, at: Option<Tick>) -> Tick {
        at.unwrap_or_else(|| tx.write_tick(self.form_tick))
    }
    /// Who staged the transaction's entry at `position`.
    fn staged_by(&self, position: usize) -> OccupiedBy {
        if position < self.entry_offset {
            return OccupiedBy::EarlierForm;
        }
        // 1-based: the last op that began at or before the entry.
        OccupiedBy::Operation(self.op_starts.partition_point(|start| *start <= position))
    }
}

/// What staging one op did.
struct Staged {
    changed: usize,
    tick: Tick,
    precision: Option<Precision>,
    /// The entity a `:create` staged.
    entity: Option<EntityId>,
    created: Option<bool>,
}

impl Staged {
    /// A schema op: no facts, reported at the form's fallback tick.
    fn schema(fallback: Tick) -> Staged {
        Staged {
            changed: 0,
            tick: fallback,
            precision: None,
            entity: None,
            created: None,
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
            node_type,
            handle,
            if_missing,
            ..
        } => stage_create(tx, node_type, handle.as_deref(), *if_missing, state),
        WriteOp::Set { .. } => stage_set(tx, op, state),
        WriteOp::Fact { .. } => stage_fact(tx, op, state),
        WriteOp::Correct { .. } => stage_correct(tx, op, index, state),
    }
}

/// `[:create TYPE handle]`: later ops of the form may refer to the entity.
fn stage_create(
    tx: &mut Txn<'_>,
    node_type: &str,
    handle: Option<&str>,
    if_missing: bool,
    state: &mut FormState,
) -> Result<Staged, Failure> {
    tx.infer_node_type(node_type).map_err(Failure::Store)?;
    let (id, created) = if if_missing {
        let handle = handle.ok_or(Failure::ExpectedReference)?;
        tx.find_or_create_entity(node_type, handle)
            .map_err(Failure::Store)?
    } else {
        (
            tx.create_entity(node_type, handle)
                .map_err(Failure::Store)?,
            true,
        )
    };
    Ok(Staged {
        entity: Some(id),
        created: if_missing.then_some(created),
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
    let tick = state.fact_tick(tx, *at);
    if !tx
        .infer_write_attribute(&attr.name, &v, *op)
        .map_err(Failure::Store)?
    {
        return Ok(Staged {
            tick,
            ..Staged::schema(state.fallback)
        });
    }
    let r = match op {
        Op::Assert => tx.assert_with_precision(e, &attr.name, v, Some(tick), *precision),
        Op::Retract => tx.retract_with_precision(e, &attr.name, v, Some(tick), *precision),
    }
    .map_err(|error| fact_failure(tx, e, error))?;
    // A no-op stores nothing, so it reports no precision (changing a
    // held fact's precision needs `:correct`).
    Ok(Staged {
        changed: r.changed,
        tick: r.tick,
        precision: (r.changed > 0).then_some(*precision),
        entity: None,
        created: None,
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
        created: None,
    })
}

/// Replace by difference, retaining existing members and their precision.
fn stage_set(tx: &mut Txn<'_>, op: &WriteOp, state: &mut FormState) -> Result<Staged, Failure> {
    let WriteOp::Set {
        subject: s,
        attr,
        values,
        at,
        precision,
        ..
    } = op
    else {
        return Err(Failure::ExpectedFact);
    };
    if values.len() > crate::MAX_WRITE_OPS {
        return Err(Failure::Limit);
    }
    let entity = subject(tx, s)?;
    let tick = state.fact_tick(tx, *at);
    let mut wanted = std::collections::BTreeMap::new();
    for v in values {
        // Caller-built ASTs bypass the text parser: bound strings before cloning
        // or encoding them. Account for NUL escaping in the stored encoding.
        let size = match v {
            WriteValue::Scalar(Item::Str(s, _)) => {
                if s.len() > state.remaining_bytes {
                    return Err(Failure::Limit);
                }
                s.len()
                    .saturating_add(s.bytes().filter(|b| *b == 0).count())
                    .saturating_add(3)
            }
            WriteValue::Scalar(Item::Bool(..)) => 2,
            _ => 9,
        };
        if size > state.remaining_bytes {
            return Err(Failure::Limit);
        }
        state.remaining_bytes -= size;
        let v = value(tx, v)?;
        let key = v
            .encode()
            .map_err(|e| Failure::Store(kala_core::Error::Value(e)))?;
        wanted.insert(key, v);
    }
    if let Some(v) = wanted.values().next() {
        tx.infer_write_attribute(&attr.name, v, Op::Assert)
            .map_err(Failure::Store)?;
    }
    let def = tx
        .attribute(&attr.name)
        .ok_or_else(|| Failure::EmptyUnknownAttribute(attr.name.clone()))?;
    if def.deprecated {
        return Err(Failure::Store(kala_core::Error::DeprecatedAttribute(
            attr.name.clone(),
        )));
    }
    if def.kind == kala_core::Kind::Event {
        return Err(Failure::SetEvent(attr.name.clone()));
    }
    for v in wanted.values() {
        if v.value_type() != def.value_type {
            return Err(Failure::Store(kala_core::Error::WrongValueType {
                attr: attr.name.clone(),
                expected: def.value_type,
                got: v.value_type(),
            }));
        }
    }
    let held = tx
        .values_at(
            entity,
            &attr.name,
            tick,
            crate::MAX_WRITE_OPS,
            state.remaining_bytes,
            &state.read_meter,
        )
        .map_err(|e| {
            if matches!(e, kala_core::Error::WorkInterrupted { .. }) {
                Failure::Limit
            } else {
                Failure::Store(e)
            }
        })?;
    if held.more {
        return Err(Failure::SetTooManyHeld {
            attr: attr.name.clone(),
            tick,
        });
    }
    let mut removed = Vec::new();
    for value in held.values {
        let key = value
            .encode()
            .map_err(|e| Failure::Store(kala_core::Error::Value(e)))?;
        // `values_at` already kept the held values within the remaining
        // budget; a checked subtraction keeps that an error, never a wrap,
        // if the two ever disagree.
        state.remaining_bytes = state
            .remaining_bytes
            .checked_sub(key.len())
            .ok_or(Failure::Limit)?;
        if wanted.remove(&key).is_none() {
            removed.push(value);
        }
    }
    let changes = removed.len().saturating_add(wanted.len());
    if changes > state.remaining {
        return Err(Failure::Limit);
    }
    let mut changed = 0;
    for (op, value) in removed
        .into_iter()
        .map(|v| (Op::Retract, v))
        .chain(wanted.into_values().map(|v| (Op::Assert, v)))
    {
        let result = match op {
            Op::Assert => {
                tx.assert_with_precision(entity, &attr.name, value.clone(), Some(tick), *precision)
            }
            Op::Retract => {
                tx.retract_with_precision(entity, &attr.name, value.clone(), Some(tick), *precision)
            }
        };
        match result {
            Ok(r) => changed += r.changed,
            Err(kala_core::Error::SameTickConflict { .. }) => {
                // The lookup only explains the conflict. If it runs out of
                // the form's read budget (the entry sits in a very large
                // transaction), the conflict is still the error to report,
                // without the entry detail: a generic limit error would hide
                // the real cause.
                let found =
                    match tx.entry_at_tick(entity, &attr.name, &value, tick, &state.read_meter) {
                        Ok(found) => found,
                        Err(kala_core::Error::WorkInterrupted { .. }) => None,
                        Err(e) => return Err(Failure::Store(e)),
                    };
                let occupant = found.map(|found| {
                    let (op, by) = match found {
                        TickOccupant::Committed { entry, op } => (op, OccupiedBy::Entry(entry)),
                        TickOccupant::Staged { position, op } => (op, state.staged_by(position)),
                    };
                    Occupant {
                        value: value_text(tx, &value),
                        op,
                        by,
                    }
                });
                return Err(Failure::SameTick(Box::new(SameTick {
                    subject: label(tx, entity),
                    attr: attr.name.clone(),
                    tick,
                    occupant,
                })));
            }
            Err(error) => return Err(Failure::Store(error)),
        }
    }
    Ok(Staged {
        changed,
        tick,
        precision: (changed > 0).then_some(*precision),
        entity: None,
        created: None,
    })
}
