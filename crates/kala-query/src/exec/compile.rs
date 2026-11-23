//! The static pass: checks a plan and compiles it into a flat [`Program`].

use std::collections::{BTreeSet, HashMap};

use kala_core::{Tick, TxId, Value, ValueType, World};

use crate::ir::{Access, Agg, AggFn, AttrRef, Func, Op, OpTerm, Predicate, Term, TickSource, Var};

use super::agg::{agg_takes, agg_wants};
use super::program::{AggStep, Attr, Pos, Program, Slots, Step, TickPos, Variant, pop};
use super::val::{CmpWork, Interner, Val, ValKind, compare};
use super::{Params, QueryError, tick_param, type_name};

/// One item of a plan's walk, in execution order.
enum Visit<'p> {
    /// `op`'s inputs are done (for a `Union`: its end; for a `Fixpoint`:
    /// its body is done).
    Op(&'p Op),
    /// A `Union` starts.
    UnionBegin,
    /// One `Union` input is done.
    UnionAdd,
    /// A `Not`'s input is done; its body starts.
    NotBody,
    /// A `Fixpoint` starts (before its rules).
    FixBegin(&'p Op),
    /// Rule number `rule` (0-based) of relation number `rel` of the
    /// innermost open `Fixpoint` is done.
    RuleEnd { rel: usize, rule: usize },
    /// The innermost open `Fixpoint`'s rules are done; its body starts.
    FixBody,
}

/// `root`'s operators in post-order, children left to right (a
/// `Fixpoint`'s rules, in order, before its body), walked with an explicit
/// stack.
fn post_order(root: &Op) -> Vec<Visit<'_>> {
    enum Todo<'p> {
        Enter(&'p Op),
        Leave(Visit<'p>),
    }
    let mut out = Vec::new();
    let mut todo = vec![Todo::Enter(root)];
    while let Some(t) = todo.pop() {
        let op = match t {
            Todo::Leave(v) => {
                out.push(v);
                continue;
            }
            Todo::Enter(op) => op,
        };
        // Pushed in reverse: the stack pops them in execution order.
        match op {
            Op::Unit => out.push(Visit::Op(op)),
            Op::NodeScan { input, .. }
            | Op::NodeSeek { input, .. }
            | Op::Expand { input, .. }
            | Op::Filter { input, .. }
            | Op::Project { input, .. }
            | Op::History { input, .. }
            | Op::During { input, .. }
            | Op::Apply { input, .. }
            | Op::Aggregate { input, .. }
            | Op::Rel { input, .. } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(input));
            }
            // The input, then the body seeded with the input's rows.
            Op::Not { input, body } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(body));
                todo.push(Todo::Leave(Visit::NotBody));
                todo.push(Todo::Enter(input));
            }
            Op::Join { left, right }
            | Op::Difference { left, right }
            | Op::Intersect { left, right } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(right));
                todo.push(Todo::Enter(left));
            }
            Op::Union { inputs } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                for i in inputs.iter().rev() {
                    todo.push(Todo::Leave(Visit::UnionAdd));
                    todo.push(Todo::Enter(i));
                }
                todo.push(Todo::Leave(Visit::UnionBegin));
            }
            Op::Fixpoint { rels, body } => {
                todo.push(Todo::Leave(Visit::Op(op)));
                todo.push(Todo::Enter(body));
                todo.push(Todo::Leave(Visit::FixBody));
                for (ri, rel) in rels.iter().enumerate().rev() {
                    for (k, rule) in rel.rules.iter().enumerate().rev() {
                        todo.push(Todo::Leave(Visit::RuleEnd { rel: ri, rule: k }));
                        todo.push(Todo::Enter(rule));
                    }
                }
                todo.push(Todo::Leave(Visit::FixBegin(op)));
            }
        }
    }
    out
}

/// The number of operators on `root`'s longest root-to-leaf path, computed
/// without recursion. Every child counts: a `Not`'s body, a `Difference`'s
/// or `Intersect`'s right side, and a `Fixpoint`'s rules (one level below
/// the `Fixpoint`: a relation's definition is not an operator) and body.
pub(super) fn plan_depth(root: &Op) -> usize {
    let mut deepest = 0;
    let mut todo = vec![(root, 1usize)];
    let mut kids = Vec::new();
    while let Some((op, d)) = todo.pop() {
        deepest = deepest.max(d);
        kids.clear();
        op.children(&mut kids);
        todo.extend(kids.iter().map(|&k| (k, d + 1)));
    }
    deepest
}

