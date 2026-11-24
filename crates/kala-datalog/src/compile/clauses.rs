//! One body's parsed clauses by kind, the variables they mention, and the
//! planning budget `or` expansion is held to.

use crate::ast::{
    Call, Clause, During, FnClause, HistoryPos, Item, Not, OpItem, Or, Pattern, Query, Rule,
    TickExpr,
};
use crate::error::{DatalogError, Span};

/// One body's clauses as parsed (`:where`, a rule definition's or a
/// `not`'s), by kind.
#[derive(Clone, Copy)]
pub(super) struct Clauses<'q> {
    pub(super) clauses: &'q [Clause],
    pub(super) calls: &'q [Call],
    pub(super) nots: &'q [Not],
    pub(super) ors: &'q [Or],
    pub(super) durings: &'q [During],
    pub(super) fns: &'q [FnClause],
}

impl<'q> Clauses<'q> {
    pub(super) fn of_query(q: &'q Query) -> Clauses<'q> {
        Clauses {
            clauses: &q.clauses,
            calls: &q.calls,
            nots: &q.nots,
            ors: &q.ors,
            durings: &q.durings,
            fns: &q.fns,
        }
    }

    pub(super) fn of_rule(r: &'q Rule) -> Clauses<'q> {
        Clauses {
            clauses: &r.clauses,
            calls: &r.calls,
            nots: &r.nots,
            ors: &r.ors,
            durings: &r.durings,
            fns: &r.fns,
        }
    }

    pub(super) fn of_not(n: &'q Not) -> Clauses<'q> {
        Clauses {
            clauses: &n.clauses,
            calls: &n.calls,
            nots: &n.nots,
            ors: &n.ors,
            durings: &n.durings,
            fns: &n.fns,
        }
    }
}

/// Every variable a body mentions (patterns, `(at ?t ..)` ticks,
/// predicates, `during` reads, function clauses, call arguments, and its
/// negations at any depth), with repeats, parameters included.
pub(super) fn body_vars<'q>(c: Clauses<'q>, out: &mut Vec<&'q str>) {
    let mut todo: Vec<Clauses<'q>> = vec![c];
    while let Some(c) = todo.pop() {
        for d in c.durings {
            pattern_vars(&d.pattern, out);
            out.push(&d.out.name);
        }
        for f in c.fns {
            for it in &f.args {
                if let Item::Var(s) = it {
                    out.push(&s.name);
                }
            }
            if let Some(o) = &f.out {
                out.push(&o.name);
            }
        }
        for clause in c.clauses {
            match clause {
                Clause::Pattern(p) => pattern_vars(p, out),
                Clause::At { tick, patterns, .. } => {
                    if let TickExpr::Var(s) = tick {
                        out.push(&s.name);
                    }
                    for p in patterns {
                        pattern_vars(p, out);
                    }
                }
                Clause::Pred(p) => {
                    for it in [&p.left, &p.right] {
                        if let Item::Var(s) = it {
                            out.push(&s.name);
                        }
                    }
                }
            }
        }
        for call in c.calls {
            for a in &call.args {
                if let Item::Var(s) = a {
                    out.push(&s.name);
                }
            }
        }
        for o in c.ors {
            for b in &o.branches {
                todo.push(Clauses::of_not(b));
            }
        }
        for n in c.nots {
            todo.push(Clauses::of_not(n));
        }
    }
}

fn pattern_vars<'p>(p: &'p Pattern, out: &mut Vec<&'p str>) {
    if let Some(HistoryPos {
        precision: Some(Item::Var(s)),
        ..
    }) = &p.history
    {
        out.push(&s.name);
    }
    let tick = p.history.as_ref().map(|h| &h.tick);
    for it in [Some(&p.e), Some(&p.v), tick].into_iter().flatten() {
        if let Item::Var(s) = it {
            out.push(&s.name);
        }
    }
    if let Some(HistoryPos {
        op: Some(OpItem::Var(s)),
        ..
    }) = &p.history
    {
        out.push(&s.name);
    }
}

// Bound the expanded prefix copies BEFORE planning/cloning. Each union branch
// carries its input bindings; successive disjunctions can multiply plan size.
pub(super) fn check_or_cost(q: &Query) -> Result<(), DatalogError> {
    fn cost(c: Clauses<'_>, span: Span) -> Result<usize, DatalogError> {
        let patterns = c
            .clauses
            .iter()
            .map(|c| match c {
                Clause::At { patterns, .. } => patterns.len(),
                _ => 1,
            })
            .sum::<usize>();
        let mut n = 1 + patterns + c.calls.len() + c.durings.len() + c.fns.len();
        // The planner may place any ready negation before the first union,
        // so every branch can copy its complete subtree along with the prefix.
        for b in c.nots {
            n = n.saturating_add(1 + cost(Clauses::of_not(b), b.span)?);
        }
        for o in c.ors {
            let mut sum = 0usize;
            for b in &o.branches {
                sum = sum.saturating_add(n.saturating_mul(cost(Clauses::of_not(b), b.span)?));
            }
            n = sum.saturating_add(1);
            if n > 4096 {
                return Err(DatalogError::limit(
                    Some(o.span),
                    "or expansion exceeds the 4096-node planning budget",
                ));
            }
        }
        if n > 4096 {
            Err(DatalogError::limit(
                Some(span),
                "or expansion exceeds the 4096-node planning budget",
            ))
        } else {
            Ok(n)
        }
    }
    let mut n = cost(Clauses::of_query(q), q.span)?;
    for r in &q.rules {
        n = n.saturating_add(cost(Clauses::of_rule(r), r.span)?);
    }
    if n > 4096 {
        return Err(DatalogError::limit(
            Some(q.span),
            "query exceeds the 4096-node planning budget",
        ));
    }
    Ok(())
}
