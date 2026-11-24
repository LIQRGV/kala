//! The planner: orders a query's resolved patterns greedily and picks the
//! index each one reads.
//!
//! Repeatedly, among the patterns and rule calls still to place, take the one
//! with the best access given the variables bound so far, in this order: a
//! pattern with a bound entity reads `E`; a rule call with some argument
//! bound (a constant, a param or a bound variable) reads its relation
//! through a hash index on those positions; a pattern with a bound `ref`
//! value reads `R`; else a bound value of an indexed attribute reads `V`; a
//! `:kala_handle` pattern with either side bound reads the handle table; a rule
//! call with nothing bound joins its whole (already computed) relation;
//! else (nothing bound) a full scan, the attribute with the fewest stored
//! pairs first. A history pattern ranks as an `E` read with its entity
//! bound (one entity's change points) and as a scan otherwise (the
//! attribute's change points across every entity). Ties go to query order (patterns before calls), so plans
//! are deterministic. The order never changes the results, only the work.
//! Steps chain as nested `Expand`s / `Rel`s over `Unit` (left-deep); a
//! filter goes right after the step that binds its last variable, and a
//! negation (`Not`) right after the step that binds the last variable it
//! shares with the body (filters first). A negation's own body is planned
//! the same way, with those shared variables bound from the start (the
//! executor seeds its `Unit` with the rows it tests) and no `Project`.
//! `Project` is on top of a query's or rule's body.

use std::collections::BTreeSet;

use kala_core::{ValueType, World};
use kala_query::{Access, AttrRef, Func, Op, OpTerm, Predicate, Term, TickSource, Var};

use crate::error::{DatalogError, ErrorKind, Span, show};

/// What the planner needs to know about a pattern's attribute.
pub(crate) enum PatAttr {
    /// The `:kala_handle` pseudo-attribute.
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
    /// For a history pattern `[e a v t op]`: its tick and op positions (it
    /// reads every tick, so `tick` above is unused).
    pub history: Option<Hist>,
    /// For `(during [e a v] ?s)`: the variable bound to the interval set
    /// (it reads every tick, so `tick` above is unused).
    pub during: Option<Var>,
    pub span: Span,
}

/// The tick and op positions of a history pattern.
pub(crate) struct Hist {
    pub tick: Term,
    pub op: OpTerm,
    pub precision: Term,
}

/// A resolved rule call: its relation's name and one term per column.
pub(crate) struct Call {
    pub name: String,
    pub args: Vec<Term>,
}

/// A resolved predicate and the variables it reads.
pub(crate) struct Filt {
    pub pred: Predicate,
    pub vars: Vec<String>,
}

/// A resolved interval-set function clause: placed (as an `Apply`) once
/// the variables among its arguments are bound; it binds `out`.
pub(crate) struct App {
    pub func: Func,
    pub args: Vec<Term>,
    pub out: Option<Var>,
    /// The variables among `args`.
    pub vars: Vec<String>,
    pub span: Span,
}

/// A resolved body: its patterns, filters, function clauses and calls
/// (each in query order) and its negations.
#[derive(Default)]
pub(crate) struct Body {
    pub pats: Vec<Pat>,
    pub calls: Vec<Call>,
    pub filts: Vec<Filt>,
    pub apps: Vec<App>,
    pub nots: Vec<Neg>,
    pub ors: Vec<Disjunction>,
}

/// A resolved `(not ..)`.
pub(crate) struct Neg {
    pub body: Body,
    /// The plan variables it shares with the enclosing body (bound there by
    /// a positive clause): it is placed once they are all bound, and only
    /// they are bound when its body starts.
    pub uses: Vec<String>,
}

