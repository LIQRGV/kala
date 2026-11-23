//! Language dispatch and bounded result enrichment at the pinned tick and canon.
use crate::error::{EXECUTION, FORBIDDEN, Failure, MISSING, TYPE};
use crate::message::Map;
use crate::packstream::{self, Value};
use kala_core::{
    Cardinality, EntityId, MeteredView, Tick, Value as CoreValue, ValueType, WorkMeter, World,
};
use kala_datalog::ParamKind;
use kala_query::{Datum, Limits, QueryError, QueryParams};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::AtomicBool;

pub(crate) struct Output {
    pub fields: Vec<String>,
    pub rows: VecDeque<Vec<u8>>,
}
pub(crate) fn run(
    world: &World,
    text: &str,
    given: &Map,
    stop: &AtomicBool,
    max_message: usize,
    max_depth: usize,
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
    eprintln!(
        "event=query_prepared language={} columns={} parameters={}",
        front_end.name(),
        prepared.plan.find.len(),
        prepared.params.len()
    );
    let params = parameters(world, &prepared.params, given, front_end.nullable_params())?;
    let limits = Limits::default();
    let result =
        kala_query::run_with_values_and_cancel(world, &prepared.plan, &params, &limits, stop)
            .map_err(query_error)?;
    let meter = WorkMeter::bounded(limits.max_steps, Some(stop));
    let view = world.at_canon(result.canon).metered(&meter);
    let mut budget = Budget {
        bytes: 0,
        steps: 0,
        limits,
        system,
    };
    let mut rows = VecDeque::new();
    for row in result.rows {
        budget.step(&meter)?;
        let mut values = Vec::new();
        for datum in row {
            values.push(datum_value(datum, &view, result.tick, &meter, &mut budget)?);
        }
        let record = Value::Structure(0x71, vec![Value::List(values)]);
        let bytes =
            packstream::encode_with_limits(&record, max_message, max_depth).map_err(|_| {
                Failure::new(EXECUTION, "result message size or nesting limit exceeded")
            })?;
        budget.charge(bytes.len())?;
        rows.push_back(bytes);
    }
    Ok(Output {
        fields: result.columns.into_iter().map(|v| v.0).collect(),
        rows,
    })
}
/// Generic on purpose: says what is refused, nothing about stored values.
pub(crate) fn protected() -> Failure {
    Failure::new(
        FORBIDDEN,
        "queries on the system database may not read the protected attribute password_hash",
    )
}
fn parameters(
    world: &World,
    kinds: &[(String, ParamKind)],
    given: &Map,
    nullable: bool,
) -> Result<QueryParams, Failure> {
    let mut params = QueryParams::new();
    for (name, kind) in kinds {
        let raw = given
            .get(name)
            .ok_or_else(|| Failure::new(MISSING, format!("missing parameter {name}")))?;
        params.insert(
            name.clone(),
            parameter_value(world, kind, raw, nullable, 0)?,
        );
    }
    Ok(params)
}
/// The largest magnitude an `i64` can have and still convert to `f64`
/// exactly (2^53).
const F64_EXACT_INT_MAX: u64 = 1 << 53;

fn parameter_value(
    world: &World,
    kind: &ParamKind,
    raw: &Value,
    nullable: bool,
    depth: usize,
) -> Result<Datum, Failure> {
    let bad = || Failure::new(TYPE, "wrong type for query parameter");
    if depth > 32 {
        return Err(bad());
    }
    if nullable && !matches!(kind, ParamKind::Tick | ParamKind::Canon) {
        match raw {
            Value::Null => return Ok(Datum::Null),
            Value::List(values) => {
                return values
                    .iter()
                    .map(|v| parameter_value(world, kind, v, true, depth + 1))
                    .collect::<Result<Vec<_>, _>>()
                    .map(Datum::List);
            }
            _ => {}
        }
    }
    let value = match (kind, raw) {
        (ParamKind::Scalar, Value::Bool(v)) => CoreValue::Bool(*v),
        (ParamKind::Scalar, Value::Integer(v)) => CoreValue::I64(*v),
        (ParamKind::Scalar, Value::Float(v)) => CoreValue::f64(*v).map_err(|_| bad())?,
        (ParamKind::Scalar, Value::String(v)) => CoreValue::Str(v.clone()),
        (ParamKind::Tick | ParamKind::Value(ValueType::I64), Value::Integer(n)) => {
            CoreValue::I64(*n)
        }
        (ParamKind::Canon, Value::Integer(n)) if *n >= 0 => CoreValue::I64(*n),
        (ParamKind::Canon, Value::String(s)) => CoreValue::I64(
            s.strip_prefix("tx#")
                .unwrap_or(s)
                .parse::<i64>()
                .ok()
                .filter(|n| *n >= 0)
                .ok_or_else(bad)?,
        ),
        (ParamKind::Value(ValueType::F64), Value::Float(n)) => {
            CoreValue::f64(*n).map_err(|_| bad())?
        }
        // Drivers send integral numbers as Integer. Accept one for a float
        // parameter only when the conversion is exact (|n| <= 2^53), so a
        // parameter never silently becomes a different number.
        (ParamKind::Value(ValueType::F64), Value::Integer(n))
            if n.unsigned_abs() <= F64_EXACT_INT_MAX =>
        {
            #[allow(clippy::cast_precision_loss)] // exact: |n| <= 2^53
            CoreValue::f64(*n as f64).map_err(|_| bad())?
        }
        (ParamKind::Value(ValueType::Bool), Value::Bool(v)) => CoreValue::Bool(*v),
        (ParamKind::Value(ValueType::Str), Value::String(v)) => CoreValue::Str(v.clone()),
        (ParamKind::Entity | ParamKind::Value(ValueType::Ref), Value::String(s)) => {
            let id = world
                .entity_by_handle(s)
                .or_else(|| world.parse_id(s))
                .filter(|id| world.is_defined(*id))
                .ok_or_else(bad)?;
            CoreValue::Ref(id)
        }
        _ => return Err(bad()),
    };
    Ok(Datum::Value(value))
}
fn query_error(error: QueryError) -> Failure {
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
    view: &MeteredView<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Value, Failure> {
    budget.charge(64)?;
    match d {
        Datum::Null => Ok(Value::Null),
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
    view: &MeteredView<'_>,
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
        view.visit_non_ref_values(id, &attr.name, tick, |value| {
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
    if let Some(handle) = view.world().handle_of(id) {
        props.insert("kala_handle".into(), budget.string(handle.into())?);
    }
    Ok(props)
}
fn entity_value(
    id: EntityId,
    view: &MeteredView<'_>,
    tick: Tick,
    meter: &WorkMeter<'_>,
    budget: &mut Budget,
) -> Result<Value, Failure> {
    budget.step(meter)?;
    budget.charge(1024)?;
    let props = properties(id, view, tick, meter, budget)?;
    let world = view.world();
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
