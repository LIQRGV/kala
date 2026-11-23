//! The compiler: resolves a parsed [`Query`] against a world's schema and
//! IDs, types every variable and parameter from its uses, and hands the
//! resolved clauses to the planner.
//!
//! Every error carries the [`Span`] of what it is about; [`crate::prepare`]
//! locates it in the query text.

use std::collections::{BTreeSet, HashMap, VecDeque};

use kala_core::{EntityId, Tick, Value, ValueType, World};
use kala_query::{
    Agg, AggFn, AttrRef, CanonSource, CmpOp, Func, Op, OpTerm, Plan, Predicate, RelDef, Term,
    TickSource, Var,
};

use crate::ast::{
    Call, CanonExpr, Clause, During, FnClause, HistoryPos, Input, Item, Not, OpItem, Or, Pattern,
    Pred, Query, Rule, Sym, TickExpr,
};
use crate::error::{DatalogError, ErrorKind, Span, show};
use crate::plan::{self, App, Filt, Hist, Pat, PatAttr};

/// What a query parameter must be given, decided from where the query uses
/// it. Tools parse parameter text by it (an entity by ID or handle, a value
/// by the attribute's type, a tick, a canon).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// An entity (or edge): used in an entity position or as a `ref` value.
    Entity,
    /// A value of this type: used as a non-`ref` value (or compared with one).
    Value(ValueType),
    /// A tick (`:at ?t`, `(at ?t ..)`): an `i64`.
    Tick,
    /// A canon (`:canon ?c`): a non-negative `i64`.
    Canon,
}

/// A query compiled and planned, with the kinds of its parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// The plan, ready for [`kala_query::run`].
    pub plan: Plan,
    /// Every `:in` parameter (name without `?`), in `:in` order, with what
    /// it must be given.
    pub params: Vec<(String, ParamKind)>,
}

/// Compiles and plans `q` against `world`; see [`crate::compile`].
pub(crate) fn compile(world: &World, q: &Query) -> Result<Prepared, DatalogError> {
    Compiler::new(world).run(q)
}

fn type_name(t: ValueType) -> &'static str {
    match t {
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::F64 => "f64",
        ValueType::Str => "string",
        ValueType::Ref => "ref",
    }
}

/// The type of a variable: a stored value's type, a change point's op (a
/// history pattern's op position) or an interval set (`during` and the set
/// functions). Parameters are always values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ty {
    Val(ValueType),
    Op,
    Precision,
    Intervals,
}

fn ty_name(t: Ty) -> &'static str {
    match t {
        Ty::Val(v) => type_name(v),
        Ty::Op => "an op",
        Ty::Precision => "a precision",
        Ty::Intervals => "an interval set",
    }
}

/// Refuses an aggregate whose variable can never be of a type it takes:
/// `min`/`max` take i64, f64 or string; `sum` i64 or f64; counts anything.
fn check_agg_type(func: AggFn, var: &Sym, ty: Option<Ty>, span: Span) -> Result<(), DatalogError> {
    let ok = match (func, ty) {
        (AggFn::Count | AggFn::CountDistinct, _) | (_, None) => true,
        (AggFn::Min | AggFn::Max, Some(t)) => {
            matches!(t, Ty::Val(ValueType::I64 | ValueType::F64 | ValueType::Str))
        }
        (AggFn::Sum, Some(t)) => matches!(t, Ty::Val(ValueType::I64 | ValueType::F64)),
    };
    match ty {
        Some(t) if !ok => Err(mismatch(
            span,
            format!(
                "`{}` takes {}, but ?{} is {}",
                func.name(),
                if func == AggFn::Sum {
                    "i64 or f64 values"
                } else {
                    "i64, f64 or string values"
                },
                show(&var.name),
                ty_name(t)
            ),
        )),
        _ => Ok(()),
    }
}

fn mismatch(span: Span, message: impl Into<String>) -> DatalogError {
    DatalogError::new(ErrorKind::TypeMismatch, Some(span), message)
}

fn unbound(span: Span, message: impl Into<String>) -> DatalogError {
    DatalogError::new(ErrorKind::UnboundVariable, Some(span), message)
}

/// A declared `:in` parameter.
struct Param {
    span: Span,
    kind: Option<ParamKind>,
    used: bool,
}

/// One variable scope: `:where`, one rule definition, or one `(not ..)`
/// (which starts as a copy of its enclosing scope, so the variables bound
/// outside are bound inside, while the ones it binds stay local to it).
/// Variables are local to their scope; parameters are global (in
/// [`Compiler`]).
#[derive(Clone, Default)]
struct Scope {
    /// The type of every variable of this scope used so far.
    types: HashMap<String, Ty>,
    /// Variables a pattern or a rule call binds.
    bound: BTreeSet<String>,
    /// Variable names as written -> as named in the plan. Only rule
    /// definitions after a rule's first rename (see [`Compiler::run`]).
    rename: HashMap<String, String>,
    /// A rule definition's head variables: local even if a parameter has
    /// the same name (the head names the rule's own arguments), so they
    /// shadow it in that definition.
    locals: BTreeSet<String>,
}

impl Scope {
    /// The plan variable for the variable written `?name` in this scope.
    fn var(&self, name: &str) -> Var {
        Var(self
            .rename
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_owned()))
    }
}

/// All definitions of one rule name.
struct RuleInfo<'q> {
    /// The definitions, in text order.
    defs: Vec<&'q Rule>,
    /// The head length every definition and call shares.
    arity: usize,
    /// The type of each head position, unified across definitions.
    types: Vec<Option<Ty>>,
    /// The rules this one's bodies call (indices into the rule list).
    callees: BTreeSet<usize>,
}

/// One body (`:where`, a definition or a `not`) resolved for the planner.
struct BodyC<'q> {
    pats: Vec<Pat>,
    calls: Vec<plan::Call>,
    /// The body's predicates, resolved once every scope is typed.
    preds: Vec<&'q Pred>,
    /// The body's interval-set function clauses, whose arguments are
    /// checked once every scope is typed (their outputs are bound and typed
    /// right away).
    fns: Vec<&'q FnClause>,
    /// The body's negations, each with its own scope.
    nots: Vec<NotC<'q>>,
    ors: Vec<OrC<'q>>,
}

/// A `(not ..)` resolved for the planner.
struct OrC<'q> {
    branches: Vec<(Scope, BodyC<'q>)>,
    binds: Vec<String>,
    uses: Vec<String>,
    span: Span,
}

