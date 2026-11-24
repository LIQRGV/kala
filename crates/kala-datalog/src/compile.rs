//! The compiler: resolves a parsed [`Query`] against a world's schema and
//! IDs, types every variable and parameter from its uses, and hands the
//! resolved clauses to the planner.
//!
//! Every error carries the [`Span`] of what it is about; [`crate::prepare`]
//! locates it in the query text.

use std::collections::{BTreeSet, HashMap};

use kala_core::{EntityId, Tick, Value, ValueType, World};
use kala_query::{
    Agg, AggFn, AttrRef, CanonSource, Op, OpTerm, Plan, RelDef, Term, TickSource, Var,
};

use crate::ast::{
    Call, CanonExpr, Clause, During, FnClause, HistoryPos, Input, Item, Not, OpItem, Or, Pattern,
    Pred, Query, Sym, TickExpr,
};
use crate::error::{DatalogError, ErrorKind, Span, show};
use crate::plan::{self, Hist, Pat, PatAttr};

mod clauses;
mod predicates;
mod rules;

use clauses::{Clauses, body_vars, check_or_cost};
use rules::{
    Component, RuleInfo, components, group_rules, link_calls, propagate_head_types, reachable,
    renames, stratify,
};

pub use kala_query::{ParamKind, Prepared};

/// Compiles and plans `q` against `world`; see [`crate::compile`].
pub(crate) fn compile(world: &World, q: &Query) -> Result<Prepared, DatalogError> {
    Compiler::new(world).run(q)
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
        Ty::Val(v) => v.name(),
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

/// Every rule definition resolved, before `:where` is.
struct Resolved<'q> {
    /// One scope per definition, in resolution order.
    scopes: Vec<Scope>,
    /// Each scope's body.
    bodies: Vec<BodyC<'q>>,
    /// Per rule: (the plan's head vars, scope index of each definition).
    heads: Vec<(Vec<Var>, Vec<usize>)>,
}

/// The `:find` columns of a query.
struct Columns {
    /// Every column, in order.
    find: Vec<Var>,
    /// The plain variables: an aggregate query's groups.
    group: Vec<Var>,
    /// The aggregates, each distinct one once.
    aggs: Vec<Agg>,
}