/// Compiles `root` into a flat program. Errors come in the order a
/// recursive walk (inputs first, left to right) would find them.
pub(super) fn compile(
    root: &Op,
    world: &World,
    canon: TxId,
    params: &Params,
    default_tick: Tick,
    slots: &mut Slots,
    interner: &mut Interner,
) -> Result<Program, QueryError> {
    let walk = post_order(root);
    let mut c = Compiler {
        world,
        canon,
        params,
        default_tick,
        slots,
        interner,
        stack: Vec::new(),
        unions: Vec::new(),
        fixes: Vec::new(),
        seeds: Vec::new(),
        rels: Vec::new(),
        pc: 0,
        occs: 0,
        kinds: HashMap::new(),
    };
    let mut steps = Vec::with_capacity(walk.len());
    for v in walk {
        c.pc = steps.len();
        if let Some(step) = c.visit(v)? {
            steps.push(step);
        }
    }
    let bound = pop(&mut c.stack);
    debug_assert!(
        c.stack.is_empty() && c.unions.is_empty() && c.fixes.is_empty() && c.seeds.is_empty()
    );
    Ok(Program {
        steps,
        bound,
        rels: c.rels.into_iter().map(|r| r.cols).collect(),
    })
}

/// A relation known to the compiler.
struct RelMeta {
    name: String,
    /// The slots of its columns, in order.
    cols: Vec<usize>,
    vars: Vec<Var>,
}

/// An open `Fixpoint` during compile.
struct FixScope {
    /// Its relations' ids, in definition order.
    rels: Vec<usize>,
    /// Whether its rules (rather than its body) are being compiled: a `Rel`
    /// of one of its relations here is recursion.
    in_rules: bool,
    /// The program counter of the first step of the rule being compiled.
    rule_start: usize,
    /// The recursive reads (relation, occurrence) of that rule so far.
    pending: Vec<(usize, usize)>,
    /// The variants of the rules compiled so far, in program order.
    variants: Vec<Variant>,
}

/// Compile state: the slots bound by each pending operator's output, the
/// open `Union`s and `Fixpoint`s, and every relation seen so far.
struct Compiler<'c> {
    world: &'c World,
    canon: TxId,
    params: &'c Params,
    default_tick: Tick,
    slots: &'c mut Slots,
    /// Interns the plan's string constants (uncharged).
    interner: &'c mut Interner,
    /// The slots bound by each pending operator's output.
    stack: Vec<BTreeSet<usize>>,
    /// Per open `Union`: the slots its first input binds.
    unions: Vec<Option<BTreeSet<usize>>>,
    /// The open `Fixpoint`s, innermost last.
    fixes: Vec<FixScope>,
    /// Per open `Not` body, innermost last: the slots its input binds,
    /// which every `Unit` inside it binds too.
    seeds: Vec<BTreeSet<usize>>,
    /// Every relation, by id.
    rels: Vec<RelMeta>,
    /// The program counter of the step being compiled.
    pc: usize,
    /// Recursive reads numbered so far.
    occs: usize,
    /// The kind of value each slot is bound to wherever the plan binds it,
    /// when that is known statically (`None`: bound to different kinds, or
    /// by a `Rel`, whose columns' kinds aren't tracked). Only used to refuse
    /// an `Apply` whose argument can never be of the kind it needs.
    kinds: HashMap<usize, Option<ValKind>>,
}

/// Records that the plan binds `pos` (if it is free) to values of `kind`.
fn note_kind(kinds: &mut HashMap<usize, Option<ValKind>>, pos: &Pos, kind: Option<ValKind>) {
    if let Pos::Free(s) = pos {
        let k = kinds.entry(*s).or_insert(kind);
        if *k != kind {
            *k = None;
        }
    }
}