struct NotC<'q> {
    /// Its scope (its predicates are typed and checked in it).
    scope: Scope,
    body: BodyC<'q>,
    /// The plan variables it shares with the enclosing body (see
    /// [`plan::Neg::uses`]).
    uses: Vec<String>,
}

struct Compiler<'w> {
    world: &'w World,
    /// Parameters by name.
    params: HashMap<String, Param>,
    /// `:in` order.
    param_order: Vec<String>,
    /// The type of every parameter used so far.
    types: HashMap<String, ValueType>,
}

impl<'w> Compiler<'w> {
    fn new(world: &'w World) -> Compiler<'w> {
        Compiler {
            world,
            params: HashMap::new(),
            param_order: Vec::new(),
            types: HashMap::new(),
        }
    }

    /// Compiles and plans `q`. Rules are checked in four passes: names and
    /// arities (grouping definitions by name; every call resolves), the
    /// call graph's components (mutually recursive rules share one), then
    /// bodies component by component in dependency order, callees first,
    /// so every call's head positions are already typed (within a
    /// recursive component, after enough passes). Every definition is
    /// checked; only rules reachable from `:where` are planned.
    fn run(mut self, q: &Query) -> Result<Prepared, DatalogError> {
        check_or_cost(q)?;
        for input in &q.inputs {
            if let Input::Var(s) = input {
                if self.params.contains_key(&s.name) {
                    return Err(DatalogError::new(
                        ErrorKind::Syntax,
                        Some(s.span),
                        format!("parameter ?{} is declared twice", show(&s.name)),
                    ));
                }
                self.params.insert(
                    s.name.clone(),
                    Param {
                        span: s.span,
                        kind: None,
                        used: false,
                    },
                );
                self.param_order.push(s.name.clone());
            }
        }

        let (mut rules, by_name) = group_rules(&q.rules)?;
        // A call inside a `not` is a dependency like any other: the rule
        // needs the callee's relation complete before it can negate it.
        for r in &q.rules {
            let caller = by_name[r.name.as_str()];
            for c in all_calls(&r.calls, &r.nots, &r.ors) {
                let callee = resolve_call(c, &rules, &by_name)?;
                rules[caller].callees.insert(callee);
            }
        }
        let mut where_callees = BTreeSet::new();
        for c in all_calls(&q.calls, &q.nots, &q.ors) {
            where_callees.insert(resolve_call(c, &rules, &by_name)?);
        }
        let comps = components(&rules);
        stratify(&q.rules, &by_name, &comps)?;

        let default_tick = match &q.at {
            None => TickSource::Default,
            Some(TickExpr::Int(n, _)) => TickSource::Const(Tick(*n)),
            Some(TickExpr::Var(s)) => {
                self.section_param(s, "`:at`", ParamKind::Tick)?;
                TickSource::Param(s.name.clone())
            }
        };
        let canon = match &q.canon {
            None => CanonSource::Latest,
            Some(CanonExpr::Int(n, _)) => CanonSource::Const(*n),
            Some(CanonExpr::Var(s)) => {
                self.section_param(s, "`:canon`", ParamKind::Canon)?;
                CanonSource::Param(s.name.clone())
            }
        };

        // Every scope (rule definitions, then `:where` last) and its body.
        let mut scopes: Vec<Scope> = Vec::new();
        let mut bodies: Vec<BodyC<'_>> = Vec::new();
        // Per rule: (the plan's head vars, scope index of each definition).
        let mut compiled: Vec<(Vec<Var>, Vec<usize>)> = vec![(Vec::new(), Vec::new()); rules.len()];
        for comp in &comps {
            // Resolve once for the initial type seeds, propagate only newly
            // known head positions, then resolve once more for the original
            // validations and final IR. Rotating a large recursive head must
            // not recompile its whole body once per position.
            let mut out = Vec::new();
            for &ri in &comp.rules {
                self.rule_defs(ri, &mut rules, &by_name, &mut out)?;
            }
            if comp.recursive {
                propagate_head_types(comp, &mut rules, &by_name);
                out.clear();
                for &ri in &comp.rules {
                    self.rule_defs(ri, &mut rules, &by_name, &mut out)?;
                }
            }
            for (ri, scope, body) in out {
                compiled[ri].1.push(scopes.len());
                scopes.push(scope);
                bodies.push(body);
            }
            for &ri in &comp.rules {
                compiled[ri].0 = rules[ri].defs[0]
                    .head
                    .iter()
                    .map(|h| Var(h.name.clone()))
                    .collect();
            }
        }

        let where_scope = scopes.len();
        scopes.push(Scope::default());
        let body = self.body(
            Clauses::of_query(q),
            &mut scopes[where_scope],
            &rules,
            &by_name,
        )?;
        bodies.push(body);

        let mut planned = self.predicates(&scopes, bodies)?;

        // The columns, and for an aggregate query its groups (the plain
        // variables) and aggregates (each distinct one once).
        let mut find = Vec::with_capacity(q.find.len());
        let mut group: Vec<Var> = Vec::new();
        let mut aggs: Vec<Agg> = Vec::new();
        for f in &q.find {
            let col = Var(f.name.clone());
            match &f.agg {
                None => {
                    self.find_var(&scopes[where_scope], &f.name, f.span)?;
                    if !group.contains(&col) {
                        group.push(col.clone());
                    }
                }
                Some(a) => {
                    let ty = self.find_var(&scopes[where_scope], &a.var.name, a.var.span)?;
                    check_agg_type(a.func, &a.var, ty, f.span)?;
                    if !aggs.iter().any(|g| g.out == col) {
                        aggs.push(Agg {
                            func: a.func,
                            var: Var(a.var.name.clone()),
                            out: col.clone(),
                        });
                    }
                }
            }
            find.push(col);
        }

        let mut params = Vec::with_capacity(self.param_order.len());
        for name in &self.param_order {
            let p = &self.params[name];
            match (&p.kind, p.used) {
                (Some(kind), true) => params.push((name.clone(), kind.clone())),
                _ => {
                    return Err(unbound(
                        p.span,
                        format!("parameter ?{} is not used by the query", show(name)),
                    ));
                }
            }
        }

        // Plan the rules `:where` reaches, each body as a conjunctive
        // query projected onto the head: one `Fixpoint` per component of
        // the call graph (all of a component's relations, so mutually
        // recursive rules are evaluated together), in dependency order.
        let reach = reachable(&rules, &where_callees);
        let mut fixes: Vec<Vec<RelDef>> = Vec::new();
        for comp in comps.iter().filter(|c| reach.contains(&c.rules[0])) {
            let mut rels = Vec::with_capacity(comp.rules.len());
            for &ri in &comp.rules {
                let (head, defs) = &compiled[ri];
                let mut bodies = Vec::with_capacity(defs.len());
                for &si in defs {
                    let b = std::mem::take(&mut planned[si]);
                    bodies.push(plan::plan(self.world, b, BTreeSet::new(), Some(head))?);
                }
                rels.push(RelDef {
                    name: rules[ri].defs[0].name.clone(),
                    vars: head.clone(),
                    rules: bodies,
                });
            }
            fixes.push(rels);
        }
        let b = planned
            .pop()
            .expect("invariant: the :where body is the last one planned");
        // An aggregate groups every binding of the body (all its positively
        // bound variables, before any projection), then the groups are
        // projected onto the columns.
        let mut root = if aggs.is_empty() {
            plan::plan(self.world, b, BTreeSet::new(), Some(&find))?
        } else {
            Op::Project {
                input: Box::new(Op::Aggregate {
                    input: Box::new(plan::plan(self.world, b, BTreeSet::new(), None)?),
                    group,
                    aggs,
                }),
                vars: find.clone(),
            }
        };
        // The lowest stratum outermost: a component's rules read only its
        // own relations and those of enclosing Fixpoints.
        for rels in fixes.into_iter().rev() {
            root = Op::Fixpoint {
                rels,
                body: Box::new(root),
            };
        }
        Ok(Prepared {
            plan: Plan {
                find,
                params: self.param_order,
                default_tick,
                canon,
                root,
            },
            params,
        })
    }

    /// Checks that `:find` may use the `:where` variable `name` (a positive
    /// clause binds it; a parameter doesn't count) and returns its type, if
    /// known.
    fn find_var(&self, scope: &Scope, name: &str, span: Span) -> Result<Option<Ty>, DatalogError> {
        if !scope.bound.contains(name) {
            let what = if self.params.contains_key(name) {
                "is a parameter; `:find` needs a variable a pattern binds"
            } else {
                "is not bound by any pattern or rule call"
            };
            return Err(unbound(span, format!("?{} {what}", show(name))));
        }
        Ok(scope.types.get(name).copied())
    }

    /// Resolves every definition of rule `ri` into `out` as (rule, scope,
    /// body), typing the rule's head positions from them. The plan names a
    /// rule's columns after its first definition's head; later definitions
    /// are renamed to match (see `renames`).
    fn rule_defs<'q>(
        &mut self,
        ri: usize,
        rules: &mut [RuleInfo<'q>],
        by_name: &HashMap<&str, usize>,
        out: &mut Vec<(usize, Scope, BodyC<'q>)>,
    ) -> Result<(), DatalogError> {
        let defs = rules[ri].defs.clone();
        let canon_head: Vec<String> = defs[0].head.iter().map(|h| h.name.clone()).collect();
        for (k, &def) in defs.iter().enumerate() {
            let mut scope = Scope {
                rename: renames(def, &canon_head, k),
                locals: def.head.iter().map(|h| h.name.clone()).collect(),
                ..Scope::default()
            };
            let body = self.body(Clauses::of_rule(def), &mut scope, rules, by_name)?;
            for (i, h) in def.head.iter().enumerate() {
                if !scope.bound.contains(&h.name) {
                    return Err(unbound(
                        h.span,
                        format!(
                            "head variable ?{} is not bound by any pattern or rule call of the body",
                            show(&h.name)
                        ),
                    ));
                }
                if let Some(&t) = scope.types.get(&h.name) {
                    match rules[ri].types[i] {
                        Some(prev) if prev != t => {
                            return Err(mismatch(
                                h.span,
                                format!(
                                    "position {} of rule `{}` is {} here but {} in another definition",
                                    i + 1,
                                    show(&def.name),
                                    ty_name(t),
                                    ty_name(prev)
                                ),
                            ));
                        }
                        Some(_) => {}
                        None => rules[ri].types[i] = Some(t),
                    }
                }
            }
            out.push((ri, scope, body));
        }
        Ok(())
    }

    /// Resolves one body's patterns, `(at ..)` clauses and rule calls into
    /// `scope` (its predicates are resolved later, with every scope's),
    /// then its negations, each in a scope of its own that starts with
    /// everything this body binds. Recursion only follows `not` nesting,
    /// which the parser bounded.
    fn body<'q>(
        &mut self,
        c: Clauses<'q>,
        scope: &mut Scope,
        rules: &[RuleInfo<'_>],
        by_name: &HashMap<&str, usize>,
    ) -> Result<BodyC<'q>, DatalogError> {
        let mut pats = Vec::new();
        let mut preds: Vec<&Pred> = Vec::new();
        let mut fns: Vec<&FnClause> = Vec::new();
        // Tick variables (not params) of `(at ?t ..)` clauses, checked once
        // every pattern is known: clause order in the text doesn't matter.
        let mut tick_vars: Vec<&Sym> = Vec::new();
        for clause in c.clauses {
            match clause {
                Clause::Pattern(p) => {
                    pats.push(self.pattern(p, TickSource::Default, None, scope)?)
                }
                Clause::At { tick, patterns, .. } => {
                    let (source, tick_var) = match tick {
                        TickExpr::Int(n, _) => (TickSource::Const(Tick(*n)), None),
                        TickExpr::Var(s) if self.is_param(scope, &s.name) => {
                            self.use_param(s, ValueType::I64, ParamKind::Tick)?;
                            (TickSource::Param(s.name.clone()), None)
                        }
                        TickExpr::Var(s) => {
                            unify_var(scope, &s.name, Ty::Val(ValueType::I64), s.span)?;
                            tick_vars.push(s);
                            let v = scope.var(&s.name);
                            (TickSource::Var(v.clone()), Some((v.0, s.span)))
                        }
                    };
                    for p in patterns {
                        pats.push(self.pattern(p, source.clone(), tick_var.clone(), scope)?);
                    }
                }
                Clause::Pred(p) => preds.push(p),
            }
        }
        for d in c.durings {
            pats.push(self.during(d, scope)?);
        }
        // A function's output is bound (and an interval set) right away;
        // its arguments are checked once every scope is typed.
        for f in c.fns {
            if let Some(out) = &f.out {
                self.interval_out(out, scope)?;
            }
            fns.push(f);
        }
        let mut out_calls = Vec::with_capacity(c.calls.len());
        for call in c.calls {
            let ri = by_name[call.name.as_str()];
            out_calls.push(self.call(call, &rules[ri], scope)?);
        }
        let mut ors = Vec::new();
        for o in c.ors {
            let mut vars = Vec::new();
            let mut branches = Vec::new();
            let inherited = scope.bound.clone();
            for b in &o.branches {
                let mut mentioned = Vec::new();
                body_vars(Clauses::of_not(b), &mut mentioned);
                let names: BTreeSet<String> = mentioned
                    .into_iter()
                    .filter(|v| !self.is_param(scope, v))
                    .map(str::to_owned)
                    .collect();
                if let Some(first) = vars.first()
                    && first != &names
                {
                    return Err(DatalogError::new(
                        ErrorKind::UnboundVariable,
                        Some(o.span),
                        "every or branch must bind the same set of variables",
                    ));
                }
                vars.push(names);
                let mut sub = scope.clone();
                let body = self.body(Clauses::of_not(b), &mut sub, rules, by_name)?;
                branches.push((sub, body));
            }
            let names = vars.into_iter().next().unwrap_or_default();
            let mut binds = BTreeSet::new();
            for (sub, _) in &branches {
                for name in &names {
                    if let Some(&ty) = sub.types.get(name) {
                        unify_var(scope, name, ty, o.span)?;
                    }
                    if sub.bound.contains(name) {
                        binds.insert(name.clone());
                    }
                }
            }
            // Mentioning a variable inside `not` does not bind it outside.
            // Reject unequal output schemas here, before emitting a Union.
            if let Some((first, _)) = branches.first()
                && branches.iter().any(|(s, _)| s.bound != first.bound)
            {
                return Err(unbound(
                    o.span,
                    "every or branch must bind the same set of variables",
                ));
            }
            let uses = names
                .intersection(&inherited)
                .map(|n| scope.var(n).0)
                .collect();
            scope.bound.extend(binds.iter().cloned());
            ors.push(OrC {
                branches,
                binds: binds.iter().map(|n| scope.var(n).0).collect(),
                uses,
                span: o.span,
            });
        }
        for s in tick_vars {
            if !scope.bound.contains(&s.name) {
                return Err(unbound(
                    s.span,
                    format!(
                        "tick variable ?{} is not bound by any pattern, rule call or `:in` parameter",
                        show(&s.name)
                    ),
                ));
            }
        }
        let mut out_nots = Vec::with_capacity(c.nots.len());
        for n in c.nots {
            out_nots.push(self.not(n, scope, rules, by_name)?);
        }
        Ok(BodyC {
            pats,
            calls: out_calls,
            preds,
            fns,
            nots: out_nots,
            ors,
        })
    }

    /// One `(not ..)` of a body whose positive clauses are resolved into
    /// `scope`: its clauses are resolved in a copy of `scope`, so what they
    /// bind stays local, while the variables `scope` binds are constraints.
    fn not<'q>(
        &mut self,
        n: &'q Not,
        scope: &mut Scope,
        rules: &[RuleInfo<'_>],
        by_name: &HashMap<&str, usize>,
    ) -> Result<NotC<'q>, DatalogError> {
        let mut sub = scope.clone();
        let body = self.body(Clauses::of_not(n), &mut sub, rules, by_name)?;
        // A variable bound outside but first typed inside (bound outside by
        // a call whose head isn't typed yet) has that type outside too.
        for (name, &ty) in &sub.types {
            if scope.bound.contains(name) {
                scope.types.entry(name.clone()).or_insert(ty);
            }
        }
        let mut mentioned = Vec::new();
        body_vars(Clauses::of_not(n), &mut mentioned);
        let uses: BTreeSet<String> = mentioned
            .into_iter()
            .filter(|v| scope.bound.contains(*v) && !self.is_param(scope, v))
            .map(|v| scope.var(v).0)
            .collect();
        Ok(NotC {
            scope: sub,
            body,
            uses: uses.into_iter().collect(),
        })
    }

