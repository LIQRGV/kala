//! Shared staging, safe error mapping, and committed write summaries.
use crate::{
    error::{FORBIDDEN, Failure, INVALID, STORE, SYNTAX, TYPE},
    message::Map,
    packstream::{self, Value},
    query::Output,
};
use kala_core::{CommitResult, EntityId, Error, SchemaChange, World};
use kala_lang::write::{self, StagedWrite, WriteOperation, WriteTxn};
use std::collections::VecDeque;

pub(crate) const CONFLICT: &str = "Neo.TransientError.Transaction.DeadlockDetected";
pub(crate) const LOCK: &str = "Neo.TransientError.Transaction.LockAcquisitionTimeout";
pub(crate) const TIMEOUT: &str = "Neo.ClientError.Transaction.TransactionTimedOut";
pub(crate) const ACCESS: &str = "Neo.ClientError.Transaction.ForbiddenDueToTransactionType";
pub(crate) const BOOKMARK: &str = "Neo.ClientError.Transaction.InvalidBookmark";
const SEMANTIC: &str = "Neo.ClientError.Statement.SemanticError";
const CONSTRAINT: &str = "Neo.ClientError.Schema.ConstraintValidationFailed";
pub(crate) const MAX_FORMS: usize = 256;
pub(crate) const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;