impl Compiler<'_> {
    /// Compiles one walk item into its step, if it has one.
    fn visit(&mut self, v: Visit<'_>) -> Result<Option<Step>, QueryError> {
        Ok(Some(match v {
            Visit::UnionBegin => {
                self.unions.push(None);
                Step::UnionBegin
            }
            Visit::UnionAdd => {
                let b = pop(&mut self.stack);
                let first = self
                    .unions
                    .last_mut()
                    .expect("invariant: UnionAdd follows its UnionBegin");
                match first {
                    Some(prev) if *prev != b => {
                        return Err(QueryError::Plan(
                            "Union inputs must bind the same variables".into(),
                        ));
                    }
                    Some(_) => {}
                    None => *first = Some(b),
                }
                Step::UnionAdd
            }
            Visit::NotBody => {
                let input = self
                    .stack
                    .last()
                    .expect("invariant: a Not's input is compiled before its body")
                    .clone();
                self.seeds.push(input);
                Step::NotBegin
            }
            Visit::FixBegin(op) => self.fix_begin(op)?,
            Visit::RuleEnd { rel, rule } => {
                let bound = pop(&mut self.stack);
                let fix = self
                    .fixes
                    .last()
                    .expect("invariant: RuleEnd is inside its Fixpoint");
                let id = fix.rels[rel];
                let meta = &self.rels[id];
                let pc = self.pc;
                for (s, v) in meta.cols.iter().zip(&meta.vars) {
                    if !bound.contains(s) {
                        return Err(QueryError::Plan(format!(
                            "Fixpoint relation `{}`: rule {} doesn't bind ?{}",
                            meta.name,
                            rule + 1,
                            v.0
                        )));
                    }
                }
                let cols = meta.cols.clone();
                let fix = self
                    .fixes
                    .last_mut()
                    .expect("invariant: RuleEnd is inside its Fixpoint");
                let start = fix.rule_start;
                fix.variants
                    .extend(
                        fix.pending
                            .drain(..)
                            .map(|(rel, occ)| Variant { start, occ, rel }),
                    );
                fix.rule_start = pc + 1;
                Step::RuleEnd { rel: id, cols }
            }
            Visit::FixBody => {
                let fix = self
                    .fixes
                    .last_mut()
                    .expect("invariant: FixBody is inside its Fixpoint");
                fix.in_rules = false;
                Step::FixLoop {
                    rels: fix.rels.clone(),
                    variants: std::mem::take(&mut fix.variants),
                }
            }
            Visit::Op(op) => {
                let (step, bound) = self.op(op)?;
                self.stack.push(bound);
                step
            }
        }))
    }

    /// A `Fixpoint` starts: checks its relations and gives each an id.
    fn fix_begin(&mut self, op: &Op) -> Result<Step, QueryError> {
        let Op::Fixpoint { rels, .. } = op else {
            unreachable!("invariant: FixBegin is only visited for a Fixpoint")
        };
        if rels.is_empty() {
            return Err(QueryError::Plan("Fixpoint defines no relations".into()));
        }
        // Its rules' `Unit`s would be seeded with the Not's rows, which has
        // no meaning for a relation; front-ends put Fixpoints outermost.
        if !self.seeds.is_empty() {
            return Err(QueryError::Plan(
                "a Fixpoint can't be inside a Not body".into(),
            ));
        }
        let mut ids = Vec::with_capacity(rels.len());
        for (i, r) in rels.iter().enumerate() {
            if rels[..i].iter().any(|o| o.name == r.name) {
                return Err(QueryError::Plan(format!(
                    "Fixpoint defines relation `{}` twice",
                    r.name
                )));
            }
            let mut cols = Vec::with_capacity(r.vars.len());
            for v in &r.vars {
                let s = self.slots.slot(v);
                if cols.contains(&s) {
                    return Err(QueryError::Plan(format!(
                        "Fixpoint relation `{}` names column ?{} twice",
                        r.name, v.0
                    )));
                }
                cols.push(s);
            }
            ids.push(self.rels.len());
            self.rels.push(RelMeta {
                name: r.name.clone(),
                cols,
                vars: r.vars.clone(),
            });
        }
        self.fixes.push(FixScope {
            rels: ids.clone(),
            in_rules: true,
            rule_start: self.pc + 1,
            pending: Vec::new(),
            variants: Vec::new(),
        });
        Ok(Step::FixBegin { rels: ids })
    }

    /// The relation a `Rel` named `name` reads: the nearest enclosing
    /// `Fixpoint`'s relation of that name, and its occurrence number if
    /// the read is recursive (inside that `Fixpoint`'s rules).
    ///
    /// A recursive read must be in the rules of the innermost open
    /// `Fixpoint`: semi-naive evaluation substitutes the delta for one read
    /// at a time, which is only sound where the read is joined into the
    /// rule's rows, not under another `Fixpoint` (whose result isn't linear
    /// in what it reads).
    fn resolve_rel(&mut self, name: &str) -> Result<(usize, Option<usize>), QueryError> {
        let innermost = self.fixes.len().wrapping_sub(1);
        for (fi, fix) in self.fixes.iter().enumerate().rev() {
            if let Some(&id) = fix.rels.iter().find(|&&id| self.rels[id].name == name) {
                if !fix.in_rules {
                    return Ok((id, None));
                }
                if fi != innermost {
                    return Err(QueryError::Plan(format!(
                        "recursive relation; Rel `{name}` is read inside a nested Fixpoint in \
                         the rules of the Fixpoint defining it"
                    )));
                }
                // Negation must read only lower strata: a relation still
                // growing can't be negated (no Fixpoint is inside a Not
                // body, so any open Not body is in this rule).
                if !self.seeds.is_empty() {
                    return Err(QueryError::Plan(format!(
                        "recursive relation; Rel `{name}` is read inside a Not body in the \
                         rules of the Fixpoint defining it (negation must read lower strata)"
                    )));
                }
                let occ = self.occs;
                self.occs += 1;
                self.fixes[fi].pending.push((id, occ));
                return Ok((id, Some(occ)));
            }
        }
        Err(QueryError::Plan(format!(
            "Rel `{name}`: no enclosing Fixpoint defines it"
        )))
    }

    /// Compiles one operator whose inputs are compiled (their bound slots on
    /// top of the stack, which this pops), returning its step and its bound
    /// slots.
    fn op(&mut self, op: &Op) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let slots = &mut *self.slots;
        let stack = &mut self.stack;
        Ok(match op {
            Op::Unit => match self.seeds.last() {
                Some(seed) => (Step::Seed, seed.clone()),
                None => (Step::Unit, BTreeSet::new()),
            },
            Op::Not { .. } => self.op_not()?,
            Op::Difference { .. } | Op::Intersect { .. } => {
                let rb = pop(stack);
                let lb = pop(stack);
                let shared: Vec<usize> = lb.intersection(&rb).copied().collect();
                let step = if matches!(op, Op::Difference { .. }) {
                    Step::Difference { shared }
                } else {
                    Step::Intersect { shared }
                };
                (step, lb)
            }
            Op::NodeScan { var, node_type, .. } => {
                let mut bound = pop(stack);
                let slot = slots.slot(var);
                let was = !bound.insert(slot);
                note_kind(
                    &mut self.kinds,
                    &Pos::Free(slot),
                    Some(ValKind::Value(ValueType::Ref)),
                );
                let step = Step::NodeScan {
                    slot,
                    bound: was,
                    node_type: node_type.clone(),
                };
                (step, bound)
            }
            Op::NodeSeek { var, entity, .. } => self.op_node_seek(var, entity)?,
            Op::Expand {
                entity,
                attr,
                value,
                tick,
                access,
                ..
            } => self.op_expand(entity, attr, value, tick, access)?,
            Op::Filter { pred, .. } => self.op_filter(pred)?,
            Op::Join { .. } => {
                let rb = pop(stack);
                let lb = pop(stack);
                let shared: Vec<usize> = lb.intersection(&rb).copied().collect();
                let bound = lb.union(&rb).copied().collect();
                (Step::Join { shared }, bound)
            }
            Op::Project { vars, .. } => self.op_project(vars)?,
            // The end of a union: its inputs were checked by `UnionAdd`.
            Op::Union { .. } => {
                let first = self
                    .unions
                    .pop()
                    .expect("invariant: a Union's end follows its UnionBegin");
                (Step::UnionEnd, first.unwrap_or_default())
            }
            // The end of a `Fixpoint`: its rows are its body's.
            Op::Fixpoint { .. } => {
                let bound = pop(stack);
                let fix = self
                    .fixes
                    .pop()
                    .expect("invariant: a Fixpoint's end follows its FixBegin");
                (Step::FixEnd { rels: fix.rels }, bound)
            }
            Op::Rel { name, args, .. } => self.op_rel(name, args)?,
            Op::History {
                entity,
                attr,
                value,
                tick,
                op,
                precision,
                ..
            } => self.op_history(entity, attr, value, tick, op, precision)?,
            Op::During {
                entity,
                attr,
                value,
                out,
                ..
            } => self.op_during(entity, attr, value, out)?,
            Op::Apply {
                func, args, out, ..
            } => self.op_apply(func, args, out)?,
            Op::Aggregate { group, aggs, .. } => self.op_aggregate(group, aggs)?,
        })
    }

    /// Compiles the end of a `Not`: its body (on top) and its input.
    fn op_not(&mut self) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let slots = &mut *self.slots;
        let stack = &mut self.stack;
        let body = pop(stack);
        let input = pop(stack);
        self.seeds.pop();
        // The body's rows are matched back to the input rows they
        // came from by every input slot, so it must keep them all.
        if let Some(s) = input.difference(&body).next() {
            let name = slots
                .0
                .iter()
                .find(|&(_, &v)| v == *s)
                .map_or("?", |(v, _)| v.0.as_str());
            return Err(QueryError::Plan(format!(
                "a Not body must keep every variable of its input, but it drops ?{name}"
            )));
        }
        let key = input.iter().copied().collect();
        Ok((Step::NotEnd { key }, input))
    }

    /// Compiles a `NodeSeek`.
    fn op_node_seek(
        &mut self,
        var: &Var,
        entity: &Term,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let params = self.params;
        let slots = &mut *self.slots;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let entity = entity_pos(entity, params, slots, &bound, "NodeSeek")?;
        if !entity.is_bound() {
            return Err(QueryError::Plan(
                "NodeSeek needs a constant, a param or a bound variable".into(),
            ));
        }
        let slot = slots.slot(var);
        let was = !bound.insert(slot);
        note_kind(
            &mut self.kinds,
            &Pos::Free(slot),
            Some(ValKind::Value(ValueType::Ref)),
        );
        let step = Step::NodeSeek {
            slot,
            bound: was,
            entity,
        };
        Ok((step, bound))
    }

    /// Compiles an `Expand` (a `:kala_handle` read included).
    fn op_expand(
        &mut self,
        entity: &Term,
        attr: &AttrRef,
        value: &Term,
        tick: &TickSource,
        access: &Access,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let (world, params, default_tick) = (self.world, self.params, self.default_tick);
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let (attr, value_type) = match attr {
            AttrRef::Handle => {
                if *access != Access::Handle {
                    return Err(QueryError::Plan(format!(
                        ":kala_handle can only be read via handle, not {access:?}"
                    )));
                }
                (None, ValueType::Str)
            }
            AttrRef::Named(name) => {
                if *access == Access::Handle {
                    return Err(QueryError::Plan(format!(
                        "handle access is only for :kala_handle, not :{name}"
                    )));
                }
                let a = resolve_attr(world, self.canon, name)?;
                let t = a.value_type;
                (Some(a), t)
            }
        };
        let e = entity_pos(entity, params, slots, &bound, "entity")?;
        let v = value_pos(value, value_type, params, interner, slots, &bound)?;
        note_kind(&mut self.kinds, &e, Some(ValKind::Value(ValueType::Ref)));
        note_kind(&mut self.kinds, &v, Some(ValKind::Value(value_type)));
        Ok(match attr {
            None => {
                if !e.is_bound() && !v.is_bound() {
                    return Err(QueryError::Plan(
                        "handle access needs the entity or the handle bound".into(),
                    ));
                }
                bind_free(&mut bound, &e, &v);
                (
                    Step::Handle {
                        entity: e,
                        value: v,
                    },
                    bound,
                )
            }
            Some(attr) => {
                check_access(*access, &attr, &e, &v)?;
                let tick = match tick {
                    TickSource::Default => TickPos::Const(default_tick),
                    TickSource::Const(t) => TickPos::Const(*t),
                    TickSource::Param(name) => TickPos::Const(tick_param(params, name)?),
                    TickSource::Var(var) => match slots.get(var) {
                        Some(s) if bound.contains(&s) => TickPos::Var(s),
                        _ => return Err(QueryError::TickNotBound(var.0.clone())),
                    },
                };
                bind_free(&mut bound, &e, &v);
                let step = Step::Expand {
                    entity: e,
                    attr,
                    value: v,
                    tick,
                    access: *access,
                };
                (step, bound)
            }
        })
    }

    /// Compiles a `Filter`.
    fn op_filter(&mut self, pred: &Predicate) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let params = self.params;
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        let bound = pop(stack);
        let left = operand_pos(&pred.left, params, interner, slots, &bound)?;
        let right = operand_pos(&pred.right, params, interner, slots, &bound)?;
        if let (Pos::Const(l), Pos::Const(r)) = (&left, &right) {
            // Constant comparisons are checked up front, like constants
            // elsewhere (the plan bounds their size: no charge).
            compare(pred.op, l, r, &mut CmpWork::default())?;
        }
        let step = Step::Filter {
            op: pred.op,
            left,
            right,
        };
        Ok((step, bound))
    }

    /// Compiles a `Project`.
    fn op_project(&mut self, vars: &[Var]) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let slots = &mut *self.slots;
        let stack = &mut self.stack;
        let bound = pop(stack);
        let mut keep = BTreeSet::new();
        for v in vars {
            match slots.get(v) {
                Some(s) if bound.contains(&s) => {
                    keep.insert(s);
                }
                _ => {
                    return Err(QueryError::Plan(format!(
                        "Project of ?{}, which its input doesn't bind",
                        v.0
                    )));
                }
            }
        }
        let step = Step::Project {
            keep: keep.iter().copied().collect(),
        };
        Ok((step, keep))
    }

    /// Compiles a `Rel` read of a `Fixpoint` relation.
    fn op_rel(&mut self, name: &str, args: &[Term]) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let params = self.params;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let (id, occ) = self.resolve_rel(name)?;
        let meta = &self.rels[id];
        if args.len() != meta.cols.len() {
            return Err(QueryError::Plan(format!(
                "Rel `{name}` has {} arguments, but its relation has {} columns",
                args.len(),
                meta.cols.len()
            )));
        }
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let mut pos = Vec::with_capacity(args.len());
        for a in args {
            pos.push(match a {
                Term::Var(v) => var_pos(v, slots, &bound),
                Term::Any => Pos::Any,
                Term::Const(c) => Pos::Const(interner.constant(c)?),
                Term::Op(o) => Pos::Const(Val::Op(*o)),
                Term::Precision(p) => Pos::Const(Val::Precision(*p)),
                Term::Param(p) => Pos::Const(interner.constant(param(params, p)?)?),
            });
        }
        for p in &pos {
            if let Pos::Free(s) = p {
                bound.insert(*s);
            }
            note_kind(&mut self.kinds, p, None);
        }
        Ok((
            Step::Rel {
                rel: id,
                args: pos,
                occ,
            },
            bound,
        ))
    }

    /// Compiles a `History` read.
    fn op_history(
        &mut self,
        entity: &Term,
        attr: &str,
        value: &Term,
        tick: &Term,
        op: &OpTerm,
        precision: &Term,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let (world, params) = (self.world, self.params);
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let attr = resolve_attr(world, self.canon, attr)?;
        let e = entity_pos(entity, params, slots, &bound, "entity")?;
        let v = value_pos(value, attr.value_type, params, interner, slots, &bound)?;
        let t = tick_term_pos(tick, params, slots, &bound)?;
        let o = match op {
            OpTerm::Any => Pos::Any,
            OpTerm::Const(o) => Pos::Const(Val::Op(*o)),
            OpTerm::Var(var) => var_pos(var, slots, &bound),
        };
        let prec = match precision {
            Term::Any => Pos::Any,
            Term::Var(v) => var_pos(v, slots, &bound),
            Term::Precision(p) => Pos::Const(Val::Precision(*p)),
            _ => {
                return Err(QueryError::TypeMismatch(
                    "History precision needs a precision, variable or wildcard".into(),
                ));
            }
        };
        for (p, k) in [
            (&prec, ValKind::Precision),
            (&e, ValKind::Value(ValueType::Ref)),
            (&v, ValKind::Value(attr.value_type)),
            (&t, ValKind::Value(ValueType::I64)),
            (&o, ValKind::Op),
        ] {
            if let Pos::Free(s) = p {
                bound.insert(*s);
            }
            note_kind(&mut self.kinds, p, Some(k));
        }
        let step = Step::History {
            entity: e,
            attr,
            value: v,
            tick: t,
            op: o,
            precision: prec,
        };
        Ok((step, bound))
    }

    /// Compiles a `During` read.
    fn op_during(
        &mut self,
        entity: &Term,
        attr: &str,
        value: &Term,
        out: &Var,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let (world, params) = (self.world, self.params);
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let attr = resolve_attr(world, self.canon, attr)?;
        let e = entity_pos(entity, params, slots, &bound, "entity")?;
        let v = value_pos(value, attr.value_type, params, interner, slots, &bound)?;
        let o = var_pos(out, slots, &bound);
        for (p, k) in [
            (&e, ValKind::Value(ValueType::Ref)),
            (&v, ValKind::Value(attr.value_type)),
            (&o, ValKind::Intervals),
        ] {
            if let Pos::Free(s) = p {
                bound.insert(*s);
            }
            note_kind(&mut self.kinds, p, Some(k));
        }
        let step = Step::During {
            entity: e,
            attr,
            value: v,
            out: o,
        };
        Ok((step, bound))
    }

    /// Compiles an `Apply`.
    fn op_apply(
        &mut self,
        func: &Func,
        args: &[Term],
        out: &Option<Var>,
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let params = self.params;
        let slots = &mut *self.slots;
        let interner = &mut *self.interner;
        let stack = &mut self.stack;
        let mut bound = pop(stack);
        let (want, has_out) = func_signature(*func);
        if args.len() != want.len() || out.is_some() != has_out {
            return Err(QueryError::Plan(format!(
                "Apply {} takes {} argument{} and {}, got {} argument{} and {}",
                func.name(),
                want.len(),
                if want.len() == 1 { "" } else { "s" },
                if has_out { "an output" } else { "no output" },
                args.len(),
                if args.len() == 1 { "" } else { "s" },
                if out.is_some() {
                    "an output"
                } else {
                    "no output"
                },
            )));
        }
        let mut pos = Vec::with_capacity(args.len());
        for (a, &kind) in args.iter().zip(want) {
            let p = apply_arg_pos(*func, a, params, interner, slots, &bound)?;
            let got = match &p {
                Pos::Const(c) => Some(c.kind()),
                Pos::Bound(s) => self.kinds.get(s).copied().flatten(),
                Pos::Free(_) | Pos::Any => None,
            };
            if let Some(got) = got
                && got != kind
            {
                if let Term::Param(name) = a {
                    return Err(QueryError::ParamType {
                        name: name.clone(),
                        expected: kind.name().into(),
                    });
                }
                let msg = format!(
                    "Apply {} needs {} arguments, but {} is {}",
                    func.name(),
                    kind.name(),
                    term_name(a),
                    got.name()
                );
                return Err(if matches!(p, Pos::Const(_)) {
                    QueryError::TypeMismatch(msg)
                } else {
                    QueryError::Plan(msg)
                });
            }
            pos.push(p);
        }
        let out = out.as_ref().map(|v| var_pos(v, slots, &bound));
        if let Some(o) = &out {
            if let Pos::Free(s) = o {
                bound.insert(*s);
            }
            note_kind(&mut self.kinds, o, Some(ValKind::Intervals));
        }
        let step = Step::Apply {
            func: *func,
            args: pos,
            out,
        };
        Ok((step, bound))
    }

    /// Compiles an `Aggregate`.
    fn op_aggregate(
        &mut self,
        group: &[Var],
        aggs: &[Agg],
    ) -> Result<(Step, BTreeSet<usize>), QueryError> {
        let slots = &mut *self.slots;
        let stack = &mut self.stack;
        let input = pop(stack);
        // Semi-naive evaluation reruns a rule on deltas and unions
        // what it derives, which is only sound for monotone rules:
        // an aggregate over part of a relation isn't.
        if self.fixes.iter().any(|f| f.in_rules) {
            return Err(QueryError::Plan(
                "an Aggregate can't be inside a Fixpoint's rules".into(),
            ));
        }
        let mut keys = Vec::with_capacity(group.len());
        for v in group {
            let s = input_slot(slots, &input, v, "group variable")?;
            if !keys.contains(&s) {
                keys.push(s);
            }
        }
        let mut bound: BTreeSet<usize> = keys.iter().copied().collect();
        let mut steps = Vec::with_capacity(aggs.len());
        for a in aggs {
            let var = input_slot(slots, &input, &a.var, "aggregated variable")?;
            let kind = self.kinds.get(&var).copied().flatten();
            if let Some(k) = kind
                && !agg_takes(a.func, k)
            {
                return Err(QueryError::TypeMismatch(format!(
                    "{} of ?{} needs {}, but ?{} is {}",
                    a.func.name(),
                    a.var.0,
                    agg_wants(a.func),
                    a.var.0,
                    k.name()
                )));
            }
            // An output is a new variable: a variable of the input
            // (a group variable included) or an earlier output would
            // leave it unclear which value the row holds.
            let out = slots.slot(&a.out);
            if input.contains(&out) || !bound.insert(out) {
                return Err(QueryError::Plan(format!(
                    "Aggregate output ?{} must be a new variable, but its input or \
                     another aggregate binds it",
                    a.out.0
                )));
            }
            let out_kind = match a.func {
                AggFn::Count | AggFn::CountDistinct => Some(ValKind::Value(ValueType::I64)),
                AggFn::Min | AggFn::Max | AggFn::Sum => kind,
            };
            note_kind(&mut self.kinds, &Pos::Free(out), out_kind);
            steps.push(AggStep {
                func: a.func,
                var,
                name: a.var.0.clone(),
                out,
            });
        }
        Ok((Step::Aggregate { keys, aggs: steps }, bound))
    }
}