fn is_bound(t: &Term, bound: &BTreeSet<String>) -> bool {
    match t {
        Term::Const(_) | Term::Op(_) | Term::Precision(_) | Term::Param(_) => true,
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
    if p.history.is_some() || p.during.is_some() {
        // Entity bound: one entity's history (like an `E` read); else the
        // attribute's history across every entity (like a scan).
        return Some(if e {
            (RANK_E, Access::E)
        } else {
            (RANK_SCAN, Access::Scan)
        });
    }
    match &p.info {
        PatAttr::Handle => (e || v).then_some((RANK_HANDLE, Access::Handle)),
        PatAttr::Named {
            value_type,
            indexed,
            ..
        } => Some(if e {
            (RANK_E, Access::E)
        } else if v && *value_type == ValueType::Ref {
            (RANK_R, Access::R)
        } else if v && *indexed {
            (RANK_V, Access::V)
        } else {
            (RANK_SCAN, Access::Scan)
        }),
    }
}

// Candidate ranks, best first (see the module docs).
const RANK_E: u8 = 0;
const RANK_REL_BOUND: u8 = 1;
const RANK_R: u8 = 2;
const RANK_V: u8 = 3;
const RANK_HANDLE: u8 = 4;
const RANK_REL_FREE: u8 = 5;
const RANK_SCAN: u8 = 6;
/// An `or` whose input variables are bound ranks with a handle read, and
/// loses ties to whatever already has that rank.
const RANK_OR: u8 = RANK_HANDLE;

/// A rule call's rank: it can always be placed.
fn call_rank(c: &Call, bound: &BTreeSet<String>) -> u8 {
    if c.args.iter().any(|a| is_bound(a, bound)) {
        RANK_REL_BOUND
    } else {
        RANK_REL_FREE
    }
}

/// What the planner places next.
#[derive(Clone, Copy)]
enum Pick {
    Pat(usize, Access),
    Call(usize),
    Or(usize),
}

/// Plans `body` into an operator tree over `Unit`, starting with `bound`
/// bound, projecting `find` if given (a negation's body has none: its
/// rows keep every variable). Recursion only follows `not` nesting, which
/// the parser bounded.
pub(crate) fn plan(
    world: &World,
    body: Body,
    bound: BTreeSet<String>,
    find: Option<&[Var]>,
) -> Result<Op, DatalogError> {
    plan_from(world, body, bound, find, Op::Unit)
}
fn plan_from(
    world: &World,
    body: Body,
    bound: BTreeSet<String>,
    find: Option<&[Var]>,
    input: Op,
) -> Result<Op, DatalogError> {
    let mut planner = Planner::new(world, body, bound)?;
    let mut op = planner.place_ready(input)?;
    while planner.has_steps() {
        op = match planner.best()? {
            Pick::Pat(pos, access) => planner.place_pattern(op, pos, access),
            Pick::Or(i) => planner.place_or(op, i)?,
            Pick::Call(i) => planner.place_call(op, i),
        };
        op = planner.place_ready(op)?;
    }
    planner.finish(op, find)
}

/// One body being planned: what is still to place, and the variables bound
/// by what already is.
struct Planner<'w> {
    world: &'w World,
    pats: Vec<Pat>,
    /// Each pattern's scan size, looked up once.
    pairs: Vec<usize>,
    /// The patterns still to place (indices into `pats`), in query order.
    left: Vec<usize>,
    calls: Vec<Option<Call>>,
    n_calls: usize,
    ors: Vec<Option<Disjunction>>,
    n_ors: usize,
    filts: Vec<Filt>,
    placed_filts: Vec<bool>,
    apps: Vec<Option<App>>,
    nots: Vec<Option<Neg>>,
    bound: BTreeSet<String>,
}

