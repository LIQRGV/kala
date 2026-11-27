//! Materialize the reading pipeline on its snapshot, then drive the shared
//! primitive stager once for each ordered binding. RETURN projections keep
//! their origins so auto-commit can resolve them against the final world.
use crate::{CypherError, ast::*};
use kala_core::{
    Cardinality, Concurrency, EntityId, IdMap, Op, Precision, Tick, Txn, Value, WorkMeter, World,
    write::{
        StagedWrite, WriteReport,
        stage::{self as native, Stager},
    },
};
use kala_query::{
    Datum, Expr, ParameterInput, ParameterValue, Prepared, QueryError, ResultSet, RunOptions, Term,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// Maximum number of input rows that can drive an update.
pub const MAX_WRITE_ROWS: usize = 10_000;
/// Maximum number of primitives (including no-ops) in one statement.
pub const MAX_WRITE_PRIMITIVES: usize = 10_000;

/// Preserve store and query failures for typed protocol error mapping.
#[derive(Debug)]
pub enum WriteError {
    Syntax(CypherError),
    Query(QueryError),
    Store(kala_core::Error),
    Stage(native::WriteError),
    Invalid(String),
}
impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(e) => e.fmt(f),
            Self::Query(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::Stage(e) if matches!(e.cause, native::Failure::SetEvent(_)) => {
                write!(f, "{e} in Datalog (// kala:datalog)")
            }
            Self::Stage(native::WriteError {
                cause: native::Failure::SetTooManyHeld { attr, tick },
                ..
            }) => write!(
                f,
                "cannot SET {}: it holds more than {} values at tick {} (the limit of one SET); remove members with DELETE or a Datalog [:retract …]",
                kala_core::Echo(attr),
                native::MAX_SET_VALUES,
                tick.0
            ),
            Self::Stage(e) => e.fmt(f),
            Self::Invalid(s) => f.write_str(s),
        }
    }
}
impl std::error::Error for WriteError {}
impl From<CypherError> for WriteError {
    fn from(e: CypherError) -> Self {
        Self::Syntax(e)
    }
}
impl From<QueryError> for WriteError {
    fn from(e: QueryError) -> Self {
        Self::Query(e)
    }
}
impl From<kala_core::Error> for WriteError {
    fn from(e: kala_core::Error) -> Self {
        Self::Store(e)
    }
}
impl From<native::WriteError> for WriteError {
    fn from(e: native::WriteError) -> Self {
        Self::Stage(e)
    }
}

fn invalid(s: &str) -> WriteError {
    WriteError::Invalid(s.into())
}
type Result<T> = std::result::Result<T, WriteError>;
type Row = BTreeMap<String, Datum>;

/// A snapshot's complete input. Its plan supplies dependency keys even when
/// its result is empty.
pub struct WriteInput<'a> {
    pub prepared: Prepared,
    reads: kala_query::reads::Reads,
    defined_names: Vec<bool>,
    depends_on_snapshot: bool,
    result: ResultSet,
    property_columns: BTreeMap<(String, String), usize>,
    tick: Option<Tick>,
    options: RunOptions<'a>,
    bytes: usize,
    steps: u64,
}

impl WriteInput<'_> {
    /// Snapshot dependencies for a server that opens a writable handle lazily.
    /// Staging also records them, so all callers get optimistic validation.
    pub fn reads(&self) -> &kala_query::reads::Reads {
        &self.reads
    }

    /// The limits staging runs under, when they differ from those the
    /// reading part ran under (an adapter that gave the reading part a
    /// smaller step budget than the statement as a whole). What the reading
    /// part used still counts against them.
    pub fn with_stage_limits(mut self, limits: kala_query::Limits) -> Self {
        self.options.limits = limits;
        self
    }

    fn note_reads(&self, tx: &mut Txn<'_>) -> Result<()> {
        if !self.depends_on_snapshot {
            return Ok(());
        }
        if tx.snapshot_tx() < self.result.canon {
            return Err(invalid(
                "write input was read on a newer snapshot than this transaction",
            ));
        }
        let moved = tx.snapshot_tx() != self.result.canon;
        if !tx.schema_changes().is_empty()
            || (moved && self.defined_names != self.reads.defined_names(tx.base_world()))
        {
            tx.note_read_all();
        } else {
            self.reads.note(tx);
        }
        tx.set_read_version(self.result.canon);
        Ok(())
    }
}

/// A statement compiled against one snapshot: the reading part's plan, the
/// property cells the write clauses read from it, and the kind each
/// parameter must have. An adapter converts its parameters with
/// [`CompiledWrite::parameter_kinds`] before [`run`], so a handle given for a
/// ref-typed position of the reading part resolves exactly as in a read query.
pub struct CompiledWrite {
    prepared: Prepared,
    property_columns: BTreeMap<(String, String), usize>,
    kinds: Vec<(String, kala_query::ParamKind)>,
}