/// The slot of `v`, which `input` must bind (a plan error otherwise).
fn input_slot(
    slots: &Slots,
    input: &BTreeSet<usize>,
    v: &Var,
    what: &str,
) -> Result<usize, QueryError> {
    match slots.get(v) {
        Some(s) if input.contains(&s) => Ok(s),
        _ => Err(QueryError::Plan(format!(
            "Aggregate {what} ?{} is not bound by its input",
            v.0
        ))),
    }
}

/// Adds the variables an `Expand` binds to `bound`.
fn bind_free(bound: &mut BTreeSet<usize>, e: &Pos, v: &Pos) {
    for p in [e, v] {
        if let Pos::Free(s) = p {
            bound.insert(*s);
        }
    }
}

fn resolve_attr(world: &World, canon: TxId, name: &str) -> Result<Attr, QueryError> {
    let (def, inverse) = world.at_canon(canon).resolve_attribute(name)?;
    Ok(Attr {
        name: name.into(),
        value_type: def.value_type,
        inverse,
        indexed: def.indexed,
        base: def.name.clone(),
    })
}

/// Whether `access` can serve a read with these positions bound.
fn check_access(access: Access, attr: &Attr, e: &Pos, v: &Pos) -> Result<(), QueryError> {
    let name = &attr.name;
    match access {
        Access::E if !e.is_bound() => Err(QueryError::Plan(format!(
            "E access on :{name} needs the entity bound"
        ))),
        Access::R if attr.value_type != ValueType::Ref => Err(QueryError::Plan(format!(
            "R access needs a ref attribute; :{name} isn't one"
        ))),
        Access::R if !v.is_bound() => Err(QueryError::Plan(format!(
            "R access on :{name} needs the value bound"
        ))),
        Access::V if !v.is_bound() => Err(QueryError::Plan(format!(
            "V access on :{name} needs the value bound"
        ))),
        Access::V if attr.value_type != ValueType::Ref && !attr.indexed => Err(QueryError::Plan(
            format!("V access needs an indexed attribute; :{name} is unindexed"),
        )),
        Access::Handle => Err(QueryError::Plan(format!(
            "handle access is only for :kala_handle, not :{name}"
        ))),
        _ => Ok(()),
    }
}

