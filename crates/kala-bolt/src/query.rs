//! Language dispatch and bounded result enrichment at the pinned tick and canon.
use crate::error::{EXECUTION, FORBIDDEN, Failure, MISSING, TYPE};
use crate::log::logln;
use crate::message::Map;
use crate::packstream::{self, Value};
use kala_core::{
    Cardinality, EntityId, MeteredView, Tick, Value as CoreValue, ValueType, WorkMeter, World,
};
use kala_query::{Datum, Limits, ParamKind, QueryError, QueryParams};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

pub(crate) struct Output {
    pub fields: Vec<String>,
    pub rows: VecDeque<Vec<u8>>,
    pub encoded_bytes: usize,
    pub exposes: VecDeque<bool>,
    pub reads: crate::reads::Reads,
}
impl Output {
    pub fn empty() -> Self {
        Self {
            fields: vec![],
            rows: VecDeque::new(),
            encoded_bytes: 0,
            exposes: VecDeque::new(),
            reads: Default::default(),
        }
    }
}
/// Prepares and runs a query. `stop` (server shutdown) and `deadline` (the
/// request's or its transaction's) both end it early; past the deadline the
/// answer is the timeout failure.
pub(crate) fn run(
    world: &World,
    text: &str,
    given: &Map,
    stop: &AtomicBool,
    max_message: usize,
    max_depth: usize,
    deadline: Instant,
) -> Result<Output, Failure> {
    until(deadline, |deadline| {
        run_inner(world, text, given, stop, max_message, max_depth, deadline)
    })
}
fn run_inner(
    world: &World,
    text: &str,
    given: &Map,
    stop: &AtomicBool,
    max_message: usize,
    max_depth: usize,
    deadline: Instant,
) -> Result<Output, Failure> {
    let source = crate::language::detect(text)?;
    let front_end = source.front_end;
    let system = crate::system::is_system(world);
    // On `system`, refuse before evaluation: output redaction alone can't
    // stop `<`, `=`, `min` or `IS NULL` from revealing a hash. The front-end
    // checks its AST; the plan check covers whatever the compiler resolves.
    let secret = |name: &str| system && crate::system::is_secret_attribute(world, name);
    let prepared = front_end.prepare(world, &source.text, &secret)?;
    if prepared.plan.attribute_names().into_iter().any(secret) {
        return Err(protected());
    }
    logln!(
        "event=query_prepared language={} columns={} parameters={}",
        front_end.name(),
        prepared.plan.find.len(),
        prepared.params.len()
    );
    let params = parameters(world, &prepared.params, given, front_end.parameter_policy())?;
    execute(
        world,
        &prepared,
        params,
        Stop {
            flag: stop,
            deadline,
        },
        max_message,
        max_depth,
        system,
    )
}
/// What ends a run early: the server's shutdown flag and the request's
/// deadline.
#[derive(Clone, Copy)]
pub(crate) struct Stop<'a> {
    pub flag: &'a AtomicBool,
    pub deadline: Instant,
}
/// Prepare adapter requests through the language seam; no query-text matching here.
pub(crate) fn server_statement(
    text: &str,
    system: bool,
) -> Result<Option<kala_lang::ServerStatement>, Failure> {
    let source = crate::language::detect(text)?;
    source
        .front_end
        .prepare_server(&source.text, &|name| system && name == "password_hash")
        .map_err(Failure::from)
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_server(
    statement: &kala_lang::ServerStatement,
    rows: Vec<Datum>,
    given: &Map,
    stop: Stop<'_>,
    max_message: usize,
    max_depth: usize,
    system: bool,
) -> Result<Output, Failure> {
    if system
        && statement
            .prepared
            .plan
            .attribute_names()
            .into_iter()
            .any(|name| name == "password_hash")
    {
        return Err(protected());
    }
    let world = World::in_memory(kala_core::Schema::new()).map_err(Failure::store)?;
    let mut params = parameters(
        &world,
        &statement.prepared.params,
        given,
        statement.parameter_policy,
    )?;
    params.insert(kala_query::SERVER_ROWS_PARAMETER.into(), Datum::List(rows));
    execute(
        &world,
        &statement.prepared,
        params,
        stop,
        max_message,
        max_depth,
        system,
    )
}
#[allow(clippy::too_many_arguments)]
fn execute(
    world: &World,
    prepared: &kala_query::Prepared,
    params: QueryParams,
    stop: Stop<'_>,
    max_message: usize,
    max_depth: usize,
    system: bool,
) -> Result<Output, Failure> {
    let limits = Limits::default();
    let result = kala_query::run_with(
        world,
        &prepared.plan,
        &params,
        kala_query::RunOptions::new(limits)
            .with_cancel(stop.flag)
            .with_deadline(stop.deadline),
    )
    .map(|(rs, _)| rs)
    .map_err(query_error)?;
    let reads = crate::reads::Reads::plan(&prepared.plan, world);
    render(
        RenderWorld::Snapshot(world, result.canon),
        result.columns.into_iter().map(|v| v.0).collect(),
        result.rows,
        result.tick,
        reads,
        RenderOptions {
            stop,
            max_message,
            max_depth,
            system,
            result_bytes: limits.max_bytes,
            preview: false,
        },
    )
}

/// A returned graph value can be read from a pinned canon or from an explicit
/// transaction's staged state. Neither view changes what MATCH can see.
pub(crate) enum RenderWorld<'a> {
    Snapshot(&'a World, kala_core::TxId),
    Staged(&'a kala_core::Txn<'a>),
}
enum Graph<'a> {
    Snapshot(MeteredView<'a>),
    Staged(&'a kala_core::Txn<'a>),
}
impl Graph<'_> {
    fn attributes(&self) -> Box<dyn Iterator<Item = &kala_core::AttrDef> + '_> {
        match self {
            Self::Snapshot(v) => Box::new(v.attributes()),
            Self::Staged(t) => Box::new(t.attributes()),
        }
    }
    fn format_id(&self, id: EntityId) -> String {
        match self {
            Self::Snapshot(v) => v.world().format_id(id),
            Self::Staged(t) => t.format_id(id),
        }
    }
    fn handle_of(&self, id: EntityId) -> Option<&str> {
        match self {
            Self::Snapshot(v) => v.world().handle_of(id),
            Self::Staged(t) => t.handle_of(id),
        }
    }
    fn edge(&self, id: kala_core::EdgeId) -> Option<(EntityId, String, EntityId)> {
        match self {
            Self::Snapshot(v) => v.edge(id),
            Self::Staged(t) => t.edge(id),
        }
    }
    fn values(
        &self,
        id: EntityId,
        name: &str,
        tick: Tick,
        meter: &WorkMeter<'_>,
        visit: impl FnMut(CoreValue) -> Result<(), kala_core::Error>,
    ) -> Result<(), kala_core::Error> {
        match self {
            Self::Snapshot(v) => v.visit_non_ref_values(id, name, tick, visit),
            Self::Staged(t) => {
                let held = t.values_at(
                    id,
                    name,
                    tick,
                    crate::write::MAX_STAGED_BYTES / 64,
                    crate::write::MAX_STAGED_BYTES,
                    meter,
                )?;
                if held.more {
                    return Err(kala_core::Error::WorkInterrupted {
                        reason: kala_core::WorkInterruptReason::Limit,
                    });
                }
                held.values.into_iter().try_for_each(visit)
            }
        }
    }
}