impl<'w> Planner<'w> {
    fn new(
        world: &'w World,
        body: Body,
        bound: BTreeSet<String>,
    ) -> Result<Planner<'w>, DatalogError> {
        let Body {
            pats,
            calls,
            filts,
            apps,
            nots,
            ors,
        } = body;
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
        Ok(Planner {
            world,
            left: (0..pats.len()).collect(),
            pats,
            pairs,
            n_calls: calls.len(),
            calls: calls.into_iter().map(Some).collect(),
            n_ors: ors.len(),
            ors: ors.into_iter().map(Some).collect(),
            placed_filts: vec![false; filts.len()],
            filts,
            apps: apps.into_iter().map(Some).collect(),
            nots: nots.into_iter().map(Some).collect(),
            bound,
        })
    }

    /// Whether a pattern, rule call or `or` is still to place.
    fn has_steps(&self) -> bool {
        !self.left.is_empty() || self.n_calls > 0 || self.n_ors > 0
    }

    /// Wraps `op` in every filter, function application and negation that
    /// became ready (filters and applications first).
    fn place_ready(&mut self, op: Op) -> Result<Op, DatalogError> {
        let op = place_filters(
            op,
            &self.filts,
            &mut self.placed_filts,
            &mut self.apps,
            &mut self.bound,
        );
        place_nots(self.world, op, &mut self.nots, &self.bound)
    }

    /// The best candidate to place next (see the module docs), or why
    /// nothing left can be placed.
    fn best(&self) -> Result<Pick, DatalogError> {
        // (rank, scan size, what) of the best candidate.
        let mut best: Option<(u8, usize, Pick)> = None;
        for (pos, &i) in self.left.iter().enumerate() {
            let Some((rank, acc)) = access(&self.pats[i], &self.bound) else {
                continue;
            };
            let size = if acc == Access::Scan {
                self.pairs[i]
            } else {
                0
            };
            if best.is_none_or(|(r, s, _)| (rank, size) < (r, s)) {
                best = Some((rank, size, Pick::Pat(pos, acc)));
            }
        }
        for (i, c) in self.calls.iter().enumerate() {
            let Some(c) = c else { continue };
            let rank = call_rank(c, &self.bound);
            if best.is_none_or(|(r, s, _)| (rank, 0) < (r, s)) {
                best = Some((rank, 0, Pick::Call(i)));
            }
        }
        for (i, o) in self.ors.iter().enumerate() {
            if o.as_ref()
                .is_some_and(|o| o.uses.iter().all(|v| self.bound.contains(v)))
                && best.is_none_or(|(r, _, _)| r > RANK_OR)
            {
                best = Some((RANK_OR, 0, Pick::Or(i)));
            }
        }
        if let Some((_, _, pick)) = best {
            return Ok(pick);
        }
        if let Some(&i) = self.left.first() {
            return Err(stuck(&self.pats[i], &self.bound));
        }
        let span = self.ors.iter().flatten().next().map(|o| o.span);
        Err(DatalogError::new(
            ErrorKind::UnboundVariable,
            span,
            "or input variables are not bound",
        ))
    }

    /// Places the pattern at position `pos` of `left` over `op`, read
    /// through `access`: a `During`, a `History` or a plain `Expand`.
    fn place_pattern(&mut self, op: Op, pos: usize, access: Access) -> Op {
        let i = self.left.remove(pos);
        let p = &self.pats[i];
        let bound = &mut self.bound;
        for t in [&p.entity, &p.value] {
            if let Term::Var(v) = t {
                bound.insert(v.0.clone());
            }
        }
        match (&p.history, &p.attr) {
            (None, AttrRef::Named(name)) if p.during.is_some() => {
                let out = p
                    .during
                    .clone()
                    .expect("invariant: checked by the match guard");
                bound.insert(out.0.clone());
                Op::During {
                    input: Box::new(op),
                    entity: p.entity.clone(),
                    attr: name.clone(),
                    value: p.value.clone(),
                    out,
                }
            }
            (Some(h), AttrRef::Named(name)) => {
                if let Term::Var(v) = &h.tick {
                    bound.insert(v.0.clone());
                }
                if let OpTerm::Var(v) = &h.op {
                    bound.insert(v.0.clone());
                }
                if let Term::Var(v) = &h.precision {
                    bound.insert(v.0.clone());
                }
                Op::History {
                    input: Box::new(op),
                    entity: p.entity.clone(),
                    attr: name.clone(),
                    value: p.value.clone(),
                    tick: h.tick.clone(),
                    op: h.op.clone(),
                    precision: h.precision.clone(),
                }
            }
            (Some(_), AttrRef::Handle) => {
                unreachable!("invariant: the parser refuses `:kala_handle` history patterns")
            }
            (None, _) => Op::Expand {
                input: Box::new(op),
                entity: p.entity.clone(),
                attr: p.attr.clone(),
                value: p.value.clone(),
                tick: p.tick.clone(),
                access,
            },
        }
    }

    /// Places `or` number `i`: a `Union` of its branches, each planned over
    /// a copy of `op` with everything bound so far.
    fn place_or(&mut self, op: Op, i: usize) -> Result<Op, DatalogError> {
        let o = self.ors[i]
            .take()
            .expect("invariant: chosen an unplaced or");
        self.n_ors -= 1;
        let mut inputs = Vec::new();
        for b in o.branches {
            inputs.push(plan_from(
                self.world,
                b,
                self.bound.clone(),
                None,
                op.clone(),
            )?);
        }
        self.bound.extend(o.binds);
        Ok(Op::Union { inputs })
    }

    /// Places rule call number `i` over `op`, binding its variable arguments.
    fn place_call(&mut self, op: Op, i: usize) -> Op {
        let c = self.calls[i]
            .take()
            .expect("invariant: only unplaced calls are picked");
        self.n_calls -= 1;
        for t in &c.args {
            if let Term::Var(v) = t {
                self.bound.insert(v.0.clone());
            }
        }
        Op::Rel {
            input: Box::new(op),
            name: c.name,
            args: c.args,
        }
    }

    /// Checks nothing is left unplaced, then projects `find` if given.
    fn finish(self, op: Op, find: Option<&[Var]>) -> Result<Op, DatalogError> {
        if let Some(a) = self.apps.iter().flatten().next() {
            // The compiler checked every argument is bound, so only functions
            // whose outputs feed each other's arguments are left.
            return Err(DatalogError::new(
                ErrorKind::UnboundVariable,
                Some(a.span),
                format!(
                    "the arguments of `{}` are only bound by functions that need its output",
                    a.func.name()
                ),
            ));
        }
        assert!(
            self.nots.iter().all(Option::is_none),
            "invariant: a not's shared variables are bound by its body's positive clauses"
        );
        Ok(match find {
            Some(find) => Op::Project {
                input: Box::new(op),
                vars: find.to_vec(),
            },
            None => op,
        })
    }
}

