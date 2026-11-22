//! The planner: orders a query's resolved patterns greedily and picks the
//! index each one reads.
//!
//! Repeatedly, among the patterns still to place, take the one with the best
//! access given the variables bound so far: a bound entity reads `E`; else a
//! bound `ref` value reads `R`; else a bound value of an indexed attribute
//! reads `V`; a `:handle` pattern with either side bound reads the handle
//! table; else (nothing bound) a full scan, the attribute with the fewest
//! stored pairs first. Ties go to query order, so plans are deterministic.
//! Patterns chain as nested `Expand`s over `Unit` (left-deep); a filter goes
//! right after the step that binds its last variable; `Project` is on top.

use std::collections::BTreeSet;

use kala_core::{ValueType, World};
use kala_query::{Access, AttrRef, Op, Predicate, Term, TickSource, Var};

use crate::error::{DatalogError, ErrorKind, Span, show};

/// What the planner needs to know about a pattern's attribute.
pub(crate) enum PatAttr {
    /// The `:handle` pseudo-attribute.
    Handle,
    /// A schema attribute (or inverse name).
    Named {
        name: String,
        /// The type of the values read under this name (`Ref` for an inverse).
        value_type: ValueType,
        indexed: bool,
    },
}

/// A resolved pattern.
pub(crate) struct Pat {
    pub entity: Term,
    pub attr: AttrRef,
    pub info: PatAttr,
    pub value: Term,
    pub tick: TickSource,
    /// The tick variable of an `(at ?t ..)` clause: the pattern can only be
    /// placed once it is bound.
    pub tick_var: Option<(String, Span)>,
    pub span: Span,
}

/// A resolved predicate and the variables it reads.
pub(crate) struct Filt {
    pub pred: Predicate,
    pub vars: Vec<String>,
}

fn is_bound(t: &Term, bound: &BTreeSet<String>) -> bool {
    match t {
        Term::Const(_) | Term::Param(_) => true,
        Term::Var(v) => bound.contains(&v.0),
        Term::Any => false,
    }
}

/// The access `p` would read through now, best first by rank, or `None` if
/// it can't be placed yet.
fn access(p: &Pat, bound: &BTreeSet<String>) -> Option<(u8, Access)> {
    if let Some((t, _)) = &p.tick_var
        && !bound.contains(t)
    {
        return None;
    }
    let e = is_bound(&p.entity, bound);
    let v = is_bound(&p.value, bound);
    match &p.info {
        PatAttr::Handle => (e || v).then_some((3, Access::Handle)),
        PatAttr::Named {
            value_type,
            indexed,
            ..
        } => Some(if e {
            (0, Access::E)
        } else if v && *value_type == ValueType::Ref {
            (1, Access::R)
        } else if v && *indexed {
            (2, Access::V)
        } else {
            (4, Access::Scan)
        }),
    }
}

/// Plans `pats` and `filts` (both in query order) into an operator tree
/// projecting `find`.
pub(crate) fn plan(
    world: &World,
    pats: Vec<Pat>,
    filts: Vec<Filt>,
    find: &[Var],
) -> Result<Op, DatalogError> {
    // Scan sizes, looked up once per pattern.
    let mut pairs = Vec::with_capacity(pats.len());
    for p in &pats {
        pairs.push(match &p.info {
            PatAttr::Handle => 0,
            PatAttr::Named { name, .. } => {
                world
                    .attr_stats(name)
                    .map_err(|e| {
                        DatalogError::new(
                            ErrorKind::UnknownAttribute,
                            Some(p.span),
                            format!("attribute `:{}`: {e}", show(name)),
                        )
                    })?
                    .pairs
            }
        });
    }

    let mut bound = BTreeSet::new();
    let mut placed_filts = vec![false; filts.len()];
    let mut op = place_filters(Op::Unit, &filts, &mut placed_filts, &bound);
    let mut left: Vec<usize> = (0..pats.len()).collect();
    while !left.is_empty() {
        // (rank, scan size, position in `left`) of the best candidate.
        let mut best: Option<(u8, usize, usize, Access)> = None;
        for (pos, &i) in left.iter().enumerate() {
            let Some((rank, acc)) = access(&pats[i], &bound) else {
                continue;
            };
            let size = if acc == Access::Scan { pairs[i] } else { 0 };
            if best.is_none_or(|(r, s, _, _)| (rank, size) < (r, s)) {
                best = Some((rank, size, pos, acc));
            }
        }
        let Some((_, _, pos, acc)) = best else {
            return Err(stuck(&pats[left[0]], &bound));
        };
        let i = left.remove(pos);
        let p = &pats[i];
        for t in [&p.entity, &p.value] {
            if let Term::Var(v) = t {
                bound.insert(v.0.clone());
            }
        }
        op = Op::Expand {
            input: Box::new(op),
            entity: p.entity.clone(),
            attr: p.attr.clone(),
            value: p.value.clone(),
            tick: p.tick.clone(),
            access: acc,
        };
        op = place_filters(op, &filts, &mut placed_filts, &bound);
    }
    Ok(Op::Project {
        input: Box::new(op),
        vars: find.to_vec(),
    })
}

/// Wraps `op` in every not-yet-placed filter whose variables are all bound,
/// in query order.
fn place_filters(mut op: Op, filts: &[Filt], placed: &mut [bool], bound: &BTreeSet<String>) -> Op {
    for (f, done) in filts.iter().zip(placed.iter_mut()) {
        if !*done && f.vars.iter().all(|v| bound.contains(v)) {
            *done = true;
            op = Op::Filter {
                input: Box::new(op),
                pred: f.pred.clone(),
            };
        }
    }
    op
}

/// Why no remaining pattern can be placed, reported on the first of them.
fn stuck(p: &Pat, bound: &BTreeSet<String>) -> DatalogError {
    if let Some((t, span)) = &p.tick_var
        && !bound.contains(t)
    {
        // The compiler checked a pattern binds ?t, so it is one that
        // itself needs ?t (or something only such a pattern binds).
        return DatalogError::new(
            ErrorKind::UnboundVariable,
            Some(*span),
            format!(
                "tick variable ?{} is only bound by patterns that read at it",
                show(t)
            ),
        );
    }
    DatalogError::new(
        ErrorKind::Unsupported,
        Some(p.span),
        "`:handle` needs the entity or the handle bound",
    )
}