/// A variable term: bound by the input (a constraint) or bound here.
fn var_pos(v: &Var, slots: &mut Slots, bound: &BTreeSet<usize>) -> Pos {
    let s = slots.slot(v);
    if bound.contains(&s) {
        Pos::Bound(s)
    } else {
        Pos::Free(s)
    }
}

fn param<'p>(params: &'p Params, name: &str) -> Result<&'p Value, QueryError> {
    params
        .get(name)
        .ok_or_else(|| QueryError::MissingParam(name.to_owned()))
}

/// An entity position: constants and params must be refs.
fn entity_pos(
    t: &Term,
    params: &Params,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
    what: &str,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(Value::Ref(e)) => Pos::Const(Val::Ref(*e)),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "{what} position needs an entity, got {}",
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(format!(
                "{what} position needs an entity, got an op"
            )));
        }
        Term::Param(name) => match param(params, name)? {
            Value::Ref(e) => Pos::Const(Val::Ref(*e)),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: "ref".into(),
                });
            }
        },
    })
}

/// A value position of an attribute of type `ty`.
fn value_pos(
    t: &Term,
    ty: ValueType,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(c) if c.value_type() == ty => Pos::Const(interner.constant(c)?),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "value position needs {}, got {}",
                type_name(ty),
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(format!(
                "value position needs {}, got an op",
                type_name(ty)
            )));
        }
        Term::Param(name) => match param(params, name)? {
            c if c.value_type() == ty => Pos::Const(interner.constant(c)?),
            _ => {
                return Err(QueryError::ParamType {
                    name: name.clone(),
                    expected: type_name(ty).into(),
                });
            }
        },
    })
}