    /// A rule call whose name and arity are already checked: arguments
    /// typed by the rule's head positions.
    fn call(
        &mut self,
        c: &Call,
        rule: &RuleInfo<'_>,
        scope: &mut Scope,
    ) -> Result<plan::Call, DatalogError> {
        let mut args = Vec::with_capacity(c.args.len());
        for (i, a) in c.args.iter().enumerate() {
            let ty = rule.types.get(i).copied().flatten();
            args.push(match (a, ty) {
                (Item::Var(s), ty) if self.is_param(scope, &s.name) => {
                    match ty {
                        Some(Ty::Val(ty)) => self.use_param(s, ty, param_kind(ty))?,
                        Some(t) => return Err(param_not_a_value(s, t)),
                        None => {}
                    }
                    Term::Param(s.name.clone())
                }
                (Item::Var(s), ty) => {
                    if let Some(ty) = ty {
                        unify_var(scope, &s.name, ty, s.span)?;
                    }
                    scope.bound.insert(s.name.clone());
                    Term::Var(scope.var(&s.name))
                }
                (Item::Any(_), _) => Term::Any,
                (other, None) => Term::Const(self.constant(other)?.0),
                (other, Some(Ty::Precision)) => Term::Precision(
                    precision_constant(other)
                        .ok_or_else(|| mismatch(other.span(), "expected a precision keyword"))?,
                ),
                (other, Some(Ty::Op)) => match op_constant(other) {
                    Some(o) => Term::Op(o),
                    None => {
                        return Err(mismatch(
                            other.span(),
                            format!(
                                "argument {} of `{}` is an op (`:assert` or `:retract`)",
                                i + 1,
                                show(&c.name)
                            ),
                        ));
                    }
                },
                (other, Some(Ty::Intervals)) => {
                    return Err(mismatch(
                        other.span(),
                        format!(
                            "argument {} of `{}` is an interval set, which can't be a constant",
                            i + 1,
                            show(&c.name)
                        ),
                    ));
                }
                (other, Some(Ty::Val(ty))) => {
                    let (v, got) = self.constant(other)?;
                    match (ty, v) {
                        (t, v) if v.value_type() == t => Term::Const(v),
                        (ValueType::F64, Value::I64(n)) => {
                            Term::Const(int_as_f64(n, other.span())?)
                        }
                        _ => {
                            let want = if ty == ValueType::Ref {
                                "an entity (`:handle`, `TYPE#n` or `edge#n`)".to_string()
                            } else {
                                format!("a {}", type_name(ty))
                            };
                            return Err(mismatch(
                                other.span(),
                                format!(
                                    "argument {} of `{}` needs {want}, not {got}",
                                    i + 1,
                                    show(&c.name)
                                ),
                            ));
                        }
                    }
                }
            });
        }
        Ok(plan::Call {
            name: c.name.clone(),
            args,
        })
    }