struct Compiler<'w> {
    world: &'w World,
    schema_canon: kala_core::TxId,
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
            schema_canon: world.latest_tx(),
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
        if let Some(CanonExpr::Int(canon, _)) = q.canon {
            self.schema_canon = kala_core::TxId(canon);
        }
        self.declare_params(&q.inputs)?;

        let (mut rules, by_name) = group_rules(&q.rules)?;
        let where_callees = link_calls(q, &mut rules, &by_name)?;
        let comps = components(&rules);
        stratify(&q.rules, &by_name, &comps)?;

        let default_tick = self.default_tick(q)?;
        let canon = self.canon(q)?;

        // Every scope (rule definitions, then `:where` last) and its body.
        let Resolved {
            mut scopes,
            mut bodies,
            heads,
        } = self.rule_bodies(&comps, &mut rules, &by_name)?;
        let mut where_scope = Scope::default();
        bodies.push(self.body(Clauses::of_query(q), &mut where_scope, &rules, &by_name)?);
        scopes.push(where_scope);
        let mut planned = self.predicates(&scopes, bodies)?;
        let Columns { find, group, aggs } = self.columns(q, &scopes[scopes.len() - 1])?;
        let params = self.used_params()?;

        let fixes = self.plan_rules(&rules, &comps, &where_callees, &heads, &mut planned)?;
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

    /// Declares the `:in` parameters, in order; each name once.
    fn declare_params(&mut self, inputs: &[Input]) -> Result<(), DatalogError> {
        for input in inputs {
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
        Ok(())
    }

    /// The query's default tick, from `:at`.
    fn default_tick(&mut self, q: &Query) -> Result<TickSource, DatalogError> {
        Ok(match &q.at {
            None => TickSource::Default,
            Some(TickExpr::Int(n, _)) => TickSource::Const(Tick(*n)),
            Some(TickExpr::Var(s)) => {
                self.section_param(s, "`:at`", ParamKind::Tick)?;
                TickSource::Param(s.name.clone())
            }
        })
    }

    /// The canon the query reads, from `:canon`.
    fn canon(&mut self, q: &Query) -> Result<CanonSource, DatalogError> {
        Ok(match &q.canon {
            None => CanonSource::Latest,
            Some(CanonExpr::Int(n, _)) => CanonSource::Const(*n),
            Some(CanonExpr::Var(s)) => {
                self.section_param(s, "`:canon`", ParamKind::Canon)?;
                CanonSource::Param(s.name.clone())
            }
        })
    }

    /// Resolves every rule definition's body, component by component in
    /// dependency order, so a call's head positions are typed before its
    /// callers are resolved.
    fn rule_bodies<'q>(
        &mut self,
        comps: &[Component],
        rules: &mut [RuleInfo<'q>],
        by_name: &HashMap<&str, usize>,
    ) -> Result<Resolved<'q>, DatalogError> {
        let mut scopes: Vec<Scope> = Vec::new();
        let mut bodies: Vec<BodyC<'_>> = Vec::new();
        let mut heads: Vec<(Vec<Var>, Vec<usize>)> = vec![(Vec::new(), Vec::new()); rules.len()];
        for comp in comps {
            // Resolve once for the initial type seeds, propagate only newly
            // known head positions, then resolve once more for the original
            // validations and final IR. Rotating a large recursive head must
            // not recompile its whole body once per position.
            let mut out = Vec::new();
            for &ri in &comp.rules {
                self.rule_defs(ri, rules, by_name, &mut out)?;
            }
            if comp.recursive {
                propagate_head_types(comp, rules, by_name);
                out.clear();
                for &ri in &comp.rules {
                    self.rule_defs(ri, rules, by_name, &mut out)?;
                }
            }
            for (ri, scope, body) in out {
                heads[ri].1.push(scopes.len());
                scopes.push(scope);
                bodies.push(body);
            }
            for &ri in &comp.rules {
                heads[ri].0 = rules[ri].defs[0]
                    .head
                    .iter()
                    .map(|h| Var(h.name.clone()))
                    .collect();
            }
        }
        Ok(Resolved {
            scopes,
            bodies,
            heads,
        })
    }

    /// The `:find` columns, and for an aggregate query its groups (the plain
    /// variables) and aggregates (each distinct one once).
    fn columns(&self, q: &Query, where_scope: &Scope) -> Result<Columns, DatalogError> {
        let mut find = Vec::with_capacity(q.find.len());
        let mut group: Vec<Var> = Vec::new();
        let mut aggs: Vec<Agg> = Vec::new();
        for f in &q.find {
            let col = Var(f.name.clone());
            match &f.agg {
                None => {
                    self.find_var(where_scope, &f.name, f.span)?;
                    if !group.contains(&col) {
                        group.push(col.clone());
                    }
                }
                Some(a) => {
                    let ty = self.find_var(where_scope, &a.var.name, a.var.span)?;
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
        Ok(Columns { find, group, aggs })
    }

    /// The declared parameters with their kinds, in `:in` order; one the
    /// query never uses is an error.
    fn used_params(&self) -> Result<Vec<(String, ParamKind)>, DatalogError> {
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
        Ok(params)
    }

    /// Plans the rules `:where` reaches, each body as a conjunctive query
    /// projected onto the head: one `Fixpoint`'s relations per component of
    /// the call graph (all of a component's relations, so mutually
    /// recursive rules are evaluated together), in dependency order.
    fn plan_rules(
        &self,
        rules: &[RuleInfo<'_>],
        comps: &[Component],
        where_callees: &BTreeSet<usize>,
        heads: &[(Vec<Var>, Vec<usize>)],
        planned: &mut [plan::Body],
    ) -> Result<Vec<Vec<RelDef>>, DatalogError> {
        let reach = reachable(rules, where_callees);
        let mut fixes: Vec<Vec<RelDef>> = Vec::new();
        for comp in comps.iter().filter(|c| reach.contains(&c.rules[0])) {
            let mut rels = Vec::with_capacity(comp.rules.len());
            for &ri in &comp.rules {
                let (head, defs) = &heads[ri];
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
        Ok(fixes)
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
                    let (source, tick_var) = self.at_tick(tick, scope, &mut tick_vars)?;
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
            ors.push(self.or(o, scope, rules, by_name)?);
        }
        check_tick_vars_bound(&tick_vars, scope)?;
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

    /// The tick of an `(at tick ..)` clause as its patterns read it: a
    /// constant, a parameter, or a variable some other clause must bind
    /// (noted in `tick_vars`, and handed to the patterns as their tick
    /// variable).
    fn at_tick<'q>(
        &mut self,
        tick: &'q TickExpr,
        scope: &mut Scope,
        tick_vars: &mut Vec<&'q Sym>,
    ) -> Result<(TickSource, Option<(String, Span)>), DatalogError> {
        Ok(match tick {
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
        })
    }

    /// One `(or ..)` of a body: each branch is resolved in a copy of
    /// `scope`; every branch must mention and bind the same variables, which
    /// the `or` then binds (and types) in `scope`.
    fn or<'q>(
        &mut self,
        o: &'q Or,
        scope: &mut Scope,
        rules: &[RuleInfo<'_>],
        by_name: &HashMap<&str, usize>,
    ) -> Result<OrC<'q>, DatalogError> {
        let mut vars = Vec::new();
        let mut branches = Vec::new();
        let inherited = scope.bound.clone();
        for b in &o.branches {
            let names = self.branch_vars(b, scope);
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
        Ok(OrC {
            branches,
            binds: binds.iter().map(|n| scope.var(n).0).collect(),
            uses,
            span: o.span,
        })
    }

    /// The variables (parameters left out) one `or` branch mentions.
    fn branch_vars(&self, branch: &Not, scope: &Scope) -> BTreeSet<String> {
        let mut mentioned = Vec::new();
        body_vars(Clauses::of_not(branch), &mut mentioned);
        mentioned
            .into_iter()
            .filter(|v| !self.is_param(scope, v))
            .map(str::to_owned)
            .collect()
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
                                "an entity (`:edwin`, `TYPE#n` or `edge#n`)".to_string()
                            } else {
                                format!("a {}", ty.name())
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
        if p.history.is_some() && p.a.name == "kala_handle" {
            return Err(DatalogError::new(
                ErrorKind::Syntax,
                Some(p.a.span),
                "`:kala_handle` has no history; a history pattern needs a schema attribute",
            ));
        }
        let (attr, info, value_type) = if p.a.name == "kala_handle" {
            (AttrRef::Handle, PatAttr::Handle, ValueType::Str)
        } else {
            let (value_type, indexed) = self.resolve_attr(&p.a.name).map_err(|e| {
                DatalogError::new(
                    ErrorKind::UnknownAttribute,
                    Some(p.a.span),
                    match e {
                        kala_core::Error::UnknownAttribute(_) if p.a.name == "handle" => {
                            "attribute `:handle` is not defined; the built-in handle attribute is now :kala_handle".into()
                        }
                        kala_core::Error::UnknownAttribute(_) => {
                            format!("unknown attribute `:{}`", show(&p.a.name))
                        }
                        other => other.to_string(),
                    },
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
        Ok(Hist {
            tick: self.history_tick(&h.tick, scope)?,
            op: self.history_op(h.op.as_ref(), scope)?,
            precision: self.history_precision(h.precision.as_ref(), scope)?,
        })
    }

    /// The tick position of a history pattern.
    fn history_tick(&mut self, tick: &Item, scope: &mut Scope) -> Result<Term, DatalogError> {
        Ok(match tick {
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
        })
    }

    /// The op position of a history pattern (absent: any op).
    fn history_op(
        &mut self,
        op: Option<&OpItem>,
        scope: &mut Scope,
    ) -> Result<OpTerm, DatalogError> {
        Ok(match op {
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
        })
    }

    /// The precision position of a history pattern (absent: any precision).
    fn history_precision(
        &mut self,
        precision: Option<&Item>,
        scope: &mut Scope,
    ) -> Result<Term, DatalogError> {
        Ok(match precision {
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
        })
    }

    /// An attribute's value type and `indexed` flag; an inverse name reads
    /// `ref` values.
    fn resolve_attr(&self, name: &str) -> Result<(ValueType, bool), kala_core::Error> {
        let (def, _) = self
            .world
            .at_canon(self.schema_canon)
            .resolve_attribute(name)?;
        Ok((def.value_type, def.indexed))
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
                            "an entity (`:edwin`, `TYPE#n` or `edge#n`)".to_string()
                        } else if ty == ValueType::Ref {
                            "a ref (`:edwin`, `TYPE#n` or `edge#n`)".to_string()
                        } else {
                            format!("a {}", ty.name())
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

    /// `(during [e a v] ?s)`: the pattern resolved as for a read (it reads
    /// every tick, at the query's canon), with `?s` an interval set.
    fn during(&mut self, d: &During, scope: &mut Scope) -> Result<Pat, DatalogError> {
        // The parser refuses both; an AST built by hand may not.
        if d.pattern.history.is_some() || d.pattern.a.name == "kala_handle" {
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
}

/// Checks that every tick variable of a body's `(at ?t ..)` clauses is bound
/// by one of the body's clauses.
fn check_tick_vars_bound(tick_vars: &[&Sym], scope: &Scope) -> Result<(), DatalogError> {
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
    Ok(())
}

/// `[?p :kala_handle :varrow]`: a handle keyword where `:kala_handle` needs the
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
            "the value position of `:kala_handle` needs a string, not the entity `:{h}`; \
             a handle keyword already is its entity, so use it directly \
             (as in `[?c :located_in :varrow]`), or match the handle's text: \
             `[{e} :kala_handle \"{h}\"]`"
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
                ty.name(),
                t.name()
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

fn precision_constant(item: &Item) -> Option<kala_core::Precision> {
    match item {
        Item::Keyword(k, _) => crate::write::precision_name(k),
        _ => None,
    }
}