impl CompiledWrite {
    /// Every parameter of the statement, in lexical order, with the kind the
    /// reading part's compilation computed for it. A parameter used only as
    /// a write value is a scalar (or NULL / a list); a write tick is a tick.
    pub fn parameter_kinds(&self) -> &[(String, kala_query::ParamKind)] {
        &self.kinds
    }
}

/// Compile the reading part (a bare write has one empty row) without running it.
pub fn compile(world: &World, stmt: &WriteStatement) -> Result<CompiledWrite> {
    if world.is_system() {
        return Err(native::WriteError {
            operation: None,
            cause: native::Failure::SystemWorld,
        }
        .into());
    }
    let mut read = stmt.read.clone();
    let mut props = BTreeMap::new();
    let bound: std::collections::BTreeSet<_> = read.items.iter().map(|i| i.name.clone()).collect();
    let mut expressions = stmt.write_expressions();
    let mut serial = 0;
    while let Some(e) = expressions.pop() {
        match e {
            Expr::Property { entity, key, .. } if bound.contains(entity) => {
                let pair = (entity.clone(), key.clone());
                if props.contains_key(&pair) {
                    continue;
                }
                loop {
                    serial += 1;
                    let col = format!("\0write-property{serial}");
                    if !bound.contains(&col) {
                        read.items.push(Item {
                            name: col.clone(),
                            expr: e.clone(),
                        });
                        props.insert(pair, props.len());
                        break;
                    }
                }
            }
            Expr::List(xs) => expressions.extend(xs),
            _ => {}
        }
    }
    // Keep an occurrence for a pipeline that exposes no named variables.
    if read.items.is_empty() {
        read.items.push(Item {
            name: String::new(),
            expr: Expr::Value(Value::Bool(true)),
        });
    }
    let prepared = crate::compile::compile(world, "", &read)?;
    let kinds = stmt
        .params
        .iter()
        .map(|name| {
            let kind = prepared
                .params
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, kind)| kind.clone())
                .unwrap_or_else(|| {
                    if matches!(&stmt.tick, Some(Term::Param(p)) if p == name) {
                        kala_query::ParamKind::Tick
                    } else {
                        kala_query::ParamKind::Scalar
                    }
                });
            (name.clone(), kind)
        })
        .collect();
    Ok(CompiledWrite {
        prepared,
        property_columns: props,
        kinds,
    })
}

/// Run a compiled reading part on the snapshot it was compiled against. It is
/// refused, before its answer is built, when more rows than a write statement
/// may take reach the final projection.
pub fn run<'a>(
    world: &World,
    stmt: &WriteStatement,
    compiled: CompiledWrite,
    params: &dyn ParameterInput,
    options: RunOptions<'a>,
) -> Result<WriteInput<'a>> {
    for name in &stmt.params {
        if !params.contains_key(name) {
            return Err(QueryError::MissingParam(name.clone()).into());
        }
    }
    let tick = match &stmt.tick {
        None => None,
        Some(Term::Const(Value::I64(t))) => Some(Tick(*t)),
        Some(Term::Param(p)) => match params.get(p) {
            Some(Value::I64(t)) => Some(Tick(*t)),
            _ => {
                return Err(QueryError::ParamType {
                    name: p.clone(),
                    expected: "an integer tick".into(),
                }
                .into());
            }
        },
        _ => return Err(invalid("integer tick required")),
    };
    let CompiledWrite {
        prepared,
        property_columns,
        ..
    } = compiled;
    // The 10,000-row limit is on the rows that reach the write clauses: the
    // run is refused before it builds a larger answer. Intermediate
    // operators keep the caller's own row limit, so a selective MATCH over
    // a type with more nodes than that still works.
    let bounded = options.with_result_rows(
        options
            .result_rows
            .map_or(MAX_WRITE_ROWS, |rows| rows.min(MAX_WRITE_ROWS)),
    );
    let (result, usage) = match kala_query::run_with(world, &prepared.plan, params, bounded) {
        Err(QueryError::RowLimit { limit }) if limit == MAX_WRITE_ROWS => {
            return Err(row_limit());
        }
        other => other?,
    };
    if result.rows.len() > MAX_WRITE_ROWS {
        return Err(row_limit());
    }
    let reads = kala_query::reads::Reads::plan(&prepared.plan, world);
    let defined_names = reads.defined_names(world);
    Ok(WriteInput {
        prepared,
        reads,
        defined_names,
        depends_on_snapshot: stmt.requires_read(),
        result,
        property_columns,
        tick,
        options,
        bytes: usage.peak_bytes,
        steps: usage.steps,
    })
}