/// Wraps `op` in a `Not` for every not-yet-placed negation whose shared
/// variables are all bound, in query order.
fn place_nots(
    world: &World,
    mut op: Op,
    nots: &mut [Option<Neg>],
    bound: &BTreeSet<String>,
) -> Result<Op, DatalogError> {
    for slot in nots.iter_mut() {
        if slot
            .as_ref()
            .is_some_and(|n| n.uses.iter().all(|v| bound.contains(v)))
        {
            let n = slot.take().expect("invariant: checked just above");
            let body = plan(world, n.body, n.uses.into_iter().collect(), None)?;
            op = Op::Not {
                input: Box::new(op),
                body: Box::new(body),
            };
        }
    }
    Ok(op)
}

/// Wraps `op` in every not-yet-placed filter and function application
/// whose variables are all bound, in query order (filters first), until
/// none is left to place: an application binds its output, which may let
/// more of them be placed.
fn place_filters(
    mut op: Op,
    filts: &[Filt],
    placed: &mut [bool],
    apps: &mut [Option<App>],
    bound: &mut BTreeSet<String>,
) -> Op {
    loop {
        for (f, done) in filts.iter().zip(placed.iter_mut()) {
            if !*done && f.vars.iter().all(|v| bound.contains(v)) {
                *done = true;
                op = Op::Filter {
                    input: Box::new(op),
                    pred: f.pred.clone(),
                };
            }
        }
        let mut progress = false;
        for slot in apps.iter_mut() {
            if slot
                .as_ref()
                .is_some_and(|a| a.vars.iter().all(|v| bound.contains(v)))
            {
                let a = slot.take().expect("invariant: checked just above");
                if let Some(o) = &a.out {
                    bound.insert(o.0.clone());
                }
                op = Op::Apply {
                    input: Box::new(op),
                    func: a.func,
                    args: a.args,
                    out: a.out,
                };
                progress = true;
            }
        }
        if !progress {
            return op;
        }
    }
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
        "`:kala_handle` needs the entity or the handle bound",
    )
}

/// Branches resolved with their shared input and output bindings.
pub(crate) struct Disjunction {
    pub branches: Vec<Body>,
    pub binds: Vec<String>,
    pub uses: Vec<String>,
    pub span: Span,
}