    /// Whether `?name` in `scope` is a parameter (declared in `:in` and not
    /// shadowed by a head variable of the definition).
    fn is_param(&self, scope: &Scope, name: &str) -> bool {
        self.params.contains_key(name) && !scope.locals.contains(name)
    }

    /// A parameter named in `:at` / `:canon`: it must be declared in `:in`.
    fn section_param(
        &mut self,
        s: &Sym,
        section: &str,
        kind: ParamKind,
    ) -> Result<(), DatalogError> {
        if !self.params.contains_key(&s.name) {
            return Err(unbound(
                s.span,
                format!(
                    "?{} in {section} must be a parameter declared in `:in`",
                    show(&s.name)
                ),
            ));
        }
        self.use_param(s, ValueType::I64, kind)
    }

    /// Records that param `name` has type `ty` here.
    fn unify(&mut self, name: &str, ty: ValueType, span: Span) -> Result<(), DatalogError> {
        unify_in(&mut self.types, name, ty, span)
    }

    /// Records a use of param `s` with type `ty`; its kind is the one of its
    /// first use.
    fn use_param(&mut self, s: &Sym, ty: ValueType, kind: ParamKind) -> Result<(), DatalogError> {
        self.unify(&s.name, ty, s.span)?;
        if let Some(p) = self.params.get_mut(&s.name) {
            p.used = true;
            p.kind.get_or_insert(kind);
        }
        Ok(())
    }