/// Charge retained statement data across the entire explicit transaction.
pub(crate) fn charge(total: &mut usize, bytes: usize) -> Result<(), Failure> {
    *total = total
        .checked_add(bytes)
        .filter(|n| *n <= MAX_STAGED_BYTES)
        .ok_or_else(|| Failure::new(INVALID, "transaction exceeds the 16 MiB staged byte limit"))?;
    Ok(())
}
const REDUCED: &str = "write failed; details require read privilege";
/// A write refused by a store rule: something about the data it met (a
/// handle that exists or doesn't, a value that is held, a declared type, a
/// same-tick conflict). With `read`, the code says which class and the
/// message says what. Without it every such failure is the same code and
/// the same sentence, so a write-only identity can't tell them apart and
/// use writes to probe what the world holds. Syntax errors and privilege
/// refusals describe the request, not the data, and are not reduced.
fn store_rule(code: &'static str, message: String, read: bool) -> Failure {
    if read {
        Failure::new(code, message)
    } else {
        Failure::new(crate::error::EXECUTION, REDUCED)
    }
}
pub(crate) fn timeout() -> Failure {
    Failure::new(TIMEOUT, "transaction timed out; nothing was written")
}
/// Whether a handle that reported `error` must be reopened before it can
/// write again. Everything else (a store rule, a conflict, a held lock, a
/// deadline, a transaction that failed or was dropped) leaves the `World`
/// valid: see `OwnedTxn::commit` and `Txn` in kala-core. The registry's
/// cached writer is dropped only for these, because reopening replays the
/// whole log while the database's writer mutex is held.
pub(crate) fn leaves_handle_unusable(error: &Error) -> bool {
    matches!(
        error,
        Error::LogDiverged
            | Error::Corrupt(_)
            | Error::Io { .. }
            | Error::BadMagic
            | Error::UnsupportedVersion(_)
            | Error::RecoveryRequired { .. }
            | Error::RecoveryUnacknowledged
    )
}
pub(crate) fn store_error(error: Error, read: bool) -> Failure {
    let (code, message) = match &error {
        Error::WorkInterrupted { reason: kala_core::WorkInterruptReason::Stopped } => return timeout(),
        Error::Conflict(_) => (CONFLICT, error.to_string()),
        Error::WorldLocked => (LOCK, "another writer held the world's lock for the whole busy timeout; nothing was written".into()),
        Error::LogDiverged => (CONFLICT, "the world changed; its handle will be reopened; run the transaction again".into()),
        Error::RecoveryRequired { .. } | Error::RecoveryUnacknowledged => (STORE, "an operator must run a kala write command with --acknowledge-recovery; nothing was written".into()),
        Error::Io { .. } | Error::Corrupt(_) | Error::BadMagic | Error::UnsupportedVersion(_) => (STORE, "the world could not be written".into()),
        // Policy, not a store rule (see `admit`): it describes the request.
        Error::SchemaChangeNotPermitted(name) => (
            FORBIDDEN,
            format!(
                "changing {}, which already exists, requires admin privilege on the database (declaring new names needs only write); nothing was written",
                crate::error::escape(name)
            ),
        ),
        Error::WrongValueType { .. } | Error::Value(_) => (TYPE, error.to_string()),
        Error::SameTickConflict { .. } | Error::TxnKeyConflict { .. } | Error::DuplicateHandle(_) | Error::HandleTypeMismatch { .. } => (CONSTRAINT, error.to_string()),
        _ => (SEMANTIC, error.to_string()),
    };
    let mut failure = if matches!(code, TYPE | CONSTRAINT | SEMANTIC) {
        store_rule(code, message, read)
    } else {
        Failure::new(
            code,
            if read || matches!(code, LOCK | STORE | FORBIDDEN) {
                message
            } else {
                REDUCED.into()
            },
        )
    };
    failure.handle_lost = leaves_handle_unusable(&error);
    failure
}
pub(crate) fn write_error(error: write::WriteError, read: bool) -> Failure {
    match error.cause {
        write::Failure::Store(e) => {
            let mut failure = store_error(e, read);
            if read && let Some((index, verb)) = error.operation {
                failure.message = format!("op {index} (:{verb}): {}", failure.message);
            }
            failure
        }
        write::Failure::SystemWorld => Failure::new(
            FORBIDDEN,
            "the system database is written only with kala user",
        ),
        _ => {
            let code = if matches!(error.cause, write::Failure::SameTick(_)) {
                CONSTRAINT
            } else {
                SEMANTIC
            };
            store_rule(code, error.to_string(), read)
        }
    }
}
pub(crate) fn statement_error(error: write::StatementError, read: bool) -> Failure {
    match error {
        write::StatementError::Stage(e)
            if matches!(
                e.cause,
                write::Failure::Store(_)
                    | write::Failure::SystemWorld
                    | write::Failure::SameTick(_)
            ) =>
        {
            write_error(e, read)
        }
        // The statement's own wording (a Cypher fix, never a Datalog verb).
        e @ write::StatementError::Stage(_) => store_rule(SEMANTIC, e.to_string(), read),
        write::StatementError::Store(e) => store_error(e, read),
        write::StatementError::Query(e) if read => crate::query::query_error(e),
        e => store_rule(SEMANTIC, e.to_string(), read),
    }
}
/// What the connection's identity holds on the database a write targets.
#[derive(Clone, Copy)]
pub(crate) struct Privileges {
    /// `read`: results and errors may describe existing data.
    pub read: bool,
    /// `admin` on this database.
    pub admin: bool,
}
/// What a form needs before anything is staged (or a writable handle
/// opened) for it: the privileges it requires and its place in the
/// transaction (`prior` forms already staged). `write` itself was checked
/// before the text was parsed (`Session::write_allowed`).
///
/// **The privilege table for writes over Bolt** (all of it is here):
///
/// | Form | Needs |
/// |---|---|
/// | `[:assert …]`, `[:retract …]`, `[:create …]`, `[:attribute …]`, `[:node-type …]`, names declared by inference | `write` |
/// | `[:set …]`, `[:create … :if-missing]`, `[:correct …]` (they read first) | `write` + `read` |
/// | `[:world-setting …]`, `[:rename …]`, `[:deprecate …]`, `[:cardinality …]`, `[:migrate …]` | `admin` on that database |
/// | `[:attribute …]` or `[:node-type …]` that changes one the committed schema already has | `admin` on that database |
///
/// The last row can't be decided from the form (it depends on the schema),
/// so it is the one refusal that comes after staging: the transaction is
/// limited to schema additions (`Txn::set_schema_additions_only`) and
/// kala-core refuses the edit, at staging or at a commit over a newer
/// transaction that declared the name ([`store_error`] maps it).
///
/// The admin rows are policy, not a store rule: those forms change what
/// existing data or the world's rules mean for every other writer, so a
/// remote identity needs `admin` for them. The owner may relax it; the CLI
/// is not affected (its boundary is the file's permissions).
pub(crate) fn admit(form: &WriteTxn, prior: usize, held: Privileges) -> Result<(), Failure> {
    admit_needs(
        write::requires_read(form),
        write::requires_admin(form),
        form.concurrency.is_some(),
        prior,
        held,
    )
}