pub(crate) struct RenderOptions<'a> {
    stop: Stop<'a>,
    max_message: usize,
    max_depth: usize,
    system: bool,
    result_bytes: usize,
    /// A size check before a commit: nothing is sent, so nothing is read on
    /// the client's behalf and no read at all is recorded on the
    /// transaction (`Txn::without_read_recording`).
    preview: bool,
}
impl<'a> RenderOptions<'a> {
    pub fn preview(self) -> Self {
        Self {
            preview: true,
            ..self
        }
    }
    pub fn write(stop: Stop<'a>, max_message: usize, max_depth: usize) -> Self {
        Self {
            stop,
            max_message,
            max_depth,
            system: false,
            result_bytes: crate::write::MAX_STAGED_BYTES,
            preview: false,
        }
    }
}
pub(crate) fn render(
    world: RenderWorld<'_>,
    fields: Vec<String>,
    records: Vec<Vec<Datum>>,
    tick: Tick,
    mut reads: crate::reads::Reads,
    options: RenderOptions<'_>,
) -> Result<Output, Failure> {
    if options.preview
        && let RenderWorld::Staged(tx) = world
    {
        // Rendering reads every property of a returned node or edge
        // (`Txn::values_at`). In a preview nobody is shown them and the
        // real answer is rendered again from the committed world, so none
        // of it is a read a later commit could invalidate.
        let options = RenderOptions {
            preview: false,
            ..options
        };
        return tx.without_read_recording(|tx| {
            render(
                RenderWorld::Staged(tx),
                fields,
                records,
                tick,
                reads,
                options,
            )
        });
    }
    let RenderOptions {
        stop,
        max_message,
        max_depth,
        system,
        result_bytes,
        preview: _,
    } = options;
    let limits = Limits {
        max_bytes: result_bytes,
        ..Limits::default()
    };
    let meter =
        WorkMeter::bounded(limits.max_steps, Some(stop.flag)).with_deadline(Some(stop.deadline));
    let view = match world {
        RenderWorld::Snapshot(w, canon) => Graph::Snapshot(w.at_canon(canon).metered(&meter)),
        RenderWorld::Staged(t) => Graph::Staged(t),
    };
    let mut budget = Budget {
        bytes: 0,
        steps: 0,
        limits,
        system,
    };
    let mut rows = VecDeque::new();
    let mut exposes = VecDeque::new();
    for row in records {
        let mut provisional = false;
        budget.step(&meter)?;
        let mut values = Vec::new();
        for datum in row {
            // Graph-value enrichment reads properties beyond those explicitly named.
            visit_ids(&datum, &mut |id| {
                reads.rendered(id);
                if let Graph::Staged(tx) = &view {
                    tx.note_read_entity(id);
                    provisional |= tx.is_provisional(id);
                }
            });
            values.push(datum_value(datum, &view, tick, &meter, &mut budget)?);
        }
        exposes.push_back(provisional);
        let record = Value::Structure(0x71, vec![Value::List(values)]);
        let bytes =
            packstream::encode_with_limits(&record, max_message, max_depth).map_err(|_| {
                Failure::new(EXECUTION, "result message size or nesting limit exceeded")
            })?;
        budget.charge(bytes.len())?;
        rows.push_back(bytes);
    }
    Ok(Output {
        fields,
        encoded_bytes: rows.iter().map(Vec::len).sum(),
        rows,
        exposes,
        reads,
    })
}
/// Generic on purpose: says what is refused, nothing about stored values.
pub(crate) fn protected() -> Failure {
    Failure::new(
        FORBIDDEN,
        "queries on the system database may not read the protected attribute password_hash",
    )
}
pub(crate) fn parameters(
    world: &World,
    kinds: &[(String, ParamKind)],
    given: &Map,
    policy: kala_lang::ParamPolicy,
) -> Result<QueryParams, Failure> {
    policy
        .bind(kinds, |name, kind| {
            use kala_lang::params::ParameterTree;
            given
                .get(name)
                .map(|v| policy.convert(world, kind, v.input(), 0))
                .transpose()
        })
        .map_err(|e| {
            let code = if matches!(e, kala_lang::params::BindError::Missing(_)) {
                MISSING
            } else {
                TYPE
            };
            Failure::new(code, e.to_string())
        })
}