fn row_limit() -> WriteError {
    invalid("statement exceeds the 10000-row write limit")
}

/// [`compile`] then [`run`], for a caller whose parameters are already typed.
pub fn prepare<'a>(
    world: &World,
    stmt: &WriteStatement,
    params: &dyn ParameterInput,
    options: RunOptions<'a>,
) -> Result<WriteInput<'a>> {
    let compiled = compile(world, stmt)?;
    run(world, stmt, compiled, params, options)
}

/// A staged write and its explicit-transaction projection. Nodes and
/// relationships are identity values; the adapter renders the graph objects.
pub struct WriteResult {
    /// Estimated expanded write state to charge across RUNs until commit/rollback.
    pub retained_bytes: usize,
    pub staged: StagedWrite,
    pub rows: Vec<Vec<Datum>>,
    pub tick: Tick,
    properties: Vec<(usize, usize, EntityId, String)>,
    limits: kala_query::Limits,
    render_bytes: usize,
    deadline: Option<std::time::Instant>,
}

fn shape(mut values: Vec<Value>, many: bool) -> Datum {
    match values.len() {
        0 => Datum::Null,
        1 if !many => Datum::Value(values.remove(0)),
        _ => {
            values.sort();
            Datum::List(values.into_iter().map(Datum::Value).collect())
        }
    }
}

impl WriteResult {
    /// Re-evaluate property cells against the committed world, and resolve
    /// provisional identities before graph objects are rendered.
    pub fn committed_rows(&mut self, world: &World, ids: &IdMap) -> Result<Vec<Vec<Datum>>> {
        fn remap(d: &mut Datum, ids: &IdMap) {
            match d {
                Datum::Value(Value::Ref(id)) => *id = ids.resolve(*id),
                Datum::List(xs) => {
                    for d in xs {
                        remap(d, ids);
                    }
                }
                _ => {}
            }
        }
        let mut rows = std::mem::take(&mut self.rows);
        // Staging already charged the source rows and property metadata. Keep that
        // charge while replacements are allocated, including an old cell's payload.
        let budget = std::cell::Cell::new(self.render_bytes);
        let reserve = |bytes: usize| {
            budget.set(
                budget
                    .get()
                    .checked_sub(bytes)
                    .ok_or(QueryError::MemoryBudget {
                        limit: self.limits.max_bytes,
                    })?,
            );
            Ok::<(), QueryError>(())
        };
        let meter = WorkMeter::bounded(self.limits.max_steps, None).with_deadline(self.deadline);
        let schema = world.at_canon(world.latest_tx());
        let view = schema.metered(&meter);

        for row in &mut rows {
            meter.check()?;
            for d in row {
                remap(d, ids);
            }
        }
        for (r, c, id, key) in std::mem::take(&mut self.properties) {
            meter.check()?;
            let entity = ids.resolve(id);
            let cell = rows
                .get_mut(r)
                .and_then(|row| row.get_mut(c))
                .ok_or_else(|| invalid("RETURN projection was modified before rendering"))?;
            *cell = if key == "kala_handle" {
                if let Some(handle) = world.handle_of(entity) {
                    reserve(handle.len())?;
                    Datum::Value(Value::Str(handle.to_owned()))
                } else {
                    Datum::Null
                }
            } else if let Ok((def, inverse)) = schema.resolve_attribute(&key) {
                let many = if inverse {
                    def.reverse_cardinality == Some(Cardinality::Many)
                } else {
                    def.cardinality == Cardinality::Many
                };
                let vals = if def.value_type == kala_core::ValueType::Ref {
                    // Each retained neighbor requires an examined index key. Cap
                    // the scan before it builds a result larger than the budget.
                    let per_value = std::mem::size_of::<(EntityId, kala_core::EdgeId)>()
                        + std::mem::size_of::<Value>()
                        + std::mem::size_of::<Datum>();
                    let cap = budget.get() / per_value;
                    meter.set_max_keys(
                        self.limits
                            .max_steps
                            .min(meter.keys().saturating_add(cap as u64)),
                    );
                    let neighbors =
                        view.neighbors(entity, &key, kala_core::Direction::Out, self.tick)?;
                    reserve(neighbors.len().saturating_mul(per_value))?;
                    meter.set_max_keys(self.limits.max_steps);
                    neighbors
                        .into_iter()
                        .map(|(id, _)| Value::Ref(id))
                        .collect()
                } else {
                    let mut vals = vec![];
                    view.visit_non_ref_values_charged(
                        entity,
                        &key,
                        self.tick,
                        |encoded| {
                            reserve(encoded.saturating_add(
                                std::mem::size_of::<Value>() + std::mem::size_of::<Datum>(),
                            ))
                            .map_err(|_| {
                                kala_core::Error::WorkInterrupted {
                                    reason: kala_core::WorkInterruptReason::Limit,
                                }
                            })
                        },
                        |v| {
                            vals.push(v);
                            Ok(())
                        },
                    )?;
                    vals
                };
                shape(vals, many)
            } else {
                Datum::Null
            };
        }
        Ok(rows)
    }
}