    fn pattern(
        &mut self,
        p: &Pattern,
        tick: TickSource,
        tick_var: Option<(String, Span)>,
        scope: &mut Scope,
    ) -> Result<Pat, DatalogError> {
        // The parser refuses both; an AST built by hand may not.
        if p.history.is_some() && (tick_var.is_some() || tick != TickSource::Default) {
            return Err(DatalogError::new(
                ErrorKind::Syntax,
                Some(p.span),
                "a history pattern can't be inside `(at ..)`: history patterns read every tick",
            ));
        }
        if p.history.is_some() && p.a.name == "handle" {
            return Err(DatalogError::new(
                ErrorKind::Syntax,
                Some(p.a.span),
                "`:handle` has no history; a history pattern needs a schema attribute",
            ));
        }
        let (attr, info, value_type) = if p.a.name == "handle" {
            (AttrRef::Handle, PatAttr::Handle, ValueType::Str)
        } else {
            let (value_type, indexed) = self.resolve_attr(&p.a.name).ok_or_else(|| {
                DatalogError::new(
                    ErrorKind::UnknownAttribute,
                    Some(p.a.span),
                    format!("unknown attribute `:{}`", show(&p.a.name)),
                )
            })?;
            let info = PatAttr::Named {
                name: p.a.name.clone(),
                value_type,
                indexed,
            };
            (AttrRef::Named(p.a.name.clone()), info, value_type)
        };
        if let (PatAttr::Handle, Item::Keyword(k, span)) = (&info, &p.v) {
            return Err(handle_keyword(&p.e, k, *span));
        }
        let entity = self.position(&p.e, ValueType::Ref, true, scope)?;
        let value = self.position(&p.v, value_type, false, scope)?;
        let history = match &p.history {
            Some(h) => Some(self.history(h, scope)?),
            None => None,
        };
        Ok(Pat {
            entity,
            attr,
            info,
            value,
            tick,
            tick_var,
            history,
            during: None,
            span: p.span,
        })
    }

    /// The tick and op positions of a history pattern. The tick is an
    /// `i64` (a parameter there is a [`ParamKind::Tick`]); an op variable
    /// binds an op (its own type, never a value's: compared with it,
    /// `:assert` / `:retract` are op constants, not handles).
    fn history(&mut self, h: &HistoryPos, scope: &mut Scope) -> Result<Hist, DatalogError> {
        let tick = match &h.tick {
            Item::Var(s) if self.is_param(scope, &s.name) => {
                self.use_param(s, ValueType::I64, ParamKind::Tick)?;
                Term::Param(s.name.clone())
            }
            Item::Var(s) => {
                unify_var(scope, &s.name, Ty::Val(ValueType::I64), s.span)?;
                scope.bound.insert(s.name.clone());
                Term::Var(scope.var(&s.name))
            }
            Item::Int(n, _) => Term::Const(Value::I64(*n)),
            Item::Any(_) => Term::Any,
            // The parser refuses these; an AST built by hand may not.
            other => {
                return Err(DatalogError::new(
                    ErrorKind::Syntax,
                    Some(other.span()),
                    "the tick position of a history pattern is a variable, an integer or `_`",
                ));
            }
        };
        let op = match &h.op {
            None | Some(OpItem::Any(_)) => OpTerm::Any,
            Some(OpItem::Assert(_)) => OpTerm::Const(kala_core::Op::Assert),
            Some(OpItem::Retract(_)) => OpTerm::Const(kala_core::Op::Retract),
            Some(OpItem::Var(s)) if self.is_param(scope, &s.name) => {
                return Err(mismatch(
                    s.span,
                    format!(
                        "?{} is a parameter; the op position of a history pattern takes a \
                         variable, `_`, `:assert` or `:retract`",
                        show(&s.name)
                    ),
                ));
            }
            Some(OpItem::Var(s)) => {
                unify_var(scope, &s.name, Ty::Op, s.span)?;
                scope.bound.insert(s.name.clone());
                OpTerm::Var(scope.var(&s.name))
            }
        };
        let precision = match &h.precision {
            None | Some(Item::Any(_)) => Term::Any,
            Some(Item::Var(s)) if self.is_param(scope, &s.name) => {
                return Err(param_not_a_value(s, Ty::Precision));
            }
            Some(Item::Var(s)) => {
                unify_var(scope, &s.name, Ty::Precision, s.span)?;
                scope.bound.insert(s.name.clone());
                Term::Var(scope.var(&s.name))
            }
            Some(i) => Term::Precision(
                precision_constant(i)
                    .ok_or_else(|| mismatch(i.span(), "expected a precision keyword"))?,
            ),
        };
        Ok(Hist {
            tick,
            op,
            precision,
        })
    }

