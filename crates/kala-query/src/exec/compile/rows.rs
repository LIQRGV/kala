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
                let (v, new) = self.interner.list(out)?;
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