pub(crate) fn admit_needs(
    read: bool,
    admin: bool,
    mode: bool,
    prior: usize,
    held: Privileges,
) -> Result<(), Failure> {
    if admin && !held.admin {
        return Err(Failure::new(
            FORBIDDEN,
            "this write form requires admin privilege on the database",
        ));
    }
    if read && !held.read {
        return Err(Failure::new(
            FORBIDDEN,
            "this write form requires read privilege",
        ));
    }
    if prior > 0 && mode {
        return Err(Failure::new(
            INVALID,
            "concurrency is allowed only in the first write form of a transaction",
        ));
    }
    if prior >= MAX_FORMS {
        return Err(Failure::new(
            INVALID,
            "transaction exceeds the 256-form limit",
        ));
    }
    Ok(())
}
/// Stages a form [`admit`] accepted.
pub(crate) fn stage(
    tx: &mut kala_core::Txn<'_>,
    form: &WriteTxn,
    read: bool,
) -> Result<StagedWrite, Failure> {
    let mut staged = write::stage(tx, form, None).map_err(|e| write_error(e, read))?;
    for op in &mut staged.ops {
        if op.kind == "create" && op.created.is_none() {
            op.created = Some(true);
        }
    }
    Ok(staged)
}
/// Whether an op of this kind writes data (an op that only edits the schema
/// does not): what tells a `w` summary from an `s` one.
pub(crate) fn writes_data(kind: &str) -> bool {
    matches!(
        kind,
        "assert" | "retract" | "set" | "create" | "correct" | "migrate"
    )
}
/// The failure of form `form` (1-based) of a multi-form auto-commit RUN,
/// where each form is its own transaction. `prior` forms before it were
/// committed and `committed` are committed now (the failing form included,
/// if it failed after its own commit); `last_tx` is the newest of them.
///
/// Once anything is committed the message must not say otherwise and the
/// failure must not invite a retry: a driver that retried the RUN would
/// apply the committed forms a second time. So the sentence "nothing was
/// written" is narrowed to the failing form, "run it again" is dropped, a
/// transient code becomes a plain client error, and the last commit's tx
/// and bookmark are named so the client can read its own writes.
pub(crate) fn form_failed(
    mut failure: Failure,
    form: usize,
    prior: usize,
    committed: usize,
    db: &str,
    last_tx: u64,
) -> Failure {
    if prior > 0 {
        let mut detail = failure
            .message
            .replace("nothing was written", "this form wrote nothing")
            .replace("; run it again", "")
            .replace("; run the transaction again", "");
        if committed == prior && !detail.contains("this form wrote nothing") {
            detail.push_str("; this form wrote nothing");
        }
        failure.message = format!(
            "form {form} failed; {prior} prior forms committed, the last as tx {last_tx} (bookmark kala:{db}:{last_tx}): {detail}"
        );
    } else if form > 1 {
        failure.message = format!(
            "form {form} failed; 0 prior forms committed: {}",
            failure.message
        );
    }
    if committed > 0 && failure.code.starts_with("Neo.TransientError") {
        failure.code = crate::error::EXECUTION;
    }
    failure
}
pub(crate) fn parse_error(error: impl std::fmt::Display) -> Failure {
    Failure::new(SYNTAX, error.to_string())
}
pub(crate) fn output() -> Output {
    Output {
        fields: [
            "form", "op", "kind", "tick", "changed", "entries", "entity", "created",
        ]
        .map(str::to_owned)
        .to_vec(),
        rows: VecDeque::new(),
        encoded_bytes: 0,
        exposes: VecDeque::new(),
        reads: crate::reads::Reads::default(),
    }
}
fn int(n: usize) -> Value {
    Value::Integer(i64::try_from(n).unwrap_or(i64::MAX))
}
fn ids(entries: &[kala_core::EntryId]) -> Value {
    Value::List(entries.iter().map(|e| Value::Integer(e.0 as i64)).collect())
}