/// Apply once per input row. Any staging error poisons the caller's entire
/// transaction; a prefix of a failed statement cannot be committed.
pub fn stage(
    tx: &mut Txn<'_>,
    stmt: &WriteStatement,
    input: WriteInput<'_>,
    params: &dyn ParameterInput,
) -> Result<WriteResult> {
    let result = apply(tx, stmt, input, params);
    if result.is_err() {
        tx.abort();
    }
    result
}

fn apply(
    tx: &mut Txn<'_>,
    stmt: &WriteStatement,
    input: WriteInput<'_>,
    params: &dyn ParameterInput,
) -> Result<WriteResult> {
    let deadline = input
        .options
        .deadline
        .into_iter()
        .chain(tx.deadline())
        .min();
    input.note_reads(tx)?;
    let tick = tx.report_tick(input.tick);
    let stager = Stager::new(tx, input.tick, Default::default(), MAX_WRITE_PRIMITIVES)?;
    let mut writer = Writer {
        tx,
        stager,
        params,
        tick,
        cols: &input.property_columns,
        snapshot: Vec::new(),
        touched: BTreeMap::new(),
        meter: WorkMeter::bounded(
            input.options.limits.max_steps.saturating_sub(input.steps),
            input.options.cancel,
        )
        .with_deadline(deadline),
        bytes: input.options.limits.max_bytes.saturating_sub(input.bytes),
        steps: input.options.limits.max_steps.saturating_sub(input.steps),
        limits: input.options.limits,
        step_bound: false,
    };
    let mut rows = Vec::new();
    let mut properties = Vec::new();
    let width = input.property_columns.len();
    let each = |writer: &mut Writer<'_, '_>| -> Result<()> {
        for mut data in input.result.rows {
            writer.snapshot = data.split_off(data.len() - width);
            writer.step()?;
            let mut row: Row = input
                .result
                .columns
                .iter()
                .map(|v| v.0.clone())
                .zip(data)
                .collect();
            for clause in &stmt.writes {
                writer.step()?;
                match clause {
                    Update::Create(paths) => {
                        for p in paths {
                            writer.path(p, false, &mut row)?;
                        }
                    }
                    Update::Merge {
                        path,
                        created,
                        matched,
                    } => {
                        let new = writer.path(path, true, &mut row)?;
                        writer.assign(if new { created } else { matched }, &row)?;
                    }
                    Update::Assign(assign) => writer.assign(assign, &row)?,
                    Update::Constraint {
                        label,
                        attribute,
                        value_type,
                    } => {
                        // Exactly `[:attribute :prop :type]`; a new label is
                        // a node type declared by the schema-on-write rule.
                        writer.stager.other(writer.tx, "attribute", |tx, tick| {
                            tx.infer_node_type(label)
                                .and_then(|()| {
                                    tx.declare_attribute_with(
                                        attribute,
                                        *value_type,
                                        &Default::default(),
                                    )
                                })
                                .map_err(native::Failure::Store)?;
                            Ok(native::Effect::schema(tick))
                        })?;
                    }
                    Update::Delete { names, detach } => {
                        for n in names {
                            writer.delete(n, *detach, &row)?;
                        }
                    }
                }
            }
            if !stmt.returns.is_empty() {
                writer.charge(
                    std::mem::size_of::<Vec<Datum>>()
                        + stmt
                            .returns
                            .len()
                            .saturating_mul(std::mem::size_of::<Datum>()),
                )?;
                let mut cells = Vec::with_capacity(stmt.returns.len());
                for (col, item) in stmt.returns.iter().enumerate() {
                    if let Expr::Property { entity, key, .. } = &item.expr
                        && let Some(Datum::Value(Value::Ref(id))) = row.get(entity)
                    {
                        writer.charge(
                            key.len() + std::mem::size_of::<(usize, usize, EntityId, String)>(),
                        )?;
                        properties.push((rows.len(), col, *id, key.clone()));
                    }
                    let d = writer.eval(&item.expr, &row, true)?;
                    cells.push(d);
                }
                rows.push(cells);
            }
        }
        Ok(())
    };
    if let Err(error) = each(&mut writer) {
        return Err(writer.limit(error));
    }
    writer.meter.check()?;
    Ok(WriteResult {
        retained_bytes: writer.stager.retained_bytes(),
        staged: writer.stager.finish(),
        rows,
        tick,
        properties,
        limits: input.options.limits,
        render_bytes: writer.bytes,
        deadline,
    })
}