impl kala_lang::params::ParameterTree for Value {
    fn input(&self) -> kala_lang::params::Input<'_, Self> {
        use kala_lang::params::Input;
        match self {
            Self::Null => Input::Null,
            Self::Bool(b) => Input::Bool(*b),
            Self::Integer(n) => Input::Integer(*n),
            Self::Float(f) => Input::Float(*f),
            Self::String(s) => Input::String(s),
            Self::List(xs) => Input::List(xs),
            _ => Input::Unsupported,
        }
    }
}
pub(crate) fn query_error(error: QueryError) -> Failure {
    match error {
        QueryError::Core(e) => core_error(e),
        QueryError::RowLimit { .. } => Failure::new(EXECUTION, "query row limit exceeded"),
        QueryError::StepBudget { .. } => Failure::new(EXECUTION, "query step limit exceeded"),
        QueryError::MemoryBudget { .. } => Failure::new(EXECUTION, "query memory limit exceeded"),
        QueryError::Cancelled => Failure::new(EXECUTION, "query cancelled"),
        QueryError::MissingParam(_) => Failure::new(MISSING, "missing query parameter"),
        QueryError::ParamType { .. } | QueryError::TypeMismatch(_) => {
            Failure::new(TYPE, "query value has the wrong type")
        }
        _ => Failure::new(EXECUTION, "query execution failed"),
    }
}
fn core_error(error: kala_core::Error) -> Failure {
    match error {
        kala_core::Error::WorkInterrupted {
            reason: kala_core::WorkInterruptReason::Stopped,
        } => Failure::new(EXECUTION, "query cancelled"),
        kala_core::Error::WorkInterrupted { .. } => {
            Failure::new(EXECUTION, "result enrichment step limit exceeded")
        }
        e => Failure::store(e),
    }
}
// Separate conservative enrichment budget, including intermediate values and
// encoded rows. It never resets per entity or attribute. Core visits clone at
// most one stored value at a time (a log entry itself is bounded at 16 MiB).
struct Budget {
    bytes: usize,
    steps: u64,
    limits: Limits,
    system: bool,
}
impl Budget {
    fn charge(&mut self, n: usize) -> Result<(), Failure> {
        self.bytes = self
            .bytes
            .checked_add(n)
            .ok_or_else(|| Failure::new(EXECUTION, "result enrichment memory limit exceeded"))?;
        if self.bytes > self.limits.max_bytes {
            return Err(Failure::new(
                EXECUTION,
                "result enrichment memory limit exceeded",
            ));
        }
        Ok(())
    }
    fn step(&mut self, meter: &WorkMeter<'_>) -> Result<(), Failure> {
        self.steps += 1;
        if self.steps.saturating_add(meter.keys()) > self.limits.max_steps {
            return Err(Failure::new(
                EXECUTION,
                "result enrichment step limit exceeded",
            ));
        }
        meter.set_max_keys(self.limits.max_steps - self.steps);
        meter.check().map_err(core_error)
    }
    fn string(&mut self, s: String) -> Result<Value, Failure> {
        self.charge(64 + s.len())?;
        Ok(Value::String(s))
    }
}
fn datum_value(
    d: Datum,
    view: &Graph<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Value, Failure> {
    budget.charge(64)?;
    match d {
        Datum::Null => Ok(Value::Null),
        Datum::Path(path) => path_value(&path, view, tick, meter, budget),
        Datum::List(values) => values
            .into_iter()
            .map(|v| datum_value(v, view, tick, meter, budget))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        Datum::Value(CoreValue::Ref(id)) => entity_value(id, view, tick, meter, budget),
        Datum::Value(v) => scalar(v, budget),
        Datum::Op(op) => budget.string(
            match op {
                kala_core::Op::Assert => "assert",
                kala_core::Op::Retract => "retract",
            }
            .into(),
        ),
        Datum::Precision(p) => budget.string(format!("{p:?}").to_ascii_lowercase()),
        Datum::Intervals(set) => {
            budget.charge(set.len().saturating_mul(192))?;
            Ok(Value::List(
                set.intervals()
                    .iter()
                    .map(|i| {
                        Value::List(vec![
                            Value::Integer(i.from.0),
                            i.until.map_or(Value::Null, |t| Value::Integer(t.0)),
                        ])
                    })
                    .collect(),
            ))
        }
    }
}
fn scalar(value: CoreValue, budget: &mut Budget) -> Result<Value, Failure> {
    budget.charge(64)?;
    Ok(match value {
        CoreValue::Bool(v) => Value::Bool(v),
        CoreValue::I64(v) => Value::Integer(v),
        CoreValue::F64(v) => Value::Float(v),
        CoreValue::Str(s) => {
            return budget.string(if budget.system && s.starts_with("$argon2") {
                "<redacted>".into()
            } else {
                s
            });
        }
        CoreValue::Ref(_) => return Err(Failure::new(EXECUTION, "unexpected reference property")),
    })
}
fn properties(
    id: EntityId,
    view: &Graph<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Map, Failure> {
    let mut props = BTreeMap::new();
    for attr in view.attributes() {
        budget.step(meter)?;
        if attr.value_type == ValueType::Ref {
            continue;
        }
        let mut values = Vec::new();
        let mut failure = None;
        view.values(id, &attr.name, tick, meter, |value| {
            let n = 64
                + match &value {
                    CoreValue::Str(s) => s.len(),
                    _ => 0,
                };
            if let Err(e) = budget.charge(n) {
                failure = Some(e);
                return Err(kala_core::Error::WorkInterrupted {
                    reason: kala_core::WorkInterruptReason::Limit,
                });
            }
            values.push(if budget.system && attr.name == "password_hash" {
                CoreValue::Str("<redacted>".into())
            } else {
                value
            });
            Ok(())
        })
        .map_err(|e| failure.unwrap_or_else(|| core_error(e)))?;
        if values.is_empty() {
            continue;
        }
        // Core Value ordering is allocation-free, including unindexed strings.
        // Charge comparison work conservatively by total payload * log2(count).
        let sort_cost = values
            .iter()
            .map(|v| match v {
                CoreValue::Str(s) => s.len() / 64 + 1,
                _ => 1,
            })
            .sum::<usize>()
            .saturating_mul(values.len().ilog2() as usize + 1);
        budget.steps = budget.steps.saturating_add(sort_cost as u64);
        budget.step(meter)?;
        values.sort_unstable();
        let many = attr.cardinality == Cardinality::Many || values.len() > 1;
        budget.charge(128 + attr.name.len())?;
        let mut values = values
            .into_iter()
            .map(|v| scalar(v, budget))
            .collect::<Result<Vec<_>, _>>()?;
        let value = if many {
            Value::List(values)
        } else {
            values.pop().ok_or_else(Failure::invalid)?
        };
        props.insert(attr.name.clone(), value);
    }
    // The reserved prefix keeps the permanent handle separate from user facts.
    if let Some(handle) = view.handle_of(id) {
        props.insert("kala_handle".into(), budget.string(handle.into())?);
    }
    Ok(props)
}
fn entity_value(
    id: EntityId,
    view: &Graph<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Value, Failure> {
    budget.step(meter)?;
    budget.charge(1024)?;
    let props = properties(id, view, tick, meter, budget)?;
    let world = view;
    let element_id = budget.string(world.format_id(id))?;
    if let Some(edge) = id.as_edge() {
        let (source, attr, target) = view
            .edge(edge)
            .ok_or_else(|| Failure::new(EXECUTION, "relationship is not visible at this canon"))?;
        Ok(Value::Structure(
            0x52,
            vec![
                Value::Integer(id.raw() as i64),
                Value::Integer(source.raw() as i64),
                Value::Integer(target.raw() as i64),
                budget.string(attr)?,
                Value::Map(props),
                element_id,
                budget.string(world.format_id(source))?,
                budget.string(world.format_id(target))?,
            ],
        ))
    } else {
        let readable = world.format_id(id);
        let node_type = readable
            .rsplit_once('#')
            .map_or(readable.as_str(), |(name, _)| name);
        let mut labels = vec![budget.string(node_type.into())?];
        if let Some(value) = props.get("label") {
            let values = match value {
                Value::List(values) => values.as_slice(),
                value => std::slice::from_ref(value),
            };
            for value in values {
                if let Value::String(label) = value {
                    labels.push(budget.string(label.clone())?);
                }
            }
        }
        Ok(Value::Structure(
            0x4e,
            vec![
                Value::Integer(id.raw() as i64),
                Value::List(labels),
                Value::Map(props),
                element_id,
            ],
        ))
    }
}

/// Bolt 5 Path: deduplicated graph values and signed stored-direction indices.
fn path_value(
    path: &kala_query::PathValue,
    view: &Graph<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Value, Failure> {
    let mut node_indices = BTreeMap::new();
    let mut nodes = Vec::new();
    for id in path.nodes() {
        budget.step(meter)?;
        if !node_indices.contains_key(id) {
            // A ref may target an edge, but a Bolt Path's node list holds
            // only Nodes: drivers would hydrate a Relationship there as a node.
            if id.as_edge().is_some() {
                return Err(Failure::new(
                    EXECUTION,
                    "a path through an edge entity cannot be returned as a Path; return its nodes and relationships separately",
                ));
            }
            budget.charge(128)?;
            node_indices.insert(*id, nodes.len() as i64);
            nodes.push(entity_value(*id, view, tick, meter, budget)?);
        }
    }
    let mut rel_indices = BTreeMap::new();
    let mut rels = Vec::new();
    let mut indices = Vec::new();
    for (i, id) in path.edges().iter().enumerate() {
        budget.step(meter)?;
        let (source, kind, target) = view.edge(*id).ok_or_else(|| {
            Failure::new(EXECUTION, "path relationship is not visible at this canon")
        })?;
        let from = path.nodes()[i];
        let to = path.nodes()[i + 1];
        let forward = path.forward()[i];
        if (forward && (from != source || to != target))
            || (!forward && (from != target || to != source))
        {
            return Err(Failure::new(
                EXECUTION,
                "path direction does not match stored endpoints",
            ));
        }
        let index = if let Some(index) = rel_indices.get(id) {
            *index
        } else {
            budget.charge(1024)?;
            let index = rels.len() as i64 + 1;
            rel_indices.insert(*id, index);
            let entity: EntityId = (*id).into();
            rels.push(Value::Structure(
                0x72,
                vec![
                    Value::Integer(entity.raw() as i64),
                    budget.string(kind)?,
                    Value::Map(properties(entity, view, tick, meter, budget)?),
                    budget.string(view.format_id(entity))?,
                ],
            ));
            index
        };
        budget.charge(128)?;
        indices.push(Value::Integer(if forward { index } else { -index }));
        indices.push(Value::Integer(node_indices[&to]));
    }
    Ok(Value::Structure(
        0x50,
        vec![Value::List(nodes), Value::List(rels), Value::List(indices)],
    ))
}

fn visit_ids(datum: &Datum, visit: &mut impl FnMut(EntityId)) {
    match datum {
        Datum::Value(CoreValue::Ref(id)) => visit(*id),
        Datum::Path(path) => {
            for id in path.nodes() {
                visit(*id);
            }
            for id in path.edges() {
                visit((*id).into());
            }
        }
        Datum::List(values) => {
            for value in values {
                visit_ids(value, visit);
            }
        }
        _ => {}
    }
}

/// Runs `run` under a deadline: it gets the deadline to hand to whatever
/// can stop early (the executor checks it where it checks the cancel flag,
/// see `kala_query::RunOptions::with_deadline`), and a result that arrives
/// after the deadline is the timeout failure whatever it was. No thread
/// watches the clock.
pub(crate) fn until<T>(
    deadline: Instant,
    run: impl FnOnce(Instant) -> Result<T, Failure>,
) -> Result<T, Failure> {
    let result = run(deadline);
    if Instant::now() >= deadline {
        Err(crate::write::timeout())
    } else {
        result
    }
}
