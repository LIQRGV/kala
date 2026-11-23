//! Predicates and interval-set function clauses, typed and resolved once
//! every scope's patterns and calls are: a parameter only predicates use
//! takes its type from what it is compared with.

use kala_core::{Value, ValueType};
use kala_query::{CmpOp, Func, Predicate, Term};

use crate::ast::{FnClause, Item, Pred};
use crate::error::{DatalogError, ErrorKind, show};
use crate::plan::{self, App, Filt};

use super::{
    BodyC, Compiler, ParamKind, Scope, Ty, int_as_f64, mismatch, op_constant, param_kind,
    param_not_a_value, precision_constant, ty_name, unbound,
};

impl Compiler<'_> {
    /// Types and resolves every scope's predicates, returning each body
    /// with its filters (in scope order). A parameter only predicates use
    /// takes the type of what it is compared with, so they are revisited
    /// (across all scopes: parameters are global) until no more types are
    /// learned.
    pub(super) fn predicates(
        &mut self,
        scopes: &[Scope],
        bodies: Vec<BodyC<'_>>,
    ) -> Result<Vec<plan::Body>, DatalogError> {
        // (scope, predicate) in scope order; within a body, its own in text
        // order, then its negations' (depth first).
        let mut preds: Vec<(&Scope, &Pred)> = Vec::new();
        let mut fns: Vec<(&Scope, &FnClause)> = Vec::new();
        for (scope, b) in scopes.iter().zip(&bodies) {
            collect_preds(scope, b, &mut preds, &mut fns);
        }
        // Function arguments depend only on the scopes' types.
        let mut apps = Vec::with_capacity(fns.len());
        for &(scope, f) in &fns {
            apps.push(self.func(f, scope)?);
        }
        // Operand checks that don't depend on other predicates come first.
        for &(scope, p) in &preds {
            for item in [&p.left, &p.right] {
                match item {
                    Item::Var(s) if !self.is_param(scope, &s.name) => {
                        if !scope.bound.contains(&s.name) {
                            return Err(unbound(
                                s.span,
                                format!(
                                    "?{} is not bound by any pattern or rule call",
                                    show(&s.name)
                                ),
                            ));
                        }
                    }
                    Item::Any(span) => {
                        return Err(DatalogError::new(
                            ErrorKind::Syntax,
                            Some(*span),
                            "`_` can't be compared; a predicate needs a variable, a parameter or a constant",
                        ));
                    }
                    _ => {}
                }
            }
        }
        let mut done = vec![false; preds.len()];
        let mut out: Vec<Option<Filt>> = (0..preds.len()).map(|_| None).collect();
        loop {
            let mut progress = false;
            for (i, &(scope, p)) in preds.iter().enumerate() {
                if done[i] {
                    continue;
                }
                if let Some(f) = self.predicate(p, scope)? {
                    out[i] = Some(f);
                    done[i] = true;
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
        if let Some(i) = done.iter().position(|d| !d) {
            let p = preds[i].1;
            let span = [&p.left, &p.right]
                .into_iter()
                .find_map(|it| match it {
                    Item::Var(s) => Some(s.span),
                    _ => None,
                })
                .unwrap_or(p.span);
            return Err(mismatch(
                span,
                "can't tell the type of a parameter that is only compared with another parameter",
            ));
        }
        // Every predicate is resolved; hand them back in collection order.
        let mut filts = out.into_iter().flatten();
        let mut apps = apps.into_iter();
        Ok(bodies
            .into_iter()
            .map(|b| planned_body(b, &mut filts, &mut apps))
            .collect())
    }

    /// One predicate, or `None` if both operands are still untyped params.
    /// Variables and params are typed first: `:assert` / `:retract`
    /// compared with an op is an op constant, not a handle.
    fn predicate(&mut self, p: &Pred, scope: &Scope) -> Result<Option<Filt>, DatalogError> {
        let lv = self.var_type(&p.left, scope);
        let rv = self.var_type(&p.right, scope);
        let lt = match lv {
            Some(t) => Some(t),
            None => self.const_type(&p.left, rv)?,
        };
        let rt = match rv {
            Some(t) => Some(t),
            None => self.const_type(&p.right, lv)?,
        };
        const F64: Ty = Ty::Val(ValueType::F64);
        const I64: Ty = Ty::Val(ValueType::I64);
        let ty = match (lt, rt) {
            (None, None) => return Ok(None),
            (Some(t), None) | (None, Some(t)) => t,
            (Some(l), Some(r)) if l == r => l,
            // An integer literal compares with an f64 as a float.
            (Some(F64), Some(I64)) if matches!(p.right, Item::Int(..)) => F64,
            (Some(I64), Some(F64)) if matches!(p.left, Item::Int(..)) => F64,
            (Some(l), Some(r)) => {
                return Err(mismatch(
                    p.span,
                    format!("can't compare {} with {}", ty_name(l), ty_name(r)),
                ));
            }
        };
        if ty == Ty::Intervals && !matches!(p.op, CmpOp::Eq | CmpOp::Ne) {
            return Err(mismatch(
                p.span,
                format!(
                    "interval sets have no order; `{}` can't compare them (use = or !=, or \
                     the interval-set functions)",
                    p.op.symbol()
                ),
            ));
        }
        let left = self.operand(&p.left, ty, scope)?;
        let right = self.operand(&p.right, ty, scope)?;
        let mut vars = Vec::new();
        for t in [&left, &right] {
            if let Term::Var(v) = t {
                vars.push(v.0.clone());
            }
        }
        Ok(Some(Filt {
            pred: Predicate {
                op: p.op,
                left,
                right,
            },
            vars,
        }))
    }

    /// The type of a variable or param operand, if known yet (`None` for a
    /// constant too).
    fn var_type(&self, item: &Item, scope: &Scope) -> Option<Ty> {
        match item {
            Item::Var(s) if self.is_param(scope, &s.name) => {
                self.types.get(&s.name).copied().map(Ty::Val)
            }
            Item::Var(s) => scope.types.get(&s.name).copied(),
            _ => None,
        }
    }

    /// The type of a constant operand compared with an operand of type
    /// `other` (`None` for a variable or param).
    fn const_type(&self, item: &Item, other: Option<Ty>) -> Result<Option<Ty>, DatalogError> {
        Ok(match item {
            Item::Var(_) | Item::Any(_) => None,
            _ if other == Some(Ty::Precision) && precision_constant(item).is_some() => {
                Some(Ty::Precision)
            }
            _ if other == Some(Ty::Op) && op_constant(item).is_some() => Some(Ty::Op),
            other => Some(Ty::Val(self.constant(other)?.0.value_type())),
        })
    }

    /// An operand as a term of type `ty` (already checked compatible).
    fn operand(&mut self, item: &Item, ty: Ty, scope: &Scope) -> Result<Term, DatalogError> {
        Ok(match item {
            Item::Var(s) if self.is_param(scope, &s.name) => match ty {
                Ty::Val(ty) => {
                    self.use_param(s, ty, param_kind(ty))?;
                    Term::Param(s.name.clone())
                }
                t => return Err(param_not_a_value(s, t)),
            },
            Item::Var(s) => Term::Var(scope.var(&s.name)),
            Item::Any(span) => {
                unreachable!("invariant: `_` operands were rejected before typing ({span:?})")
            }
            other if ty == Ty::Precision => Term::Precision(
                precision_constant(other)
                    .ok_or_else(|| mismatch(other.span(), "expected a precision keyword"))?,
            ),
            other if ty == Ty::Op => match op_constant(other) {
                Some(o) => Term::Op(o),
                None => unreachable!("invariant: an op-typed constant is :assert or :retract"),
            },
            other => match (ty, self.constant(other)?.0) {
                (Ty::Val(ValueType::F64), Value::I64(n)) => {
                    Term::Const(int_as_f64(n, other.span())?)
                }
                (_, v) => Term::Const(v),
            },
        })
    }

    /// An interval-set function clause, once every scope is typed: each
    /// argument must be bound and an interval set (`contains`'s tick an
    /// i64 variable, integer or param).
    fn func(&mut self, f: &FnClause, scope: &Scope) -> Result<App, DatalogError> {
        let name = f.func.name();
        let mut args = Vec::with_capacity(f.args.len());
        let mut vars = Vec::new();
        for (i, a) in f.args.iter().enumerate() {
            let tick = f.func == Func::Contains && i == 1;
            let want = if tick {
                Ty::Val(ValueType::I64)
            } else {
                Ty::Intervals
            };
            args.push(match a {
                Item::Any(span) => {
                    return Err(DatalogError::new(
                        ErrorKind::Syntax,
                        Some(*span),
                        format!("`_` can't be an argument of `{name}`"),
                    ));
                }
                Item::Var(s) if self.is_param(scope, &s.name) => {
                    if !tick {
                        return Err(param_not_a_value(s, want));
                    }
                    self.use_param(s, ValueType::I64, ParamKind::Tick)?;
                    Term::Param(s.name.clone())
                }
                Item::Var(s) => {
                    if !scope.bound.contains(&s.name) {
                        return Err(unbound(
                            s.span,
                            format!(
                                "?{} is not bound by any pattern, rule call or function",
                                show(&s.name)
                            ),
                        ));
                    }
                    if let Some(&t) = scope.types.get(&s.name)
                        && t != want
                    {
                        return Err(mismatch(
                            s.span,
                            format!(
                                "argument {} of `{name}` needs {}, but ?{} is {}",
                                i + 1,
                                ty_name(want),
                                show(&s.name),
                                ty_name(t)
                            ),
                        ));
                    }
                    let v = scope.var(&s.name);
                    vars.push(v.0.clone());
                    Term::Var(v)
                }
                Item::Int(n, _) if tick => Term::Const(Value::I64(*n)),
                other => {
                    return Err(mismatch(
                        other.span(),
                        format!(
                            "argument {} of `{name}` needs {}, not a constant of another type",
                            i + 1,
                            if tick {
                                "an i64 tick"
                            } else {
                                "an interval set (a variable bound by `during` or a set function)"
                            }
                        ),
                    ));
                }
            });
        }
        Ok(App {
            func: f.func,
            args,
            out: f.out.as_ref().map(|o| scope.var(&o.name)),
            vars,
            span: f.span,
        })
    }
}

/// Appends `b`'s predicates and function clauses with their scope, then
/// its negations' (depth first): the order [`planned_body`] hands them
/// back in.
fn collect_preds<'a, 'q>(
    scope: &'a Scope,
    b: &'a BodyC<'q>,
    out: &mut Vec<(&'a Scope, &'q Pred)>,
    fns: &mut Vec<(&'a Scope, &'q FnClause)>,
) {
    out.extend(b.preds.iter().map(|p| (scope, *p)));
    fns.extend(b.fns.iter().map(|f| (scope, *f)));
    for o in &b.ors {
        for (s, b) in &o.branches {
            collect_preds(s, b, out, fns);
        }
    }
    for n in &b.nots {
        collect_preds(&n.scope, &n.body, out, fns);
    }
}

/// `b` for the planner, taking its filters and function applications (and
/// its negations') from `filts` and `apps` in [`collect_preds`] order.
fn planned_body(
    b: BodyC<'_>,
    filts: &mut impl Iterator<Item = Filt>,
    apps: &mut impl Iterator<Item = App>,
) -> plan::Body {
    let own = filts.by_ref().take(b.preds.len()).collect();
    let own_apps = apps.by_ref().take(b.fns.len()).collect();
    let ors = b
        .ors
        .into_iter()
        .map(|o| plan::Disjunction {
            branches: o
                .branches
                .into_iter()
                .map(|(_, b)| planned_body(b, filts, apps))
                .collect(),
            binds: o.binds,
            uses: o.uses,
            span: o.span,
        })
        .collect();
    let nots = b
        .nots
        .into_iter()
        .map(|n| plan::Neg {
            body: planned_body(n.body, filts, apps),
            uses: n.uses,
        })
        .collect();
    plan::Body {
        pats: b.pats,
        calls: b.calls,
        filts: own,
        apps: own_apps,
        nots,
        ors,
    }
}
