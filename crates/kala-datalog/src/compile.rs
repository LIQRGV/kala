//! The compiler: resolves a parsed [`Query`] against a world's schema and
//! IDs, types every variable and parameter from its uses, and hands the
//! resolved clauses to the planner.
//!
//! Every error carries the [`Span`] of what it is about; [`crate::prepare`]
//! locates it in the query text.

use std::collections::{BTreeSet, HashMap};

use kala_core::{EntityId, Tick, Value, ValueType, World};
use kala_query::{AttrRef, CanonSource, Plan, Predicate, Term, TickSource, Var};

use crate::ast::{CanonExpr, Clause, Input, Item, Pattern, Pred, Query, Sym, TickExpr};
use crate::error::{DatalogError, ErrorKind, Span, show};
use crate::plan::{self, Filt, Pat, PatAttr};

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

struct Compiler<'w> {
    world: &'w World,
    /// Parameters by name.
    params: HashMap<String, Param>,
    /// `:in` order.
    param_order: Vec<String>,
    /// The type of every variable and parameter used so far.
    types: HashMap<String, ValueType>,
    /// Variables that appear in some pattern (so a read binds them).
    pattern_vars: BTreeSet<String>,
}

impl<'w> Compiler<'w> {
    fn new(world: &'w World) -> Compiler<'w> {
        Compiler {
            world,
            params: HashMap::new(),
            param_order: Vec::new(),
            types: HashMap::new(),
            pattern_vars: BTreeSet::new(),
        }
    }

    fn run(mut self, q: &Query) -> Result<Prepared, DatalogError> {
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

        let mut pats = Vec::new();
        let mut preds: Vec<&Pred> = Vec::new();
        // Tick variables (not params) of `(at ?t ..)` clauses, checked once
        // every pattern is known: clause order in the text doesn't matter.
        let mut tick_vars: Vec<&Sym> = Vec::new();
        for clause in &q.clauses {
            match clause {
                Clause::Pattern(p) => pats.push(self.pattern(p, TickSource::Default, None)?),
                Clause::At { tick, patterns, .. } => {
                    let (source, tick_var) = match tick {
                        TickExpr::Int(n, _) => (TickSource::Const(Tick(*n)), None),
                        TickExpr::Var(s) if self.params.contains_key(&s.name) => {
                            self.use_param(s, ValueType::I64, ParamKind::Tick)?;
                            (TickSource::Param(s.name.clone()), None)
                        }
                        TickExpr::Var(s) => {
                            self.unify(&s.name, ValueType::I64, s.span)?;
                            tick_vars.push(s);
                            (
                                TickSource::Var(Var(s.name.clone())),
                                Some((s.name.clone(), s.span)),
                            )
                        }
                    };
                    for p in patterns {
                        pats.push(self.pattern(p, source.clone(), tick_var.clone())?);
                    }
                }
                Clause::Pred(p) => preds.push(p),
            }
        }

        for s in tick_vars {
            if !self.pattern_vars.contains(&s.name) {
                return Err(unbound(
                    s.span,
                    format!(
                        "tick variable ?{} is not bound by any pattern or `:in` parameter",
                        show(&s.name)
                    ),
                ));
            }
        }

        let filts = self.predicates(&preds)?;

        let mut find = Vec::with_capacity(q.find.len());
        for s in &q.find {
            if !self.pattern_vars.contains(&s.name) {
                let what = if self.params.contains_key(&s.name) {
                    "is a parameter; `:find` needs a variable a pattern binds"
                } else {
                    "is not bound by any pattern"
                };
                return Err(unbound(s.span, format!("?{} {what}", show(&s.name))));
            }
            find.push(Var(s.name.clone()));
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

        let root = plan::plan(self.world, pats, filts, &find)?;
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

    /// Records that `name` (a variable or param) has type `ty` here.
    fn unify(&mut self, name: &str, ty: ValueType, span: Span) -> Result<(), DatalogError> {
        match self.types.get(name) {
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
                self.types.insert(name.to_owned(), ty);
                Ok(())
            }
        }
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
    ) -> Result<Pat, DatalogError> {
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
        let entity = self.position(&p.e, ValueType::Ref, true)?;
        let value = self.position(&p.v, value_type, false)?;
        Ok(Pat {
            entity,
            attr,
            info,
            value,
            tick,
            tick_var,
            span: p.span,
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
    ) -> Result<Term, DatalogError> {
        Ok(match item {
            Item::Var(s) => {
                if self.params.contains_key(&s.name) {
                    let kind = match ty {
                        ValueType::Ref => ParamKind::Entity,
                        t => ParamKind::Value(t),
                    };
                    self.use_param(s, ty, kind)?;
                    Term::Param(s.name.clone())
                } else {
                    self.unify(&s.name, ty, s.span)?;
                    self.pattern_vars.insert(s.name.clone());
                    Term::Var(Var(s.name.clone()))
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

    /// Whether `id` (a parsed ID) is defined at the latest canon.
    fn defined(&self, id: EntityId) -> bool {
        if let Some(edge) = id.as_edge() {
            return self.world.edge(edge).is_some();
        }
        let Some(type_name) = self.world.node_types().nth(usize::from(id.node_type().0)) else {
            return false;
        };
        // Compile time only, and worlds are small: a linear check is fine.
        self.world
            .entities_of_type(type_name)
            .is_ok_and(|mut it| it.any(|e| e == id))
    }

    /// Types and resolves the predicates. A parameter only predicates use
    /// takes the type of what it is compared with, so they are revisited
    /// until no more types are learned.
    fn predicates(&mut self, preds: &[&Pred]) -> Result<Vec<Filt>, DatalogError> {
        // Operand checks that don't depend on other predicates come first.
        for p in preds {
            for item in [&p.left, &p.right] {
                match item {
                    Item::Var(s) if !self.params.contains_key(&s.name) => {
                        if !self.pattern_vars.contains(&s.name) {
                            return Err(unbound(
                                s.span,
                                format!("?{} is not bound by any pattern", show(&s.name)),
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
            for (i, p) in preds.iter().enumerate() {
                if done[i] {
                    continue;
                }
                if let Some(f) = self.predicate(p)? {
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
            let p = preds[i];
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
        Ok(out.into_iter().flatten().collect())
    }

    /// One predicate, or `None` if both operands are still untyped params.
    fn predicate(&mut self, p: &Pred) -> Result<Option<Filt>, DatalogError> {
        let lt = self.operand_type(&p.left)?;
        let rt = self.operand_type(&p.right)?;
        let ty = match (lt, rt) {
            (None, None) => return Ok(None),
            (Some(t), None) | (None, Some(t)) => t,
            (Some(l), Some(r)) if l == r => l,
            // An integer literal compares with an f64 as a float.
            (Some(ValueType::F64), Some(ValueType::I64)) if matches!(p.right, Item::Int(..)) => {
                ValueType::F64
            }
            (Some(ValueType::I64), Some(ValueType::F64)) if matches!(p.left, Item::Int(..)) => {
                ValueType::F64
            }
            (Some(l), Some(r)) => {
                return Err(mismatch(
                    p.span,
                    format!("can't compare {} with {}", type_name(l), type_name(r)),
                ));
            }
        };
        let left = self.operand(&p.left, ty)?;
        let right = self.operand(&p.right, ty)?;
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

    /// The type of an operand, if known yet.
    fn operand_type(&self, item: &Item) -> Result<Option<ValueType>, DatalogError> {
        Ok(match item {
            Item::Var(s) => self.types.get(&s.name).copied(),
            Item::Any(_) => None,
            other => Some(self.constant(other)?.0.value_type()),
        })
    }

    /// An operand as a term of type `ty` (already checked compatible).
    fn operand(&mut self, item: &Item, ty: ValueType) -> Result<Term, DatalogError> {
        Ok(match item {
            Item::Var(s) if self.params.contains_key(&s.name) => {
                let kind = match ty {
                    ValueType::Ref => ParamKind::Entity,
                    t => ParamKind::Value(t),
                };
                self.use_param(s, ty, kind)?;
                Term::Param(s.name.clone())
            }
            Item::Var(s) => Term::Var(Var(s.name.clone())),
            Item::Any(span) => {
                unreachable!("invariant: `_` operands were rejected before typing ({span:?})")
            }
            other => match (ty, self.constant(other)?.0) {
                (ValueType::F64, Value::I64(n)) => Term::Const(int_as_f64(n, other.span())?),
                (_, v) => Term::Const(v),
            },
        })
    }
}

fn float(f: f64, span: Span) -> Result<Value, DatalogError> {
    Value::f64(f).map_err(|e| mismatch(span, format!("not a storable float: {e}")))
}

fn int_as_f64(n: i64, span: Span) -> Result<Value, DatalogError> {
    // Large integers round to the nearest float, as an f64 literal would.
    float(n as f64, span)
}