/// A `History` tick position: an `i64` constant or param, a variable or `_`.
fn tick_term_pos(
    t: &Term,
    params: &Params,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    Ok(match t {
        Term::Var(v) => var_pos(v, slots, bound),
        Term::Any => Pos::Any,
        Term::Const(Value::I64(n)) => Pos::Const(Val::I64(*n)),
        Term::Const(c) => {
            return Err(QueryError::TypeMismatch(format!(
                "History tick position needs i64, got {}",
                type_name(c.value_type())
            )));
        }
        Term::Op(_) | Term::Precision(_) => {
            return Err(QueryError::TypeMismatch(
                "History tick position needs i64, got an op".into(),
            ));
        }
        Term::Param(name) => Pos::Const(Val::I64(tick_param(params, name)?.0)),
    })
}

/// A filter operand: a constant, a param or a bound variable.
fn operand_pos(
    t: &Term,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    match t {
        Term::Const(c) => Ok(Pos::Const(interner.constant(c)?)),
        Term::Op(o) => Ok(Pos::Const(Val::Op(*o))),
        Term::Precision(p) => Ok(Pos::Const(Val::Precision(*p))),
        Term::Param(name) => Ok(Pos::Const(interner.constant(param(params, name)?)?)),
        Term::Var(v) => match slots.get(v) {
            Some(s) if bound.contains(&s) => Ok(Pos::Bound(s)),
            _ => Err(QueryError::Plan(format!(
                "Filter on ?{}, which its input doesn't bind",
                v.0
            ))),
        },
        Term::Any => Err(QueryError::Plan("Filter operand can't be _".into())),
    }
}