    /// An attribute's value type and `indexed` flag; an inverse name reads
    /// `ref` values.
    fn resolve_attr(&self, name: &str) -> Option<(ValueType, bool)> {
        if let Some(def) = self.world.attribute(name) {
            return Some((def.value_type, def.indexed));
        }
        self.world
            .attributes()
            .find(|d| d.inverse.as_deref() == Some(name))
            .map(|d| (ValueType::Ref, d.indexed))
    }

    /// A pattern position whose values have type `ty` (`Ref` for the entity
    /// position).
    fn position(
        &mut self,
        item: &Item,
        ty: ValueType,
        entity_pos: bool,
        scope: &mut Scope,
    ) -> Result<Term, DatalogError> {
        Ok(match item {
            Item::Var(s) => {
                if self.is_param(scope, &s.name) {
                    self.use_param(s, ty, param_kind(ty))?;
                    Term::Param(s.name.clone())
                } else {
                    unify_var(scope, &s.name, Ty::Val(ty), s.span)?;
                    scope.bound.insert(s.name.clone());
                    Term::Var(scope.var(&s.name))
                }
            }
            Item::Any(_) => Term::Any,
            other => {
                let (v, got) = self.constant(other)?;
                match (ty, v) {
                    (t, v) if v.value_type() == t => Term::Const(v),
                    (ValueType::F64, Value::I64(n)) => Term::Const(int_as_f64(n, other.span())?),
                    _ => {
                        let want = if entity_pos {
                            "an entity (`:handle`, `TYPE#n` or `edge#n`)".to_string()
                        } else if ty == ValueType::Ref {
                            "a ref (`:handle`, `TYPE#n` or `edge#n`)".to_string()
                        } else {
                            format!("a {}", type_name(ty))
                        };
                        let pos = if entity_pos { "entity" } else { "value" };
                        return Err(mismatch(
                            other.span(),
                            format!("the {pos} position needs {want}, not {got}"),
                        ));
                    }
                }
            }
        })
    }

    /// A constant item as a value, and how to describe it in a message.
    /// Handles and IDs are resolved (unknown ones are errors).
    fn constant(&self, item: &Item) -> Result<(Value, &'static str), DatalogError> {
        Ok(match item {
            Item::Keyword(k, span) => match self.world.entity_by_handle(k) {
                Some(id) => (Value::Ref(id), "an entity"),
                None => {
                    return Err(DatalogError::new(
                        ErrorKind::UnknownEntity,
                        Some(*span),
                        format!("no entity has the handle `{}`", show(k)),
                    ));
                }
            },
            Item::Id(s, span) => match self.world.parse_id(s).filter(|&id| self.defined(id)) {
                Some(id) => (Value::Ref(id), "an entity"),
                None => {
                    return Err(DatalogError::new(
                        ErrorKind::UnknownEntity,
                        Some(*span),
                        format!("`{}` is not an entity or edge of this world", show(s)),
                    ));
                }
            },
            Item::Str(s, _) => (Value::Str(s.clone()), "a string"),
            Item::Int(n, _) => (Value::I64(*n), "an integer"),
            Item::Float(f, span) => (float(*f, *span)?, "a float"),
            Item::Bool(b, _) => (Value::Bool(*b), "a bool"),
            Item::Var(_) | Item::Any(_) => {
                unreachable!("invariant: constant() is only called on constant items")
            }
        })
    }

    /// Whether `id` (a parsed ID) is defined at the latest canon: one
    /// constant-time lookup in the world's ID tables.
    fn defined(&self, id: EntityId) -> bool {
        self.world.is_defined(id)
    }