/// Identity-only nodes deliberately avoid exposing held properties to writers.
/// A write result identifies the created/found entity; ordinary queries enrich it.
pub(crate) fn entity(id: EntityId, label: String) -> Value {
    let node_type = label
        .rsplit_once('#')
        .map_or(label.as_str(), |(s, _)| s)
        .to_owned();
    Value::Structure(
        0x4e,
        vec![
            Value::Integer(id.raw() as i64),
            Value::List(vec![Value::String(node_type)]),
            Value::Map(Map::new()),
            Value::String(label),
        ],
    )
}
pub(crate) fn rows(
    out: &mut Output,
    ops: &[WriteOperation],
    form: usize,
    read: bool,
    label: impl Fn(EntityId) -> String,
    max_message: usize,
    max_depth: usize,
) -> Result<bool, Failure> {
    let mut exposed = false;
    for op in ops {
        let id = op.entity.filter(|_| read || op.created == Some(true));
        exposed |= id.is_some();
        let record = Value::Structure(
            0x71,
            vec![Value::List(vec![
                int(form),
                int(op.position),
                Value::String(op.kind.into()),
                Value::Integer(op.tick.0),
                int(op.changed),
                ids(&op.entries),
                id.map_or(Value::Null, |id| entity(id, label(id))),
                op.created
                    .filter(|created| read || *created)
                    .map_or(Value::Null, Value::Bool),
            ])],
        );
        let bytes = packstream::encode_with_limits(&record, max_message, max_depth)
            .map_err(|_| Failure::new(INVALID, "write result exceeds the message limit"))?;
        let total = out.encoded_bytes.saturating_add(bytes.len());
        if total > MAX_STAGED_BYTES {
            return Err(Failure::new(
                INVALID,
                "write result exceeds the 16 MiB result limit",
            ));
        }
        out.encoded_bytes = total;
        out.exposes.push_back(id.is_some());
        out.rows.push_back(bytes);
    }
    Ok(exposed)
}
#[derive(Default)]
pub(crate) struct Summary {
    pub changed: usize,
    declared: usize,
    entries: Vec<kala_core::EntryId>,
    mapping: Vec<Value>,
    notifications: Vec<Value>,
    nodes: usize,
    edges: usize,
    relationships_deleted: usize,
    labels_added: usize,
    labels_removed: usize,
    ops: Vec<Value>,
    ops_omitted: usize,
    cypher: bool,
    properties: usize,
    pub tx: u64,
    updates: bool,
    data: bool,
    moved: Option<Value>,
    remaining: usize,
    entries_omitted: usize,
    ids_omitted: usize,
    notifications_omitted: usize,
}
fn notification(code: &str, title: &str, description: String, warning: bool) -> Value {
    Value::Map(
        [
            ("code".into(), Value::String(code.into())),
            ("title".into(), Value::String(title.into())),
            (
                "description".into(),
                Value::String(crate::error::escape(&description)),
            ),
            (
                "severity".into(),
                Value::String(if warning { "WARNING" } else { "INFORMATION" }.into()),
            ),
        ]
        .into(),
    )
}
impl Summary {
    pub fn new(max: usize) -> Self {
        Self {
            remaining: max.saturating_sub(1024) / 2,
            ..Self::default()
        }
    }
    fn notify(&mut self, value: Value) {
        let cost = packstream::encode_with_limits(&value, self.remaining, 32)
            .ok()
            .map(|bytes| bytes.len().saturating_add(128));
        if let Some(cost) = cost.filter(|cost| *cost <= self.remaining) {
            self.remaining -= cost;
            self.notifications.push(value);
        } else {
            self.notifications_omitted += 1;
        }
    }
    fn mapping(&mut self, from: EntityId, to: EntityId) {
        if self.remaining >= 128 {
            self.remaining -= 128;
            self.mapping.push(Value::List(vec![
                Value::Integer(from.raw() as i64),
                Value::Integer(to.raw() as i64),
            ]));
        } else {
            self.ids_omitted += 1;
        }
    }
    /// Save bounded per-operation details where a Cypher result's rows
    /// have been reserved for RETURN. Counts, positions and entry IDs carry
    /// the same information as the reduced Datalog operation records.
    pub fn operations(&mut self, form: usize, ops: &[WriteOperation]) {
        self.cypher = true;
        for op in ops {
            let cost = 192usize.saturating_add(op.entries.len().saturating_mul(9));
            if cost > self.remaining {
                self.ops_omitted += 1;
                continue;
            }
            self.remaining -= cost;
            self.ops.push(Value::Map(
                [
                    ("form".into(), int(form)),
                    ("op".into(), int(op.position)),
                    ("kind".into(), Value::String(op.kind.into())),
                    ("tick".into(), Value::Integer(op.tick.0)),
                    ("changed".into(), int(op.changed)),
                    ("entries".into(), ids(&op.entries)),
                ]
                .into(),
            ));
        }
    }
    pub fn add(&mut self, world: &World, result: &CommitResult, forms: &[StagedWrite], read: bool) {
        self.tx = result.tx.0;
        self.changed += result.changed;
        self.declared += result.declared;
        for entry in &result.entries {
            if self.remaining >= 16 {
                self.remaining -= 16;
                self.entries.push(*entry);
            } else {
                self.entries_omitted += 1;
            }
        }
        self.updates |= result.committed;
        self.edges += result.edges.len();
        for form in forms {
            for op in &form.ops {
                self.nodes += usize::from(op.kind == "create" && op.created != Some(false));
                self.data |= writes_data(op.kind);
            }
        }
        for id in &result.entries {
            if let Some(info) = world.entry(*id) {
                use kala_core::log::EntryBody;
                if let EntryBody::Fact {
                    op, attr, value, ..
                }
                | EntryBody::Revised {
                    op, attr, value, ..
                } = info.body
                {
                    let asserted = op == kala_core::Op::Assert;
                    if matches!(value, kala_core::Value::Ref(_)) {
                        self.relationships_deleted += usize::from(!asserted);
                    } else {
                        self.properties += usize::from(asserted);
                        if world.attr_name(attr) == Some("label") {
                            self.labels_added += usize::from(asserted);
                            self.labels_removed += usize::from(!asserted);
                        }
                    }
                }
            }
        }
        for (a, b) in &result.ids.entities {
            self.mapping(*a, *b);
        }
        for (a, b) in &result.ids.edges {
            self.mapping((*a).into(), (*b).into());
        }
        for (overlaps, resolved) in [
            (&result.warnings.introduced, false),
            (&result.warnings.resolved, true),
        ] {
            for overlap in overlaps {
                let description = if read {
                    format!(
                        "overlap {}: {} :{} {:?}, {} and {}, from {} until {:?}",
                        if resolved { "resolved" } else { "introduced" },
                        world.format_id(overlap.entity),
                        crate::error::escape(&overlap.attr),
                        overlap.side,
                        short_value(&overlap.a),
                        short_value(&overlap.b),
                        overlap.during.from.0,
                        overlap.during.until.map(|tick| tick.0)
                    )
                } else {
                    "overlap details require read privilege".into()
                };
                self.notify(notification(
                    if resolved {
                        "Kala.Information.OverlapResolved"
                    } else {
                        "Kala.Warning.Overlap"
                    },
                    "Cardinality overlap",
                    description,
                    !resolved,
                ));
            }
        }
        let warnings = &result.warnings;
        if warnings.introduced_omitted > 0 || warnings.resolved_omitted > 0 {
            self.notify(notification(
                "Kala.Information.OverlapsOmitted",
                "Overlap notifications capped",
                format!(
                    "{} introduced and {} resolved overlaps omitted",
                    warnings.introduced_omitted, warnings.resolved_omitted
                ),
                false,
            ));
        }
        if let Some(moved) = result.latest_tick_moved {
            self.moved = Some(Value::Map(
                [
                    ("from".into(), Value::Integer(moved.from.0)),
                    ("to".into(), Value::Integer(moved.to.0)),
                ]
                .into(),
            ));
            self.notify(notification(
                "Kala.Warning.LatestTickMoved",
                "Latest tick moved",
                moved.to_string(),
                true,
            ));
        }
        for change in &result.schema {
            let description = match change {
                SchemaChange::Attribute {
                    name,
                    value_type,
                    inferred: true,
                } => Some(format!(
                    "declared :{name} as {} (inferred)",
                    value_type.name()
                )),
                SchemaChange::NodeType {
                    name,
                    inferred: true,
                } => Some(format!("declared node type {name} (inferred)")),
                _ => None,
            };
            if let Some(description) = description {
                self.notify(notification(
                    "Kala.Information.InferredDeclaration",
                    "Inferred declaration",
                    description,
                    false,
                ));
            }
        }
    }
    pub fn metadata(self, db: &str, queried: bool, max: usize, depth: usize) -> Map {
        let mut kala: Map = [
            ("tx".into(), Value::Integer(self.tx as i64)),
            ("changed".into(), int(self.changed)),
            ("declared".into(), int(self.declared)),
            ("entries".into(), ids(&self.entries)),
        ]
        .into();
        if self.cypher {
            kala.insert("ops".into(), Value::List(self.ops));
        }
        for (name, count) in [
            ("ops_omitted", self.ops_omitted),
            ("entries_omitted", self.entries_omitted),
            ("ids_omitted", self.ids_omitted),
            ("notifications_omitted", self.notifications_omitted),
        ] {
            if count > 0 {
                kala.insert(name.into(), int(count));
            }
        }
        if !self.mapping.is_empty() {
            kala.insert("ids".into(), Value::List(self.mapping));
        }
        if let Some(moved) = self.moved {
            kala.insert("latest_tick_moved".into(), moved);
        }
        let stats = [
            ("nodes-created".into(), int(self.nodes)),
            ("relationships-created".into(), int(self.edges)),
            (
                "relationships-deleted".into(),
                int(self.relationships_deleted),
            ),
            ("labels-added".into(), int(self.labels_added)),
            ("labels-removed".into(), int(self.labels_removed)),
            ("properties-set".into(), int(self.properties)),
            ("contains-updates".into(), Value::Bool(self.updates)),
        ]
        .into();
        let mut map: Map = [
            (
                "type".into(),
                Value::String(
                    if queried {
                        "rw"
                    } else if self.data || self.cypher {
                        "w"
                    } else {
                        "s"
                    }
                    .into(),
                ),
            ),
            ("db".into(), Value::String(db.into())),
            (
                "bookmark".into(),
                Value::String(format!("kala:{db}:{}", self.tx)),
            ),
            ("stats".into(), Value::Map(stats)),
            ("kala".into(), Value::Map(kala)),
        ]
        .into();
        if !self.notifications.is_empty() {
            map.insert("notifications".into(), Value::List(self.notifications));
        }
        // A durable commit always has a bounded SUCCESS. Details are optional;
        // the bookmark, tx, totals and counters must never be lost to encoding.
        for field in ["notifications", "ops", "ids", "entries"] {
            while packstream::encode_with_limits(
                &Value::Structure(0x70, vec![Value::Map(map.clone())]),
                max.saturating_sub(32),
                depth,
            )
            .is_err()
            {
                let target = if field == "notifications" {
                    &mut map
                } else {
                    let Some(Value::Map(kala)) = map.get_mut("kala") else {
                        break;
                    };
                    kala
                };
                let Some(Value::List(values)) = target.get_mut(field) else {
                    break;
                };
                if values.is_empty() {
                    break;
                }
                let omitted = values.len().div_ceil(2);
                values.truncate(values.len() - omitted);
                let Some(Value::Map(kala)) = map.get_mut("kala") else {
                    break;
                };
                let count = kala
                    .entry(format!("{field}_omitted"))
                    .or_insert(Value::Integer(0));
                if let Value::Integer(count) = count {
                    *count += omitted as i64;
                }
            }
        }
        map
    }
}

