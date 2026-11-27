//! Stage resolved operations into one native transaction, retaining its budgets and report.
use super::{StagedWrite, WriteOperation, WriteReport};
use crate::{Echo, EntityId, EntryId, Op, Precision, Tick, TickOccupant, Txn, Value};
use std::collections::HashMap;

/// Largest value set inspected or supplied to a replacement operation.
pub const MAX_SET_VALUES: usize = 256;
/// Key-work allowance shared by the replacements of one text form.
pub const MAX_SET_READ_KEYS: u64 = MAX_SET_VALUES as u64 * 256;
/// Key-work allowance of a bounded statement, per primitive it may stage:
/// what a text form gets per operation (`MAX_SET_READ_KEYS` over its 256
/// operations).
pub const READ_KEYS_PER_PRIMITIVE: u64 = MAX_SET_READ_KEYS / 256;
/// A failure's position and typed cause. Adapters choose formatting and exit codes.
#[derive(Debug)]
pub struct WriteError {
    pub operation: Option<(usize, &'static str)>,
    pub cause: Failure,
}
/// Typed write failures, retaining native errors and migration labels for adapters.
#[derive(Debug)]
pub enum Failure {
    Store(crate::Error),
    Migration {
        error: Box<crate::Error>,
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
    /// A statement used more primitives than its front end allows.
    OperationLimit(usize),
    /// All rows together exceeded the statement value-byte allowance.
    ValueBudget,
    /// A bounded statement's replacement and deletion reads examined more
    /// index keys than its allowance (carried here).
    ReadWork(u64),
    /// A bounded statement set more values for one property than a
    /// replacement takes.
    SetValueCount,
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
    /// What holds the tick, when it could be found within the read budget.
    /// `None` for a text form's plain `:assert` / `:retract`.
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
            Failure::UnknownEntity(s) => write!(f, "unknown entity `{}`", crate::Echo(s)),
            Failure::UnknownEdge => {
                f.write_str("unknown edge triple (the edge must already exist)")
            }
            Failure::ExpectedReference => f.write_str("expected a reference"),
            Failure::ExpectedFact => f.write_str("expected a fact"),
            Failure::ExpectedCorrection => f.write_str("expected a correction"),
            Failure::EmptyUnknownAttribute(name) => write!(
                f,
                "cannot set unknown attribute :{} to []; declare it first",
                crate::Echo(name)
            ),
            Failure::SetEvent(name) => write!(
                f,
                "cannot set event :{}: events are never retracted; use [:correct …]",
                crate::Echo(name)
            ),
            Failure::ValueBudget => f.write_str("statement exceeds the 16 MiB value budget"),
            Failure::ReadWork(limit) => write!(
                f,
                "statement exceeds its read work limit ({limit} index keys read by SET, REMOVE and DELETE)"
            ),
            Failure::SetValueCount => write!(
                f,
                "a property takes at most {MAX_SET_VALUES} values in one SET"
            ),
            Failure::OperationLimit(limit) => {
                write!(f, "statement exceeds the {limit}-primitive write limit")
            }
            Failure::Limit => write!(
                f,
                ":set exceeds the {}-operation/value limit, 16 MiB per-form value budget, or {}-key read budget",
                MAX_SET_VALUES, MAX_SET_READ_KEYS
            ),
            Failure::SetTooManyHeld { attr, tick } => write!(
                f,
                "cannot set :{}: it holds more than {} values at tick {} (the :set limit); use :retract/:assert",
                Echo(attr),
                MAX_SET_VALUES,
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
/// What the caller lets a write form touch. The default refuses Kala's own
/// `system` database: accounts change only through account management
/// (`kala user`), whatever grants an identity holds, so an adapter that
/// forgets its own check still cannot write them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WritePolicy {
    /// Apply forms to a system-marked world ([`crate::World::is_system`]). Only for
    /// a caller that owns the file, such as restoring an accounts dump into a
    /// new private file.
    pub allow_system: bool,
}

/// Commit a transaction holding one staged form and report it.
///
/// A commit refused because a newer transaction wrote something this one
/// read ([`crate::Error::Conflict`]) is reported at the op that made
/// the read, when the form made it ([`StagedWrite::operation_of`]):
/// `op 2 (:set): conflict: …`. Nothing was written; the caller may stage
/// and commit the same form again on the caught-up world.
pub fn commit(tx: Txn<'_>, staged: StagedWrite) -> Result<WriteReport, WriteError> {
    match tx.commit() {
        Ok(result) => Ok(staged.complete(&result)),
        Err(error) => {
            let operation = match &error {
                crate::Error::Conflict(conflict) => staged
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
fn fact_failure(tx: &Txn<'_>, subject: EntityId, error: crate::Error) -> Failure {
    match error {
        crate::Error::SameTickConflict { attr, tick, .. } => {
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

/// What holds `(subject, attr, stored)` at exactly `tick`, to explain a
/// same-tick conflict; `shown` is the value as the author wrote it.
///
/// The lookup only explains the conflict. If it runs out of the form's read
/// budget (the entry sits in a very large transaction), the conflict is
/// still the error to report, without the entry detail: a generic limit
/// error would hide the real cause.
fn occupant(
    tx: &Txn<'_>,
    state: &FormState,
    subject: EntityId,
    attr: &str,
    stored: &Value,
    shown: &Value,
    tick: Tick,
) -> Result<Option<Occupant>, Failure> {
    let found = match tx.entry_at_tick(subject, attr, stored, tick, &state.read_meter) {
        Ok(found) => found,
        Err(crate::Error::WorkInterrupted {
            reason: crate::WorkInterruptReason::Limit,
        }) => None,
        Err(e) => return Err(Failure::Store(e)),
    };
    Ok(found.map(|found| {
        let (op, by) = match found {
            TickOccupant::Committed { entry, op } => (op, OccupiedBy::Entry(entry)),
            TickOccupant::Staged { position, op } => (op, state.staged_by(position)),
        };
        Occupant {
            value: value_text(tx, shown),
            op,
            by,
        }
    }))
}

/// What staging one op did.
pub struct Effect {
    pub changed: usize,
    pub tick: Tick,
    pub precision: Option<Precision>,
    /// The entity a `:create` staged.
    pub entity: Option<EntityId>,
    pub created: Option<bool>,
}

impl Effect {
    /// A schema op: no facts, reported at the form's fallback tick.
    pub fn schema(fallback: Tick) -> Effect {
        Effect {
            changed: 0,
            tick: fallback,
            precision: None,
            entity: None,
            created: None,
        }
    }
}

/// Tracks the effects, read work and budgets of one statement, including
/// effects from earlier rows of a multi-row statement.
pub struct Stager {
    policy: WritePolicy,
    state: FormState,
    report: StagedWrite,
}

impl Stager {
    /// Start a statement before resolving any input against the transaction.
    pub fn new(
        tx: &mut Txn<'_>,
        default: Option<Tick>,
        policy: WritePolicy,
        limit: usize,
    ) -> Result<Self, WriteError> {
        if tx.is_system() && !policy.allow_system {
            tx.abort();
            return Err(WriteError {
                operation: None,
                cause: Failure::SystemWorld,
            });
        }
        let entry_offset = tx.staged_entry_count();
        // A bounded statement stages up to `limit` primitives over many
        // rows, so its reads get the per-operation allowance of a text form
        // for each of them; its front end lowers the meter further to the
        // statement's remaining step budget.
        let read_limit = READ_KEYS_PER_PRIMITIVE.saturating_mul(limit as u64);
        Ok(Self {
            policy,
            state: FormState {
                bounded_statement: true,
                remaining: limit,
                limit,
                remaining_bytes: crate::MAX_ENTRY_BYTES,
                read_limit,
                read_meter: crate::WorkMeter::bounded(read_limit, None)
                    .with_deadline(tx.deadline()),
                form_tick: default,
                fallback: tx.report_tick(default),
                entry_offset,
                op_starts: Vec::new(),
            },
            report: StagedWrite {
                ops: Vec::new(),
                entry_offset,
                spans: Vec::new(),
            },
        })
    }

    /// A text form with a bounded operation count before staging. Only its
    /// replacement operations need an aggregate byte budget: a fact literal
    /// from the source cannot expand across rows.
    pub fn for_form(
        tx: &mut Txn<'_>,
        default: Option<Tick>,
        policy: WritePolicy,
        limit: usize,
    ) -> Result<Self, WriteError> {
        let mut stager = Self::new(tx, default, policy, limit)?;
        stager.state.bounded_statement = false;
        stager.state.read_limit = MAX_SET_READ_KEYS;
        stager.state.read_meter.set_max_keys(MAX_SET_READ_KEYS);
        Ok(stager)
    }

    fn apply(
        &mut self,
        tx: &mut Txn<'_>,
        kind: &'static str,
        action: impl FnOnce(&mut Txn<'_>, &mut FormState) -> Result<Effect, Failure>,
    ) -> Result<Effect, WriteError> {
        if tx.is_system() && !self.policy.allow_system {
            tx.abort();
            return Err(WriteError {
                operation: None,
                cause: Failure::SystemWorld,
            });
        }
        let schema_before = tx.schema_changes().len();
        let first_operation = tx.operation_count();
        let position = self.report.ops.len() + 1;
        self.state.op_starts.push(tx.staged_entry_count());
        let result = if self.state.bounded_statement && self.state.remaining == 0 {
            Err(self.state.operation_limit())
        } else {
            action(tx, &mut self.state)
        }
        .and_then(|effect| {
            let cost = effect.changed.max(1);
            self.state.remaining = self
                .state
                .remaining
                .checked_sub(cost)
                .ok_or_else(|| self.state.operation_limit())?;
            Ok(effect)
        });
        let staged = result.map_err(|cause| {
            tx.abort();
            WriteError {
                operation: Some((position, kind)),
                cause,
            }
        })?;
        self.report.ops.push(WriteOperation {
            position,
            kind,
            tick: staged.tick,
            precision: staged.precision,
            changed: staged.changed,
            declared: tx.schema_changes().len() - schema_before,
            entries: Vec::new(),
            entity: staged.entity,
            created: staged.created,
        });
        self.report
            .spans
            .push(first_operation..tx.operation_count());
        Ok(staged)
    }

    /// Stage a schema operation or correction, retaining the same accounting.
    pub fn other(
        &mut self,
        tx: &mut Txn<'_>,
        kind: &'static str,
        action: impl FnOnce(&mut Txn<'_>, Tick) -> Result<Effect, Failure>,
    ) -> Result<(), WriteError> {
        self.apply(tx, kind, |tx, state| action(tx, state.fallback))
            .map(|_| ())
    }

    /// Create an entity or bind the existing handle for a conditional create.
    pub fn create(
        &mut self,
        tx: &mut Txn<'_>,
        node_type: &str,
        handle: Option<&str>,
        if_missing: bool,
    ) -> Result<(EntityId, bool), WriteError> {
        let staged = self.apply(tx, "create", |tx, state| {
            if state.bounded_statement {
                state.charge(node_type.len().saturating_add(handle.map_or(0, str::len)))?;
            }
            stage_create(tx, node_type, handle, if_missing, state)
        })?;
        Ok((
            staged.entity.expect("a successful create binds an entity"),
            staged.created.unwrap_or(true),
        ))
    }

    /// Assert or retract a fact through the transaction's inference policy.
    #[allow(clippy::too_many_arguments)]
    pub fn fact(
        &mut self,
        tx: &mut Txn<'_>,
        entity: EntityId,
        attr: &str,
        value: Value,
        op: Op,
        at: Option<Tick>,
        precision: Precision,
    ) -> Result<usize, WriteError> {
        let kind = match op {
            Op::Assert => "assert",
            Op::Retract => "retract",
        };
        self.apply(tx, kind, |tx, state| {
            if state.bounded_statement {
                state.charge(attr.len())?;
                state.charge_value(&value)?;
            }
            let tick = state.fact_tick(tx, at);
            let result = match tx.write_inferring(entity, attr, &value, Some(tick), op, precision) {
                // A statement has no retcon of its own: say which entry
                // holds the tick and how to correct it. (A text form's
                // plain `:assert` / `:retract` keeps its short message.)
                Err(crate::Error::SameTickConflict { .. }) if state.bounded_statement => {
                    let occupant = occupant(tx, state, entity, attr, &value, &value, tick)?;
                    return Err(Failure::SameTick(Box::new(SameTick {
                        subject: label(tx, entity),
                        attr: attr.to_owned(),
                        tick,
                        occupant,
                    })));
                }
                result => result.map_err(|error| fact_failure(tx, entity, error))?,
            };
            let changed = result.map_or(0, |r| r.changed);
            Ok(Effect {
                changed,
                tick,
                precision: (changed > 0).then_some(precision),
                ..Effect::schema(tick)
            })
        })
        .map(|effect| effect.changed)
    }

    /// Replace the held member set by difference at the given tick.
    pub fn set(
        &mut self,
        tx: &mut Txn<'_>,
        entity: EntityId,
        attr: &str,
        values: &[Value],
        at: Option<Tick>,
        precision: Precision,
    ) -> Result<(), WriteError> {
        self.apply(tx, "set", |tx, state| {
            stage_set(tx, entity, attr, values, at, precision, state)
        })
        .map(|_| ())
    }

    /// Primitives still available to this statement before staging another row.
    pub fn remaining_operations(&self) -> usize {
        self.state.remaining
    }

    /// Key-work budget shared by replacement and node-deletion scans.
    pub fn read_meter(&self) -> &crate::WorkMeter<'static> {
        &self.state.read_meter
    }

    /// The most keys [`Stager::read_meter`] may count for this statement.
    pub fn read_limit(&self) -> u64 {
        self.state.read_limit
    }

    /// Why a read under [`Stager::read_meter`] and the remaining value
    /// bytes was interrupted by a limit: the key work, else the bytes.
    pub fn read_limit_failure(&self) -> Failure {
        self.state.read_failure()
    }

    /// The byte allowance available before resolving a front end's string literals.
    pub fn remaining_bytes(&self) -> usize {
        self.state.remaining_bytes
    }

    /// Conservative admission cost of retaining this expanded statement until
    /// its transaction ends. This accounts for 128 bytes of form metadata,
    /// 512 bytes per native operation (including no-ops), and four copies
    /// of its charged name/value bytes for replay, fact data and indexes.
    /// In-memory transactions need no replay log; changed facts still carry
    /// the same cost there. SET includes both requested and held values.
    pub fn retained_bytes(&self) -> usize {
        let payload = crate::MAX_ENTRY_BYTES - self.state.remaining_bytes;
        self.report.ops.iter().zip(&self.report.spans).fold(
            128usize.saturating_add(payload.saturating_mul(4)),
            |total, (op, span)| {
                let primitives = span.len().max(op.changed).max(1);
                total.saturating_add(primitives.saturating_mul(512))
            },
        )
    }

    /// Finish this statement without committing the caller's transaction.
    pub fn finish(self) -> StagedWrite {
        self.report
    }
}
struct FormState {
    bounded_statement: bool,
    remaining: usize,
    limit: usize,
    remaining_bytes: usize,
    /// The statement's key-work allowance (the meter's bound may be lower).
    read_limit: u64,
    read_meter: crate::WorkMeter<'static>,
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
}
impl FormState {
    fn charge(&mut self, bytes: usize) -> Result<(), Failure> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes)
            .ok_or_else(|| self.byte_limit())?;
        Ok(())
    }

    /// A limit interrupted a read: name the one that did.
    fn read_failure(&self) -> Failure {
        if !self.bounded_statement {
            Failure::Limit
        } else if self.read_meter.check().is_err() {
            Failure::ReadWork(self.read_limit)
        } else {
            Failure::ValueBudget
        }
    }

    fn byte_limit(&self) -> Failure {
        if self.bounded_statement {
            Failure::ValueBudget
        } else {
            Failure::Limit
        }
    }

    fn charge_value(&mut self, value: &Value) -> Result<(), Failure> {
        let size = match value {
            Value::Str(s) => {
                if s.len() > self.remaining_bytes {
                    return Err(self.byte_limit());
                }
                s.len()
                    .saturating_add(s.bytes().filter(|b| *b == 0).count())
                    .saturating_add(3)
            }
            Value::Bool(_) => 2,
            _ => 9,
        };
        self.charge(size)
    }

    fn operation_limit(&self) -> Failure {
        if !self.bounded_statement {
            Failure::Limit
        } else {
            Failure::OperationLimit(self.limit)
        }
    }
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

/// `[:create TYPE handle]`: later ops of the form may refer to the entity.
fn stage_create(
    tx: &mut Txn<'_>,
    node_type: &str,
    handle: Option<&str>,
    if_missing: bool,
    state: &mut FormState,
) -> Result<Effect, Failure> {
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
    Ok(Effect {
        entity: Some(id),
        created: if_missing.then_some(created),
        ..Effect::schema(state.fallback)
    })
}

/// Replace by difference, retaining existing members and their precision.
fn stage_set(
    tx: &mut Txn<'_>,
    entity: EntityId,
    attr: &str,
    values: &[Value],
    at: Option<Tick>,
    precision: Precision,
    state: &mut FormState,
) -> Result<Effect, Failure> {
    if values.len() > MAX_SET_VALUES {
        return Err(if state.bounded_statement {
            Failure::SetValueCount
        } else {
            Failure::Limit
        });
    }
    let tick = state.fact_tick(tx, at);
    let mut wanted = std::collections::BTreeMap::new();
    for v in values {
        state.charge_value(v)?;
        let key = v
            .encode()
            .map_err(|e| Failure::Store(crate::Error::Value(e)))?;
        wanted.insert(key, v.clone());
    }
    // A Cypher property can use the inverse spelling of a relationship. Its
    // held values come from R and its primitives still use the stored name.
    let inverse = if state.bounded_statement {
        let base = tx.relationship(entity, attr, entity).1;
        (base != attr).then(|| base.to_owned())
    } else {
        None
    };
    let stored_attr = inverse.as_deref().unwrap_or(attr);
    if state.bounded_statement {
        // A read or inference may survive even when the difference is empty.
        state.charge(attr.len())?;
        if inverse.is_some() {
            state.charge(stored_attr.len())?;
        }
    }
    if let Some(v) = wanted.values().next() {
        tx.infer_write_attribute(stored_attr, v, Op::Assert)
            .map_err(Failure::Store)?;
    }
    let def = tx
        .attribute(stored_attr)
        .ok_or_else(|| Failure::EmptyUnknownAttribute(attr.to_owned()))?;
    if def.deprecated {
        return Err(Failure::Store(crate::Error::DeprecatedAttribute(
            attr.to_owned(),
        )));
    }
    if def.kind == crate::Kind::Event {
        return Err(Failure::SetEvent(attr.to_owned()));
    }
    for v in wanted.values() {
        if v.value_type() != def.value_type {
            return Err(Failure::Store(crate::Error::WrongValueType {
                attr: attr.to_owned(),
                expected: def.value_type,
                got: v.value_type(),
            }));
        }
    }
    let held = tx
        .values_at(
            entity,
            attr,
            tick,
            MAX_SET_VALUES,
            state.remaining_bytes,
            &state.read_meter,
        )
        .map_err(|e| {
            if matches!(
                e,
                crate::Error::WorkInterrupted {
                    reason: crate::WorkInterruptReason::Limit
                }
            ) {
                state.read_failure()
            } else {
                Failure::Store(e)
            }
        })?;
    if held.more {
        return Err(Failure::SetTooManyHeld {
            attr: attr.to_owned(),
            tick,
        });
    }
    let mut removed = Vec::new();
    for value in held.values {
        let key = value
            .encode()
            .map_err(|e| Failure::Store(crate::Error::Value(e)))?;
        // `values_at` already kept the held values within the remaining
        // budget; a checked subtraction keeps that an error, never a wrap,
        // if the two ever disagree.
        state.remaining_bytes = state
            .remaining_bytes
            .checked_sub(key.len())
            .ok_or_else(|| state.byte_limit())?;
        if wanted.remove(&key).is_none() {
            removed.push(value);
        }
    }
    let changes = removed.len().saturating_add(wanted.len());
    if changes > state.remaining {
        return Err(state.operation_limit());
    }
    if state.bounded_statement {
        // Each individual native write owns the attribute name in its replay log.
        state.charge(stored_attr.len().saturating_mul(changes))?;
    }
    let mut changed = 0;
    for (op, value) in removed
        .into_iter()
        .map(|v| (Op::Retract, v))
        .chain(wanted.into_values().map(|v| (Op::Assert, v)))
    {
        let (subject, stored_value) = match (&inverse, &value) {
            (Some(_), Value::Ref(source)) => (*source, Value::Ref(entity)),
            _ => (entity, value.clone()),
        };
        let result = match op {
            Op::Assert => tx.assert_with_precision(
                subject,
                stored_attr,
                stored_value.clone(),
                Some(tick),
                precision,
            ),
            Op::Retract => tx.retract_with_precision(
                subject,
                stored_attr,
                stored_value.clone(),
                Some(tick),
                precision,
            ),
        };
        match result {
            Ok(r) => changed += r.changed,
            Err(crate::Error::SameTickConflict { .. }) => {
                let occupant =
                    occupant(tx, state, subject, stored_attr, &stored_value, &value, tick)?;
                return Err(Failure::SameTick(Box::new(SameTick {
                    subject: label(tx, entity),
                    attr: attr.to_owned(),
                    tick,
                    occupant,
                })));
            }
            Err(error) => return Err(Failure::Store(error)),
        }
    }
    Ok(Effect {
        changed,
        tick,
        precision: (changed > 0).then_some(precision),
        entity: None,
        created: None,
    })
}
