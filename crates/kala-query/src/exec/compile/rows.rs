//! Static binding checks for nullable row operations.
use super::*;
use crate::{Expr, RowAction};
impl Compiler<'_> {
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
        &self,
        e: &Expr,
        bound: &BTreeSet<usize>,
        depth: usize,
    ) -> Result<Expr<usize>, QueryError> {
        if depth > 32 {
            return Err(QueryError::Plan("expression nesting limit exceeded".into()));
        }
        let sub = |e| self.row_expr(e, bound, depth + 1).map(Box::new);
        Ok(match e {
            Expr::Var(v) => Expr::Var(input_slot(self.slots, bound, v, "expression")?),
            Expr::Value(v) => Expr::Value(v.clone()),
            Expr::Null => Expr::Null,
            Expr::Param(p) => Expr::Value(param(self.params, p)?.clone()),
            Expr::Property { entity, key, tick } => Expr::Property {
                entity: input_slot(self.slots, bound, entity, "property")?,
                key: key.clone(),
                tick: self.row_tick(tick)?,
            },
            Expr::Compare(op, a, b) => Expr::Compare(*op, sub(a)?, sub(b)?),
            Expr::And(a, b) => Expr::And(sub(a)?, sub(b)?),
            Expr::Or(a, b) => Expr::Or(sub(a)?, sub(b)?),
            Expr::Xor(a, b) => Expr::Xor(sub(a)?, sub(b)?),
            Expr::Not(e) => Expr::Not(sub(e)?),
            Expr::IsNull(e) => Expr::IsNull(sub(e)?),
        })
    }
}