struct Writer<'a, 't> {
    tx: &'a mut Txn<'t>,
    stager: Stager,
    params: &'a dyn ParameterInput,
    tick: Tick,
    cols: &'a BTreeMap<(String, String), usize>,
    snapshot: Vec<Datum>,
    touched: BTreeMap<EntityId, BTreeSet<String>>,
    meter: WorkMeter<'a>,
    bytes: usize,
    steps: u64,
    limits: kala_query::Limits,
    /// The stager's read meter is capped by the statement's step budget,
    /// not by its own read allowance.
    step_bound: bool,
}
impl Writer<'_, '_> {
    /// Name the limit a stager read really hit.
    fn limit(&self, error: WriteError) -> WriteError {
        match error {
            WriteError::Stage(native::WriteError {
                cause: native::Failure::ReadWork(_),
                ..
            }) if self.step_bound => QueryError::StepBudget {
                limit: self.limits.max_steps,
            }
            .into(),
            other => other,
        }
    }
    fn step(&mut self) -> Result<()> {
        self.meter.check()?;
        self.steps = self.steps.checked_sub(1).ok_or(QueryError::StepBudget {
            limit: self.limits.max_steps,
        })?;
        // The reads SET, REMOVE and DELETE make through the stager are
        // bounded by min(256 index keys x the 10,000 primitives a statement
        // may stage, the statement's remaining step budget), and by its
        // deadline. (A Datalog form has the same 256 keys for each of its
        // 256 operations.)
        let native_reads = self.stager.read_meter();
        let remaining = self.steps.saturating_sub(self.meter.keys());
        self.step_bound = remaining < self.stager.read_limit();
        native_reads.set_max_keys(self.stager.read_limit().min(remaining));
        self.meter
            .set_max_keys(self.steps.saturating_sub(native_reads.keys()));
        if self.meter.keys().saturating_add(native_reads.keys()) > self.steps {
            return Err(QueryError::StepBudget {
                limit: self.limits.max_steps,
            }
            .into());
        }
        Ok(())
    }
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_sub(bytes)
            .ok_or(QueryError::MemoryBudget {
                limit: self.limits.max_bytes,
            })?;
        Ok(())
    }
    /// The bulk import lists can be much larger than one updated property.
    /// Inspect the bindings in place and charge before cloning only the selected cell.
    fn indexed_value(
        &mut self,
        list: &Expr<String>,
        index: &Expr<String>,
        row: &Row,
    ) -> Result<Datum> {
        self.step()?;
        let list = match list {
            Expr::Param(name) => self
                .params
                .binding(name)
                .ok_or_else(|| QueryError::MissingParam(name.clone()))?,
            Expr::Var(name) => ParameterValue::Datum(
                row.get(name)
                    .ok_or_else(|| invalid("unbound list variable"))?,
            ),
            _ => {
                return Err(invalid(
                    "write indexing expects a list parameter or variable",
                ));
            }
        };
        let index = match index {
            Expr::Null => return Ok(Datum::Null),
            Expr::Value(v) => ParameterValue::Scalar(v),
            Expr::Param(p) => self
                .params
                .binding(p)
                .ok_or_else(|| QueryError::MissingParam(p.clone()))?,
            Expr::Var(v) => {
                ParameterValue::Datum(row.get(v).ok_or_else(|| invalid("unbound list offset"))?)
            }
            _ => return Err(invalid("a list offset needs an integer")),
        };
        if matches!(list, ParameterValue::Datum(Datum::Null))
            || matches!(index, ParameterValue::Datum(Datum::Null))
        {
            return Ok(Datum::Null);
        }
        let ParameterValue::Datum(Datum::List(values)) = list else {
            return Err(QueryError::TypeMismatch("list indexing requires a list".into()).into());
        };
        let (ParameterValue::Scalar(Value::I64(i))
        | ParameterValue::Datum(Datum::Value(Value::I64(i)))) = index
        else {
            return Err(QueryError::TypeMismatch("list offset must be an integer".into()).into());
        };
        match kala_query::rows::list_element(values, *i) {
            Some(value) => {
                self.charge(datum_bytes(value))?;
                Ok(value.clone())
            }
            None => Ok(Datum::Null),
        }
    }
    fn eval(&mut self, e: &Expr<String>, row: &Row, returned: bool) -> Result<Datum> {
        self.step()?;
        let borrowed = match e {
            Expr::ListIndex(list, index) => return self.indexed_value(list, index, row),
            Expr::Var(n) => Some(
                row.get(n)
                    .ok_or_else(|| invalid("unbound write variable"))?,
            ),
            Expr::Property { entity, key, .. } => {
                let id = match row.get(entity) {
                    Some(Datum::Value(Value::Ref(id))) => *id,
                    Some(Datum::Null) => return Ok(Datum::Null),
                    _ => return Err(invalid("property requires a node or relationship")),
                };
                if !returned
                    && !self
                        .touched
                        .get(&id)
                        .is_some_and(|attrs| attrs.contains(key))
                    && let Some(col) = self.cols.get(&(entity.clone(), key.clone()))
                {
                    let col = *col;
                    self.charge(datum_bytes(&self.snapshot[col]))?;
                    return Ok(self.snapshot[col].clone());
                } else {
                    return self.property(id, key);
                }
            }
            Expr::Null => return Ok(Datum::Null),
            Expr::Value(v) => {
                self.charge(value_bytes(v))?;
                return Ok(Datum::Value(v.clone()));
            }
            Expr::Param(p) => match self
                .params
                .binding(p)
                .ok_or_else(|| QueryError::MissingParam(p.clone()))?
            {
                ParameterValue::Scalar(v) => {
                    self.charge(value_bytes(v))?;
                    return Ok(Datum::Value(v.clone()));
                }
                ParameterValue::Datum(d) => Some(d),
            },
            Expr::List(xs) => {
                return xs
                    .iter()
                    .map(|e| self.eval(e, row, returned))
                    .collect::<Result<Vec<_>>>()
                    .map(Datum::List);
            }
            _ => return Err(invalid("unsupported expression in write")),
        };
        let d = borrowed.ok_or_else(|| invalid("missing value"))?;
        self.charge(datum_bytes(d))?;
        Ok(d.clone())
    }
    fn property(&mut self, id: EntityId, key: &str) -> Result<Datum> {
        if key == "kala_handle" {
            if let Some(len) = self.tx.handle_of(id).map(str::len) {
                self.charge(len)?;
            }
            return Ok(self
                .tx
                .handle_of(id)
                .map(|h| Datum::Value(Value::Str(h.into())))
                .unwrap_or(Datum::Null));
        }
        let Some(cardinality) = self.tx.property_cardinality(key) else {
            return Ok(Datum::Null);
        };
        let many = cardinality == Cardinality::Many;
        let held = self.tx.values_at(
            id,
            key,
            self.tick,
            MAX_WRITE_PRIMITIVES,
            self.bytes,
            &self.meter,
        )?;
        if held.more {
            return Err(invalid(
                "property value exceeds the write statement value limit",
            ));
        }
        let d = shape(held.values, many);
        self.charge(datum_bytes(&d))?;
        Ok(d)
    }
    fn fact(&mut self, id: EntityId, key: &str, value: Value, op: Op) -> Result<usize> {
        self.step()?;
        if !self.cols.is_empty() {
            self.touched.entry(id).or_default().insert(key.to_owned());
            if let Value::Ref(to) = &value
                && let Some(def) = self.tx.attribute(key)
            {
                if def.direction == Some(kala_core::RefDirection::Bidirectional) {
                    self.touched.entry(*to).or_default().insert(key.to_owned());
                } else if let Some(inverse) = &def.inverse {
                    self.touched.entry(*to).or_default().insert(inverse.clone());
                }
            }
        }
        Ok(self
            .stager
            .fact(self.tx, id, key, value, op, None, Precision::Exact)?)
    }
    /// A string written to a ref attribute names an entity by its handle, as
    /// a Datalog form's `:handle` does (entities created earlier in the
    /// transaction included). A handle nobody holds is the ordinary
    /// unknown-entity error. An undeclared attribute keeps the string: it
    /// is inferred as a string property.
    fn handles(&mut self, id: EntityId, key: &str, values: &mut [Value]) -> Result<()> {
        if !values.iter().any(|v| matches!(v, Value::Str(_))) {
            return Ok(());
        }
        let stored = self.tx.relationship(id, key, id).1;
        if !self
            .tx
            .attribute(stored)
            .is_some_and(|def| def.value_type == kala_core::ValueType::Ref)
        {
            return Ok(());
        }
        for value in values {
            if let Value::Str(handle) = value {
                let Some(entity) = self.tx.entity_by_handle(handle) else {
                    self.tx.abort();
                    return Err(native::WriteError {
                        operation: None,
                        cause: native::Failure::UnknownEntity(std::mem::take(handle)),
                    }
                    .into());
                };
                *value = Value::Ref(entity);
            }
        }
        Ok(())
    }
    /// Whether `name` is undeclared, recording that its absence was read:
    /// a newer declaration of the name then conflicts with this
    /// transaction under optimistic concurrency.
    fn absent(&self, id: EntityId, name: &str) -> bool {
        let stored = self.tx.relationship(id, name, id);
        if self.tx.attribute(stored.1).is_some() {
            return false;
        }
        // The lookup's error is the answer; the read of the name is recorded.
        let _ = self.tx.note_read_attribute(name);
        true
    }
    fn assign(&mut self, items: &[Assignment], row: &Row) -> Result<()> {
        for item in items {
            self.step()?;
            match item {
                Assignment::Label(var, label, assert) => {
                    // A NULL variable (OPTIONAL MATCH found nothing) has
                    // nothing to write to: the row is skipped, as for DELETE.
                    if row.get(var) == Some(&Datum::Null) {
                        continue;
                    }
                    let id = reference(row.get(var))?;
                    if id.is_edge() {
                        return Err(invalid("labels apply only to nodes"));
                    }
                    if !*assert && self.absent(id, "label") {
                        continue;
                    }
                    self.fact(
                        id,
                        "label",
                        Value::Str(label.clone()),
                        if *assert { Op::Assert } else { Op::Retract },
                    )?;
                }
                Assignment::Property(var, key, expr) => {
                    if key == "kala_handle" {
                        return Err(invalid("kala_handle is permanent"));
                    }
                    if row.get(var) == Some(&Datum::Null) {
                        continue;
                    }
                    let id = reference(row.get(var))?;
                    let mut wanted = if let Some(e) = expr {
                        values(self.eval(e, row, false)?)?
                    } else {
                        vec![]
                    };
                    self.handles(id, key, &mut wanted)?;
                    // Removing a property nobody declared changes nothing
                    // (in Datalog an empty `:set` of an unknown name is an
                    // error; in Cypher REMOVE and `= NULL` are no-ops).
                    if wanted.is_empty() && self.absent(id, key) {
                        continue;
                    }
                    self.stager
                        .set(self.tx, id, key, &wanted, None, Precision::Exact)?;
                    if !self.cols.is_empty() {
                        self.touched.entry(id).or_default().insert(key.clone());
                    }
                }
            }
        }
        Ok(())
    }
    fn path(&mut self, path: &Path, merge: bool, row: &mut Row) -> Result<bool> {
        let mut nodes = vec![];
        let mut changed = false;
        for node in &path.nodes {
            self.step()?;
            if node.labels.is_empty() {
                let id = reference(node.name.as_ref().and_then(|n| row.get(n)))?;
                if id.is_edge() {
                    return Err(invalid("relationship endpoints must be nodes"));
                }
                nodes.push(id);
                continue;
            }
            let handle = if let Some((_, e)) = node.props.iter().find(|(k, _)| k == "kala_handle") {
                match self.eval(e, row, false)? {
                    Datum::Value(Value::Str(s)) => Some(s),
                    _ => return Err(invalid("kala_handle requires a string")),
                }
            } else {
                None
            };
            let (id, created) =
                self.stager
                    .create(self.tx, &node.labels[0], handle.as_deref(), merge)?;
            changed = created;
            if let Some(n) = &node.name {
                row.insert(n.clone(), Datum::Value(Value::Ref(id)));
            }
            for l in &node.labels[1..] {
                self.fact(id, "label", Value::Str(l.clone()), Op::Assert)?;
            }
            self.properties(id, &node.props, row, true)?;
            nodes.push(id);
        }
        for (i, edge) in path.edges.iter().enumerate() {
            let (a, b) = if edge.direction == kala_query::Orientation::In {
                (nodes[i + 1], nodes[i])
            } else {
                (nodes[i], nodes[i + 1])
            };
            let (src, kind, tgt) = self.tx.relationship(a, &edge.kind, b);
            let kind = kind.to_owned();
            // A conditional MERGE branch depends on whether the edge was held.
            if merge && self.tx.attribute(&kind).is_some() {
                self.tx.note_read(src, &kind)?;
            }
            changed = self.fact(src, &kind, Value::Ref(tgt), Op::Assert)? != 0;
            if edge.name.is_some() || !edge.props.is_empty() {
                let id = EntityId::from(
                    self.tx
                        .edge_id(src, &kind, tgt)
                        .ok_or_else(|| invalid("assert did not resolve an edge"))?,
                );
                if let Some(n) = &edge.name {
                    row.insert(n.clone(), Datum::Value(Value::Ref(id)));
                }
                self.properties(id, &edge.props, row, false)?;
            }
        }
        Ok(changed)
    }
    fn properties(
        &mut self,
        id: EntityId,
        props: &[(String, Expr<String>)],
        row: &Row,
        node: bool,
    ) -> Result<()> {
        for (key, e) in props {
            if node && key == "kala_handle" {
                continue;
            }
            let mut wanted = values(self.eval(e, row, false)?)?;
            self.handles(id, key, &mut wanted)?;
            for v in wanted {
                // A ref-valued key is a relationship type: an inverse name
                // writes the stored attribute with the endpoints flipped,
                // exactly as `SET n.key = m` and `CREATE (n)-[:key]->(m)`.
                if let Value::Ref(to) = v {
                    let (src, kind, tgt) = self.tx.relationship(id, key, to);
                    let kind = kind.to_owned();
                    self.fact(src, &kind, Value::Ref(tgt), Op::Assert)?;
                } else {
                    self.fact(id, key, v, Op::Assert)?;
                }
            }
        }
        Ok(())
    }
    fn delete(&mut self, name: &str, detach: bool, row: &Row) -> Result<()> {
        self.step()?;
        if row.get(name) == Some(&Datum::Null) {
            return Ok(());
        }
        let id = reference(row.get(name))?;
        if let Some(edge) = id.as_edge() {
            if detach {
                return Err(invalid("DETACH DELETE applies only to nodes"));
            }
            let (src, attr, tgt) = self
                .tx
                .edge(edge)
                .ok_or_else(|| invalid("unknown relationship"))?;
            self.fact(src, &attr, Value::Ref(tgt), Op::Retract)?;
        } else {
            let facts = self
                .tx
                .delete_facts(
                    id,
                    self.tick,
                    detach,
                    self.stager.remaining_operations(),
                    self.stager.read_meter(),
                )
                .map_err(|e| match e {
                    kala_core::Error::WorkInterrupted {
                        reason: kala_core::WorkInterruptReason::Limit,
                    } => WriteError::Stage(native::WriteError {
                        operation: None,
                        cause: self.stager.read_limit_failure(),
                    }),
                    e => e.into(),
                })?;
            for (s, k, v) in facts {
                self.fact(s, &k, v, Op::Retract)?;
            }
        }
        Ok(())
    }
}
fn reference(value: Option<&Datum>) -> Result<EntityId> {
    match value {
        Some(Datum::Value(Value::Ref(e))) => Ok(*e),
        _ => Err(invalid("write requires a bound node or relationship")),
    }
}
fn value_bytes(v: &Value) -> usize {
    std::mem::size_of::<Datum>()
        + match v {
            Value::Str(s) => s.len(),
            _ => 0,
        }
}
fn datum_bytes(d: &Datum) -> usize {
    fn size(d: &Datum, depth: usize) -> usize {
        if depth >= crate::MAX_DEPTH {
            return usize::MAX;
        }
        match d {
            Datum::Value(v) => value_bytes(v),
            Datum::List(xs) => xs.iter().fold(std::mem::size_of::<Datum>(), |n, d| {
                n.saturating_add(size(d, depth + 1))
            }),
            Datum::Path(p) => std::mem::size_of::<Datum>() + p.nodes().len() * 24,
            _ => std::mem::size_of::<Datum>(),
        }
    }
    size(d, 0)
}
fn values(d: Datum) -> Result<Vec<Value>> {
    match d {
        Datum::Null => Ok(vec![]),
        Datum::Value(v) => Ok(vec![v]),
        Datum::List(xs) => xs
            .into_iter()
            .map(|x| {
                if let Datum::Value(v) = x {
                    Ok(v)
                } else {
                    Err(invalid("property lists must contain stored scalar values"))
                }
            })
            .collect(),
        _ => Err(invalid("property requires a stored value or list")),
    }
}

/// Execute one statement on the native transaction path.
pub fn execute(
    world: &mut World,
    stmt: &WriteStatement,
    params: &dyn ParameterInput,
    options: RunOptions<'_>,
) -> Result<WriteReport> {
    execute_in(world, stmt, params, options, Concurrency::default())
}

/// Execute using the caller's concurrency policy. Reads and writes share
/// the snapshot acquired by begin, including a catch-up to another writer.
pub fn execute_in(
    world: &mut World,
    stmt: &WriteStatement,
    params: &dyn ParameterInput,
    options: RunOptions<'_>,
    mode: Concurrency,
) -> Result<WriteReport> {
    let mut tx = world.begin();
    tx.set_concurrency(mode);
    if let Some(at) = options.deadline {
        tx.set_deadline(at);
    }
    let input = prepare(tx.base_world(), stmt, params, options)?;
    let staged = stage(&mut tx, stmt, input, params)?.staged;
    Ok(native::commit(tx, staged)?)
}