    /// Types and resolves every scope's predicates, returning each body
    /// with its filters (in scope order). A parameter only predicates use
    /// takes the type of what it is compared with, so they are revisited
    /// (across all scopes: parameters are global) until no more types are
    /// learned.
    fn predicates(
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

    /// `(during [e a v] ?s)`: the pattern resolved as for a read (it reads
    /// every tick, at the query's canon), with `?s` an interval set.
    fn during(&mut self, d: &During, scope: &mut Scope) -> Result<Pat, DatalogError> {
        // The parser refuses both; an AST built by hand may not.
        if d.pattern.history.is_some() || d.pattern.a.name == "handle" {
            return Err(DatalogError::new(
                ErrorKind::Syntax,
                Some(d.pattern.span),
                "`during` takes a 3-element pattern over a schema attribute",
            ));
        }
        let mut pat = self.pattern(&d.pattern, TickSource::Default, None, scope)?;
        pat.during = Some(self.interval_out(&d.out, scope)?);
        Ok(pat)
    }

    /// The output variable of `during` or a set function: bound here, an
    /// interval set (a constraint if something else binds it too).
    fn interval_out(&mut self, out: &Sym, scope: &mut Scope) -> Result<Var, DatalogError> {
        if self.is_param(scope, &out.name) {
            return Err(param_not_a_value(out, Ty::Intervals));
        }
        unify_var(scope, &out.name, Ty::Intervals, out.span)?;
        scope.bound.insert(out.name.clone());
        Ok(scope.var(&out.name))
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

/// `[?p :handle :varrow]`: a handle keyword where `:handle` needs the
/// handle's text. The keyword already is the entity, so say both ways to
/// write what was probably meant (the handle and variable echoed safely).
fn handle_keyword(entity: &Item, handle: &str, span: Span) -> DatalogError {
    let e = match entity {
        Item::Var(s) => format!("?{}", show(&s.name)),
        _ => "?e".to_owned(),
    };
    let h = show(handle);
    mismatch(
        span,
        format!(
            "the value position of `:handle` needs a string, not the entity `:{h}`; \
             a handle keyword already is its entity, so use it directly \
             (as in `[?c :located_in :varrow]`), or match the handle's text: \
             `[{e} :handle \"{h}\"]`"
        ),
    )
}

fn float(f: f64, span: Span) -> Result<Value, DatalogError> {
    Value::f64(f).map_err(|e| mismatch(span, format!("not a storable float: {e}")))
}

fn int_as_f64(n: i64, span: Span) -> Result<Value, DatalogError> {
    // Large integers round to the nearest float, as an f64 literal would.
    float(n as f64, span)
}

/// The parameter kind a use at a position of type `ty` gives.
fn param_kind(ty: ValueType) -> ParamKind {
    match ty {
        ValueType::Ref => ParamKind::Entity,
        t => ParamKind::Value(t),
    }
}

/// Records in `types` that param `name` has type `ty` here.
fn unify_in(
    types: &mut HashMap<String, ValueType>,
    name: &str,
    ty: ValueType,
    span: Span,
) -> Result<(), DatalogError> {
    match types.get(name) {
        Some(&t) if t != ty => Err(mismatch(
            span,
            format!(
                "?{} is used as {} here but as {} elsewhere",
                show(name),
                type_name(ty),
                type_name(t)
            ),
        )),
        Some(_) => Ok(()),
        None => {
            types.insert(name.to_owned(), ty);
            Ok(())
        }
    }
}

/// Records that variable `name` of `scope` has type `ty` here.
fn unify_var(scope: &mut Scope, name: &str, ty: Ty, span: Span) -> Result<(), DatalogError> {
    match scope.types.get(name) {
        Some(&t) if t != ty => Err(mismatch(
            span,
            format!(
                "?{} is used as {} here but as {} elsewhere",
                show(name),
                ty_name(ty),
                ty_name(t)
            ),
        )),
        Some(_) => Ok(()),
        None => {
            scope.types.insert(name.to_owned(), ty);
            Ok(())
        }
    }
}

/// `:assert` / `:retract` as an op constant (where the other side of a
/// comparison, or a rule position, is an op), else `None`.
fn op_constant(item: &Item) -> Option<kala_core::Op> {
    match item {
        Item::Keyword(k, _) if k == "assert" => Some(kala_core::Op::Assert),
        Item::Keyword(k, _) if k == "retract" => Some(kala_core::Op::Retract),
        _ => None,
    }
}

/// Parameter `?s` used where only an op or an interval set can be: a
/// parameter is always a value.
fn param_not_a_value(s: &Sym, t: Ty) -> DatalogError {
    mismatch(
        s.span,
        format!(
            "?{} is a parameter, so it is a value; it can't be used as {}",
            show(&s.name),
            ty_name(t)
        ),
    )
}

/// Groups the definitions by name (in order of first definition) and checks
/// that each name's definitions agree on the head length.
fn group_rules(defs: &[Rule]) -> Result<(Vec<RuleInfo<'_>>, HashMap<&str, usize>), DatalogError> {
    let mut rules: Vec<RuleInfo<'_>> = Vec::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for d in defs {
        match by_name.get(d.name.as_str()) {
            Some(&i) => {
                let r = &mut rules[i];
                if r.arity != d.head.len() {
                    return Err(DatalogError::new(
                        ErrorKind::Arity,
                        Some(d.name_span),
                        format!(
                            "rule `{}` is defined with {} head variables here but {} before",
                            show(&d.name),
                            d.head.len(),
                            r.arity
                        ),
                    ));
                }
                r.defs.push(d);
            }
            None => {
                by_name.insert(d.name.as_str(), rules.len());
                rules.push(RuleInfo {
                    defs: vec![d],
                    arity: d.head.len(),
                    types: vec![None; d.head.len()],
                    callees: BTreeSet::new(),
                });
            }
        }
    }
    Ok((rules, by_name))
}

/// The rule `c` calls, checking it exists and takes that many arguments.
fn resolve_call(
    c: &Call,
    rules: &[RuleInfo<'_>],
    by_name: &HashMap<&str, usize>,
) -> Result<usize, DatalogError> {
    let Some(&i) = by_name.get(c.name.as_str()) else {
        return Err(DatalogError::new(
            ErrorKind::UnknownRule,
            Some(c.name_span),
            format!("no rule named `{}` in `:rules`", show(&c.name)),
        ));
    };
    let arity = rules[i].arity;
    if c.args.len() != arity {
        return Err(DatalogError::new(
            ErrorKind::Arity,
            Some(c.span),
            format!(
                "rule `{}` takes {arity} argument{}, not {}",
                show(&c.name),
                if arity == 1 { "" } else { "s" },
                c.args.len()
            ),
        ));
    }
    Ok(i)
}

/// A strongly connected component of the rule call graph: rules that call
/// each other, directly or through other rules, evaluated together as one
/// `Fixpoint`.
///
/// This is where stratification belongs: negation (slice 7 item 4) must
/// not reach a rule of its own component through a negated call.
struct Component {
    /// Its rules (indices into the rule list), in order of first definition.
    rules: Vec<usize>,
    /// Whether it is recursive: more than one rule, or a rule calling itself.
    recursive: bool,
}

/// The call graph's components in dependency order (every component after
/// the components it calls; otherwise in order of first definition), every
/// rule in exactly one. A cycle, even among rules `:where` never calls, is
/// recursion: its rules share a component.
fn components(rules: &[RuleInfo<'_>]) -> Vec<Component> {
    let n = rules.len();
    // At most 64 rules: a reachability matrix and quadratic walks are cheap.
    let reach: Vec<BTreeSet<usize>> = (0..n)
        .map(|i| reachable(rules, &rules[i].callees))
        .collect();
    let mut comp_of = vec![usize::MAX; n];
    let mut comps: Vec<Component> = Vec::new();
    for i in 0..n {
        if comp_of[i] != usize::MAX {
            continue;
        }
        let members: Vec<usize> = (i..n)
            .filter(|&j| j == i || (reach[i].contains(&j) && reach[j].contains(&i)))
            .collect();
        for &j in &members {
            comp_of[j] = comps.len();
        }
        comps.push(Component {
            recursive: reach[i].contains(&i),
            rules: members,
        });
    }
    let mut done = vec![false; comps.len()];
    let mut order = Vec::with_capacity(comps.len());
    while order.len() < comps.len() {
        let next = (0..comps.len())
            .find(|&c| {
                !done[c]
                    && comps[c].rules.iter().all(|&r| {
                        rules[r]
                            .callees
                            .iter()
                            .all(|&k| comp_of[k] == c || done[comp_of[k]])
                    })
            })
            .expect("invariant: the component graph is acyclic");
        done[next] = true;
        order.push(next);
    }
    let mut slots: Vec<Option<Component>> = comps.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|c| {
            slots[c]
                .take()
                .expect("invariant: each component is ordered once")
        })
        .collect()
}

/// The rules reachable from `roots` through calls.
fn reachable(rules: &[RuleInfo<'_>], roots: &BTreeSet<usize>) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<usize> = roots.iter().copied().collect();
    while let Some(i) = todo.pop() {
        if seen.insert(i) {
            todo.extend(rules[i].callees.iter().copied());
        }
    }
    seen
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

/// Every rule call of a body, those inside its negations (at any depth)
/// included.
fn all_calls<'q>(calls: &'q [Call], nots: &'q [Not], ors: &'q [Or]) -> Vec<&'q Call> {
    let mut out: Vec<&Call> = calls.iter().collect();
    for n in nots {
        out.extend(all_calls(&n.calls, &n.nots, &n.ors));
    }
    for o in ors {
        for b in &o.branches {
            out.extend(all_calls(&b.calls, &b.nots, &b.ors));
        }
    }
    out
}
fn negated_calls(c: Clauses<'_>) -> Vec<&Call> {
    let mut out = all_calls(&[], c.nots, &[]);
    for o in c.ors {
        for b in &o.branches {
            out.extend(negated_calls(Clauses::of_not(b)));
        }
    }
    out
}

/// Stratification: a rule's `not` may only reach rules of lower strata. A
/// call inside a `not` (at any depth) of a rule in the same component as
/// the rule would make the rule depend negatively on itself. Checked per
/// definition, in text order.
fn stratify(
    defs: &[Rule],
    by_name: &HashMap<&str, usize>,
    comps: &[Component],
) -> Result<(), DatalogError> {
    let mut comp_of: HashMap<usize, usize> = HashMap::new();
    for (ci, c) in comps.iter().enumerate() {
        for &r in &c.rules {
            comp_of.insert(r, ci);
        }
    }
    for d in defs {
        let me = comp_of[&by_name[d.name.as_str()]];
        let negated = negated_calls(Clauses::of_rule(d));
        if let Some(c) = negated
            .into_iter()
            .find(|c| comp_of[&by_name[c.name.as_str()]] == me)
        {
            return Err(DatalogError::new(
                ErrorKind::Stratification,
                Some(c.span),
                format!(
                    "rule `{}` negates `{}`, which depends on `{}` itself: negation through \
                     recursion can't be stratified",
                    show(&d.name),
                    show(&c.name),
                    show(&d.name)
                ),
            ));
        }
    }
    Ok(())
}

/// Propagate a recursive component's known types along callee-position to
/// caller-head-position edges. A body's patterns/functions provide the seeds;
/// rule calls are the only way another position gains a type. Each position is
/// queued at most once, so propagation is linear in positions and arguments.
/// Conflicts are left to `rule_defs`, which preserves the existing diagnostics.
fn propagate_head_types(
    comp: &Component,
    rules: &mut [RuleInfo<'_>],
    by_name: &HashMap<&str, usize>,
) {
    let mut offsets = vec![None; rules.len()];
    let mut types = Vec::new();
    for &ri in &comp.rules {
        offsets[ri] = Some(types.len());
        types.extend_from_slice(&rules[ri].types);
    }
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); types.len()];
    for &ri in &comp.rules {
        let target = offsets[ri].expect("invariant: component member has an offset");
        for def in &rules[ri].defs {
            let heads: HashMap<&str, usize> = def
                .head
                .iter()
                .enumerate()
                .map(|(i, h)| (h.name.as_str(), target + i))
                .collect();
            // Every head is bound positively (checked by rule_defs), so a
            // same-named use inside a negation is shared with the outer body.
            for call in all_calls(&def.calls, &def.nots, &def.ors) {
                let callee = by_name[call.name.as_str()];
                let Some(source) = offsets[callee] else {
                    continue;
                };
                for (i, arg) in call.args.iter().enumerate() {
                    if let Item::Var(s) = arg
                        && let Some(&to) = heads.get(s.name.as_str())
                    {
                        edges[source + i].push(to);
                    }
                }
            }
        }
    }
    let mut queue: VecDeque<usize> = types
        .iter()
        .enumerate()
        .filter_map(|(i, t)| t.map(|_| i))
        .collect();
    while let Some(from) = queue.pop_front() {
        for &to in &edges[from] {
            if types[to].is_none() {
                types[to] = types[from];
                queue.push_back(to);
            }
        }
    }
    for &ri in &comp.rules {
        let offset = offsets[ri].expect("invariant: component member has an offset");
        let arity = rules[ri].arity;
        rules[ri]
            .types
            .copy_from_slice(&types[offset..offset + arity]);
    }
}

/// The variable renames of definition number `k` (0-based) of its rule,
/// whose plan columns are named `canon` (the first definition's head).
/// Head variable `i` becomes `canon[i]`; a body variable that happens to be
/// named like a column it isn't gets a name no query can write (`?a'1`:
/// `'` is not a name character), so it can't be confused with the column.
fn renames(def: &Rule, canon: &[String], k: usize) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if k == 0 {
        return out;
    }
    for (h, c) in def.head.iter().zip(canon) {
        out.insert(h.name.clone(), c.clone());
    }
    let mut vars: Vec<&str> = Vec::new();
    body_vars(Clauses::of_rule(def), &mut vars);
    for v in vars {
        if !out.contains_key(v) && canon.iter().any(|c| c == v) {
            out.insert(v.to_owned(), format!("{v}'{k}"));
        }
    }
    out
}

/// One body's clauses as parsed (`:where`, a rule definition's or a
/// `not`'s), by kind.
#[derive(Clone, Copy)]
struct Clauses<'q> {
    clauses: &'q [Clause],
    calls: &'q [Call],
    nots: &'q [Not],
    ors: &'q [Or],
    durings: &'q [During],
    fns: &'q [FnClause],
}

impl<'q> Clauses<'q> {
    fn of_query(q: &'q Query) -> Clauses<'q> {
        Clauses {
            clauses: &q.clauses,
            calls: &q.calls,
            nots: &q.nots,
            ors: &q.ors,
            durings: &q.durings,
            fns: &q.fns,
        }
    }

    fn of_rule(r: &'q Rule) -> Clauses<'q> {
        Clauses {
            clauses: &r.clauses,
            calls: &r.calls,
            nots: &r.nots,
            ors: &r.ors,
            durings: &r.durings,
            fns: &r.fns,
        }
    }

    fn of_not(n: &'q Not) -> Clauses<'q> {
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
fn body_vars<'q>(c: Clauses<'q>, out: &mut Vec<&'q str>) {
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

fn precision_constant(item: &Item) -> Option<kala_core::Precision> {
    match item {
        Item::Keyword(k, _) => crate::write::precision_name(k),
        _ => None,
    }
}

// Bound the expanded prefix copies BEFORE planning/cloning. Each union branch
// carries its input bindings; successive disjunctions can multiply plan size.
fn check_or_cost(q: &Query) -> Result<(), DatalogError> {
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