/// The kinds of `func`'s arguments, in order, and whether it binds an
/// output (else it is a filter).
fn func_signature(func: Func) -> (&'static [ValKind], bool) {
    const SETS: &[ValKind] = &[ValKind::Intervals, ValKind::Intervals];
    match func {
        Func::Intersect | Func::Union | Func::Difference => (SETS, true),
        Func::NotEmpty => (&[ValKind::Intervals], false),
        Func::Contains => (&[ValKind::Intervals, ValKind::Value(ValueType::I64)], false),
    }
}

/// An `Apply` argument: a constant, a param or a bound variable.
fn apply_arg_pos(
    func: Func,
    t: &Term,
    params: &Params,
    interner: &mut Interner,
    slots: &mut Slots,
    bound: &BTreeSet<usize>,
) -> Result<Pos, QueryError> {
    match t {
        Term::Var(v) => match slots.get(v) {
            Some(s) if bound.contains(&s) => Ok(Pos::Bound(s)),
            _ => Err(QueryError::Plan(format!(
                "Apply {} on ?{}, which its input doesn't bind",
                func.name(),
                v.0
            ))),
        },
        Term::Any => Err(QueryError::Plan(format!(
            "Apply {} argument can't be _",
            func.name()
        ))),
        other => operand_pos(other, params, interner, slots, bound),
    }
}

/// How a term is named in an error message.
fn term_name(t: &Term) -> String {
    match t {
        Term::Var(v) => format!("?{}", v.0),
        Term::Param(p) => format!("${p}"),
        Term::Const(_) | Term::Op(_) | Term::Precision(_) => "a constant".into(),
        Term::Any => "_".into(),
    }
}
