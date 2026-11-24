//! Static binding checks for nullable row operations.
use super::*;
use crate::{Expr, RowAction};
impl Compiler<'_, '_> {
    pub(super) fn op_row(
        &mut self,
        action: &RowAction,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let mut bound = pop(&mut self.stack);
        let action = match action {
            RowAction::Source {
                source,
                columns,
                tick,
                ordinal,
            } => {
                let columns: Vec<_> = columns.iter().map(|v| self.slots.slot(v)).collect();
                let ordinal = self.slots.slot(ordinal);
                bound.extend(columns.iter().copied().chain([ordinal]));
                if matches!(source, crate::RowSource::Server(_)) {
                    let rows = self.row_expr(
                        &Expr::Param(crate::SERVER_ROWS_PARAMETER.into()),
                        &bound,
                        0,
                    )?;
                    RowAction::Values {
                        rows,
                        columns,
                        ordinal,
                    }
                } else {
                    RowAction::Source {
                        source: source.clone(),
                        columns,
                        tick: self.row_tick(tick)?,
                        ordinal,
                    }
                }
            }
            RowAction::Values {
                rows,
                columns,
                ordinal,
            } => {
                let rows = self.row_expr(rows, &bound, 0)?;
                let columns: Vec<_> = columns.iter().map(|v| self.slots.slot(v)).collect();
                let ordinal = self.slots.slot(ordinal);
                bound.extend(columns.iter().copied().chain([ordinal]));
                RowAction::Values {
                    rows,
                    columns,
                    ordinal,
                }
            }
            RowAction::CheckParameter { name, expected } => {
                let value = self.row_expr(&Expr::Param(name.clone()), &bound, 0)?;
                if let Expr::Value(v) = value {
                    self.check_parameter_value(&v, *expected)?;
                }
                RowAction::CheckParameter {
                    name: name.clone(),
                    expected: *expected,
                }
            }
            RowAction::Trail {
                source,
                target,
                path,
                attr,
                direction,
                tick,
                min,
                max,
                excluded,
                properties,
            } => {
                if min > max || *max > crate::MAX_VAR_LENGTH {
                    return Err(QueryError::Plan(
                        "variable-length relationships need an upper bound of at most 10".into(),
                    ));
                }
                let source = input_slot(self.slots, &bound, source, "trail start")?;
                let excluded = excluded
                    .iter()
                    .map(|v| input_slot(self.slots, &bound, v, "trail exclusion"))
                    .collect::<Result<_, _>>()?;
                let properties = properties
                    .iter()
                    .map(|(k, e)| Ok((k.clone(), self.row_expr(e, &bound, 0)?)))
                    .collect::<Result<_, QueryError>>()?;
                let target = self.slots.slot(target);
                let path = self.slots.slot(path);
                bound.extend([target, path]);
                RowAction::Trail {
                    source,
                    target,
                    path,
                    attr: attr.clone(),
                    direction: *direction,
                    tick: self.row_tick(tick)?,
                    min: *min,
                    max: *max,
                    excluded,
                    properties,
                }
            }
            RowAction::Project {
                vars,
                distinct,
                order,
                skip,
                limit,
                ordinal,
            } => {
                let vars = vars
                    .iter()
                    .map(|v| input_slot(self.slots, &bound, v, "projection"))
                    .collect::<Result<Vec<_>, _>>()?;
                let order = order
                    .iter()
                    .map(|(v, desc)| Ok((input_slot(self.slots, &bound, v, "order")?, *desc)))
                    .collect::<Result<_, QueryError>>()?;
                let skip = super::super::count_term(skip.as_ref(), self.params, 0)?;
                let limit =
                    super::super::count_term(limit.as_ref(), self.params, i64::MAX as usize)?;
                let ordinal = self.slots.slot(ordinal);
                bound = vars.iter().copied().chain([ordinal]).collect();
                RowAction::Project {
                    vars,
                    distinct: *distinct,
                    order,
                    skip: Some(Term::Const(Value::I64(skip.min(i64::MAX as usize) as i64))),
                    limit: Some(Term::Const(Value::I64(limit.min(i64::MAX as usize) as i64))),
                    ordinal,
                }
            }
            RowAction::Unwind { expr, var, ordinal } => {
                let expr = self.row_expr(expr, &bound, 0)?;
                let var = self.slots.slot(var);
                let ordinal = self.slots.slot(ordinal);
                bound.extend([var, ordinal]);
                RowAction::Unwind { expr, var, ordinal }
            }
            RowAction::Group { keys, aggregates } => {
                let keys = keys
                    .iter()
                    .map(|v| input_slot(self.slots, &bound, v, "group key"))
                    .collect::<Result<Vec<_>, _>>()?;
                let aggregates = aggregates
                    .iter()
                    .map(|a| {
                        let argument = a
                            .argument
                            .as_ref()
                            .map(|e| self.row_expr(e, &bound, 0))
                            .transpose()?;
                        Ok(crate::BagAggregate {
                            output: self.slots.slot(&a.output),
                            function: a.function,
                            distinct: a.distinct,
                            argument,
                        })
                    })
                    .collect::<Result<Vec<_>, QueryError>>()?;
                bound = keys
                    .iter()
                    .copied()
                    .chain(aggregates.iter().map(|a| a.output))
                    .collect();
                RowAction::Group { keys, aggregates }
            }
            RowAction::NodeByHandle { var, handle } => {
                let handle = match self.slots.get(handle) {
                    Some(s) if bound.contains(&s) => s,
                    _ => {
                        return Err(QueryError::Plan(format!(
                            "handle seek value ?{} is not bound by its input",
                            handle.0
                        )));
                    }
                };
                let var = self.slots.slot(var);
                bound.insert(var);
                RowAction::NodeByHandle { var, handle }
            }
            RowAction::Nodes { var, labels, tick } => {
                let tick = self.row_tick(tick)?;
                let var = self.slots.slot(var);
                bound.insert(var);
                RowAction::Nodes {
                    var,
                    labels: labels.clone(),
                    tick,
                }
            }
            RowAction::Edge {
                source,
                target,
                edge,
                attr,
                direction,
                tick,
            } => {
                let tick = self.row_tick(tick)?;
                let source = self.slots.slot(source);
                let target = self.slots.slot(target);
                let edge = self.slots.slot(edge);
                bound.extend([source, target, edge]);
                RowAction::Edge {
                    source,
                    target,
                    edge,
                    attr: attr.clone(),
                    direction: *direction,
                    tick,
                }
            }
            RowAction::Bind { var, expr } => {
                let expr = self.row_expr(expr, &bound, 0)?;
                let var = self.slots.slot(var);
                if !bound.insert(var) {
                    return Err(QueryError::Plan(
                        "expression output must be a new variable".into(),
                    ));
                }
                RowAction::Bind { var, expr }
            }
            RowAction::Select(expr) => RowAction::Select(self.row_expr(expr, &bound, 0)?),
        };
        Ok((Step::Row(action), bound))
    }
    fn row_tick(&self, tick: &TickSource) -> Result<TickSource, QueryError> {
        Ok(TickSource::Const(match tick {
            TickSource::Default => self.default_tick,
            TickSource::Const(t) => *t,
            TickSource::Param(p) => tick_param(self.params, p)?,
            TickSource::Var(_) => {
                return Err(QueryError::Plan(
                    "row read needs a constant or parameter tick".into(),
                ));
            }
        }))
    }
    fn row_expr(
        &mut self,
        e: &Expr,
        bound: &BTreeSet<usize>,
        depth: usize,
    ) -> Result<Expr<usize, Val>, QueryError> {
        if depth > 32 {
            return Err(QueryError::Plan("expression nesting limit exceeded".into()));
        }
        self.budget.step(1)?;
        self.budget
            .charge_bytes(std::mem::size_of::<Expr<usize, Val>>())?;
        Ok(match e {
            Expr::Var(v) => Expr::Var(input_slot(self.slots, bound, v, "expression")?),
            Expr::Value(v) => {
                Expr::Value(self.row_constant(crate::ParameterValue::Scalar(v), depth)?)
            }
            Expr::Null => Expr::Null,
            Expr::Function(f, e) => {
                Expr::Function(*f, Box::new(self.row_expr(e, bound, depth + 1)?))
            }
            Expr::Disjoint(a, b) => Expr::Disjoint(
                Box::new(self.row_expr(a, bound, depth + 1)?),
                Box::new(self.row_expr(b, bound, depth + 1)?),
            ),
            Expr::Path { start, segments } => Expr::Path {
                start: input_slot(self.slots, bound, start, "path start")?,
                segments: segments
                    .iter()
                    .map(|v| input_slot(self.slots, bound, v, "path segment"))
                    .collect::<Result<_, _>>()?,
            },
            Expr::List(xs) => Expr::List(
                xs.iter()
                    .map(|e| self.row_expr(e, bound, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Aggregate { .. } => {
                return Err(QueryError::Plan(
                    "aggregate must be lowered to Group".into(),
                ));
            }
            Expr::Param(p) => {
                let value = if let Some(v) = self.parameter_constants.get(p) {
                    v.clone()
                } else {
                    let binding = self
                        .params
                        .binding(p)
                        .ok_or_else(|| QueryError::MissingParam(p.clone()))?;
                    let value = self.row_constant(binding, depth)?;
                    self.budget.charge_bytes(64 + p.len())?;
                    self.parameter_constants.insert(p.clone(), value.clone());
                    value
                };
                Expr::Value(value)
            }
            Expr::Property { entity, key, tick } => Expr::Property {
                entity: input_slot(self.slots, bound, entity, "property")?,
                key: key.clone(),
                tick: self.row_tick(tick)?,
            },
            Expr::Compare(op, a, b) => Expr::Compare(
                *op,
                Box::new(self.row_expr(a, bound, depth + 1)?),
                Box::new(self.row_expr(b, bound, depth + 1)?),
            ),
            Expr::And(a, b) => Expr::And(
                Box::new(self.row_expr(a, bound, depth + 1)?),
                Box::new(self.row_expr(b, bound, depth + 1)?),
            ),
            Expr::Or(a, b) => Expr::Or(
                Box::new(self.row_expr(a, bound, depth + 1)?),
                Box::new(self.row_expr(b, bound, depth + 1)?),
            ),
            Expr::Xor(a, b) => Expr::Xor(
                Box::new(self.row_expr(a, bound, depth + 1)?),
                Box::new(self.row_expr(b, bound, depth + 1)?),
            ),
            Expr::Not(e) => Expr::Not(Box::new(self.row_expr(e, bound, depth + 1)?)),
            Expr::IsNull(e) => Expr::IsNull(Box::new(self.row_expr(e, bound, depth + 1)?)),
        })
    }
    fn check_parameter_value(
        &mut self,
        value: &Val,
        expected: ValueType,
    ) -> Result<(), QueryError> {
        self.budget.step(1)?;
        match value {
            Val::Null => Ok(()),
            Val::List(_, xs) => {
                for x in xs.iter() {
                    self.check_parameter_value(x, expected)?;
                }
                Ok(())
            }
            x if x.kind() == super::super::val::ValKind::Value(expected)
                || (expected == ValueType::F64 && matches!(x, Val::I64(_))) =>
            {
                Ok(())
            }
            _ => Err(QueryError::TypeMismatch(format!(
                "parameter list elements must be {}",
                expected.name()
            ))),
        }
    }
    fn row_constant(
        &mut self,
        value: crate::ParameterValue<'_>,
        depth: usize,
    ) -> Result<Val, QueryError> {
        use crate::{Datum, ParameterValue};
        if depth > 32 {
            return Err(QueryError::Plan("parameter nesting limit exceeded".into()));
        }
        self.budget.step(1)?;
        match value {
            ParameterValue::Scalar(v) | ParameterValue::Datum(Datum::Value(v)) => {
                if let Value::Str(s) = v {
                    self.budget.step(s.len() as u64 / 64)?;
                    // Reserve before the interner copies; release if already charged.
                    let bytes = s
                        .len()
                        .saturating_add(super::super::val::STRING_OVERHEAD_BYTES);
                    self.budget.charge_bytes(bytes)?;
                    let (v, new) = self.interner.string(s, true)?;
                    if !new {
                        self.budget.release_bytes(bytes);
                    }
                    Ok(v)
                } else {
                    self.interner.constant(v)
                }
            }
            ParameterValue::Datum(Datum::Null) => Ok(Val::Null),
            ParameterValue::Datum(Datum::List(values)) => {
                let bytes =
                    64usize.saturating_add(values.len().saturating_mul(std::mem::size_of::<Val>()));
                self.budget.charge_bytes(bytes)?;
                let mut out = Vec::with_capacity(values.len());
                for v in values {
                    out.push(self.row_constant(ParameterValue::Datum(v), depth + 1)?);
                }
                let (v, new) = self.interner.list(out, self.budget.limits.max_bytes)?;
                if !new {
                    self.budget.release_bytes(bytes);
                }
                Ok(v)
            }
            _ => Err(QueryError::TypeMismatch(
                "unsupported parameter value".into(),
            )),
        }
    }
}