/// The CLI conflict sentence, using only the caller's read keys, never held values.
pub(crate) fn commit_error(w: &World, error: Error, forms: &[StagedWrite], read: bool) -> Failure {
    let Error::Conflict(conflict) = &error else {
        return store_error(error, read);
    };
    if !read {
        return store_error(error, false);
    }
    use kala_core::ConflictKey;
    let op = forms
        .iter()
        .find_map(|form| form.operation_of(conflict.operation))
        .map(|op| (op.position, op.kind));
    let entity = |id: EntityId| match w.handle_of(id) {
        Some(handle) => format!(":{}", crate::error::escape(handle)),
        None => w.format_id(id),
    };
    let attr = |name: &str| format!(":{}", crate::error::escape(name));
    let who = match op {
        Some((index, verb)) => format!("op {index} (:{verb})"),
        None => "this transaction".to_string(),
    };
    let what = match &conflict.key {
        ConflictKey::Entity { entity: id } => {
            format!("read {}, which another transaction changed", entity(*id))
        }
        ConflictKey::EntityAttribute { entity: e, attr: a } => format!(
            "read {} {}, which another transaction changed",
            entity(*e),
            attr(a)
        ),
        ConflictKey::Reverse { target, attr: a } => format!(
            "read what points at {} through {}, which another transaction changed",
            entity(*target),
            attr(a)
        ),
        ConflictKey::Value { attr: a } => format!(
            "looked up a value of {}, which another transaction wrote",
            attr(a)
        ),
        ConflictKey::Attribute { attr: a } => {
            format!("read {}, which another transaction changed", attr(a))
        }
        ConflictKey::TypeScan { node_type } => format!(
            "read every {} entity, and another transaction created one",
            crate::error::escape(node_type)
        ),
        ConflictKey::AnyDefine => {
            "read the world's entities, and another transaction created one".to_string()
        }
        ConflictKey::Handle { handle } => format!(
            "found the handle :{} free, and another transaction created it",
            crate::error::escape(handle)
        ),
        ConflictKey::Entry { entry } => format!(
            "read entry#{}, which another transaction corrected",
            entry.0
        ),
        ConflictKey::Undeclared { name } => format!(
            "found {} undeclared, and another transaction declared it",
            attr(name)
        ),
        ConflictKey::All => "read the whole world, which another transaction changed".to_string(),
        ConflictKey::ExposedId { id } => format!(
            "handed out {}, which a commit over another transaction would renumber",
            w.format_id(*id)
        ),
    };
    Failure::new(
        CONFLICT,
        format!("conflict: {who} {what}; nothing was written; run it again"),
    )
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;

fn short_value(value: &kala_core::Value) -> String {
    match value {
        kala_core::Value::Str(text) => format!("\"{}\"", crate::error::escape(text)),
        other => format!("{other:?}"),
    }
}
