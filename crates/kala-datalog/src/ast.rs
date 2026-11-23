//! The parsed form of a Datalog query, before any schema lookup.
//!
//! Names are kept as written (attribute names, handles, IDs); the compiler
//! resolves them against a world. Every node carries the [`Span`] of the text
//! it came from, so later errors can point at it.

pub use crate::error::Span;
pub use kala_query::{AggFn, CmpOp, Func};

/// A whole query: `[:find .. :in .. :at .. :canon .. :rules .. :where ..]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// The columns returned, in order (`:find ?a (count ?b)`): variables
    /// and aggregates. Never empty.
    pub find: Vec<Find>,
    /// Inputs in order (`:in $ ?who`); may be empty.
    pub inputs: Vec<Input>,
    /// Query default tick (`:at T`); `None` means the world's latest tick.
    pub at: Option<TickExpr>,
    /// Canon to read (`:canon C`, i.e. `tx <= C`); `None` means the latest.
    pub canon: Option<CanonExpr>,
    /// The `:rules` definitions in text order; empty without a `:rules`
    /// section.
    pub rules: Vec<Rule>,
    /// The `:where` clauses other than rule calls, in query order.
    pub clauses: Vec<Clause>,
    /// The `:where` rule calls, in query order. Clause order doesn't matter
    /// (the planner orders them), so calls are kept in their own list.
    pub calls: Vec<Call>,
    /// The `:where` negations `(not ..)`, in query order (kept apart for
    /// the same reason). `clauses`, `calls`, `nots`, `durings` and `fns`
    /// are never all empty.
    pub nots: Vec<Not>,
    /// Disjunctions, each with matching branch variables.
    pub ors: Vec<Or>,
    /// The `:where` interval-set reads `(during ..)`, in query order.
    pub durings: Vec<During>,
    /// The `:where` interval-set function clauses, in query order.
    pub fns: Vec<FnClause>,
    /// The whole outer vector, `[` to `]`.
    pub span: Span,
}

/// One rule definition `[(name ?v ..) clause+]`. Several definitions of one
/// name are a union of their bodies.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    /// The rule's name (a bare symbol, not a reserved one).
    pub name: String,
    /// The name token.
    pub name_span: Span,
    /// The head variables: at least one, all distinct. Local to this
    /// definition, like every variable of its body.
    pub head: Vec<Sym>,
    /// The body's clauses other than rule calls, in text order.
    pub clauses: Vec<Clause>,
    /// The body's rule calls, in text order.
    pub calls: Vec<Call>,
    /// The body's negations, in text order. `clauses`, `calls`, `nots`,
    /// `durings` and `fns` are never all empty.
    pub nots: Vec<Not>,
    /// Disjunctions, each with matching branch variables.
    pub ors: Vec<Or>,
    /// The body's `(during ..)` reads, in text order.
    pub durings: Vec<During>,
    /// The body's interval-set function clauses, in text order.
    pub fns: Vec<FnClause>,
    /// The whole definition vector, `[` to `]`.
    pub span: Span,
}

/// A negation `(not clause+)`: keeps a row iff no way satisfies all its
/// clauses under the row's bindings. Variables bound outside (by a positive
/// clause of the enclosing body) are constraints; the others are local to
/// it. Its clauses are split by kind like a body's.
#[derive(Clone, Debug, PartialEq)]
pub struct Not {
    /// Patterns, `(at ..)` clauses and predicates, in text order.
    pub clauses: Vec<Clause>,
    /// Rule calls, in text order.
    pub calls: Vec<Call>,
    /// Nested negations, in text order. `clauses`, `calls`, `nots`,
    /// `durings` and `fns` are never all empty.
    pub nots: Vec<Not>,
    /// Disjunctions, each with matching branch variables.
    pub ors: Vec<Or>,
    /// `(during ..)` reads, in text order.
    pub durings: Vec<During>,
    /// Interval-set function clauses, in text order.
    pub fns: Vec<FnClause>,
    /// The whole list, `(` to `)`.
    pub span: Span,
}

/// A rule call `(name arg ..)` in `:where` or a rule body.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// The called rule's name.
    pub name: String,
    /// The name token.
    pub name_span: Span,
    /// The arguments in order: variables, `_` or constants (possibly none;
    /// the compiler checks the arity).
    pub args: Vec<Item>,
    /// The whole list, `(` to `)`.
    pub span: Span,
}

/// A variable occurrence: `?name`.
#[derive(Clone, Debug, PartialEq)]
pub struct Sym {
    /// The name without the leading `?`.
    pub name: String,
    /// The token, including the `?`.
    pub span: Span,
}

/// One `:find` element: a variable `?a` or an aggregate `(count ?b)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Find {
    /// The column's name: a variable's name without the `?`, or an
    /// aggregate's normalized text (`(count ?b)`: one space between the
    /// function and its variable, none inside the parentheses).
    pub name: String,
    /// The element as written: the variable token, or the whole list.
    pub span: Span,
    /// The aggregate, if this element is one (`None` for a variable).
    pub agg: Option<FindAgg>,
}

/// An aggregate in `:find`: `(func ?var)`.
#[derive(Clone, Debug, PartialEq)]
pub struct FindAgg {
    /// The aggregate function.
    pub func: AggFn,
    /// The aggregated variable.
    pub var: Sym,
}

/// One entry of the `:in` section.
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// `$`, the database.
    Db(Span),
    /// A parameter `?name`.
    Var(Sym),
}

/// A tick: a constant or a variable (a parameter, or bound earlier).
#[derive(Clone, Debug, PartialEq)]
pub enum TickExpr {
    /// A literal tick.
    Int(i64, Span),
    /// `?t`.
    Var(Sym),
}

/// A canon (`tx <= N`): a non-negative constant or a parameter.
#[derive(Clone, Debug, PartialEq)]
pub enum CanonExpr {
    /// A literal tx number.
    Int(u64, Span),
    /// `?c`.
    Var(Sym),
}

/// One `:where` clause.
#[derive(Clone, Debug, PartialEq)]
pub enum Clause {
    /// `[e a v]`, read at the query default tick.
    Pattern(Pattern),
    /// `(at T [e a v]..)`: patterns read at their own tick.
    At {
        /// The tick these patterns read at.
        tick: TickExpr,
        /// At least one pattern.
        patterns: Vec<Pattern>,
        /// The whole list, `(` to `)`.
        span: Span,
    },
    /// `[(op x y)]`.
    Pred(Pred),
}

/// `(during [e a v] ?s)`: binds `?s` to the interval set where `(e, a, v)`
/// holds, over every tick at the query's canon.
#[derive(Clone, Debug, PartialEq)]
pub struct During {
    /// The 3-element pattern (never a history pattern, never `:handle`).
    pub pattern: Pattern,
    /// The variable bound to the set.
    pub out: Sym,
    /// The whole list, `(` to `)`.
    pub span: Span,
}

/// An interval-set function clause: `[(intersect ?a ?b) ?c]`,
/// `[(union ?a ?b) ?c]`, `[(difference ?a ?b) ?c]` (with an output), or
/// the filters `[(not-empty ?s)]` and `[(contains ?s T)]`. The parser
/// checks the arity and whether there is an output.
#[derive(Clone, Debug, PartialEq)]
pub struct FnClause {
    /// The function.
    pub func: Func,
    /// The function name token.
    pub name_span: Span,
    /// The arguments in order.
    pub args: Vec<Item>,
    /// The output variable (set functions only).
    pub out: Option<Sym>,
    /// The whole outer vector, `[` to `]`.
    pub span: Span,
}

/// A pattern: three elements `[e a v]` (read at one tick), or a history
/// pattern `[e a v t]` / `[e a v t op]` (the change points of `(e, a, v)`
/// over every tick).
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    /// Entity position.
    pub e: Item,
    /// The attribute (a keyword).
    pub a: Attr,
    /// Value position.
    pub v: Item,
    /// The tick and op positions of a history pattern; `None` for `[e a v]`.
    pub history: Option<HistoryPos>,
    /// The whole vector, `[` to `]`.
    pub span: Span,
}

/// The extra positions of a history pattern `[e a v t op?]`.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryPos {
    /// The change point's tick: a variable, an integer or `_` (the parser
    /// refuses anything else).
    pub tick: Item,
    /// The change point's op; `None` for the 4-element form (any op).
    pub op: Option<OpItem>,
    /// Optional sixth history position: variable, wildcard or precision keyword.
    pub precision: Option<Item>,
}

/// The op position of a history pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum OpItem {
    /// `?op`: bound to the change point's op.
    Var(Sym),
    /// `_`: any op.
    Any(Span),
    /// `:assert`.
    Assert(Span),
    /// `:retract`.
    Retract(Span),
}

/// An attribute name as written (`:ally_of` → `ally_of`). May be an inverse
/// name or the `handle` pseudo-attribute; the compiler decides.
#[derive(Clone, Debug, PartialEq)]
pub struct Attr {
    /// The name without the leading `:`.
    pub name: String,
    /// The keyword token, including the `:`.
    pub span: Span,
}

/// A term in a pattern or predicate.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    /// `?name`.
    Var(Sym),
    /// `_`, matches anything and binds nothing.
    Any(Span),
    /// `:name`: a handle (the name is without the `:`).
    Keyword(String, Span),
    /// `TYPE#n` or `edge#n`, as written.
    Id(String, Span),
    /// A string literal, escapes resolved.
    Str(String, Span),
    /// An integer literal.
    Int(i64, Span),
    /// A float literal (always finite).
    Float(f64, Span),
    /// `true` / `false`.
    Bool(bool, Span),
}

impl Item {
    /// The source span of this item.
    pub fn span(&self) -> Span {
        match self {
            Item::Var(s) => s.span,
            Item::Any(s)
            | Item::Keyword(_, s)
            | Item::Id(_, s)
            | Item::Str(_, s)
            | Item::Int(_, s)
            | Item::Float(_, s)
            | Item::Bool(_, s) => *s,
        }
    }
}

/// A predicate clause `[(op left right)]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pred {
    /// The comparison.
    pub op: CmpOp,
    /// Left operand.
    pub left: Item,
    /// Right operand.
    pub right: Item,
    /// The whole outer vector, `[` to `]`.
    pub span: Span,
}

/// One transaction authored as EDN, with an optional default tick.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteTxn {
    /// True only for a top-level restore-mode `[:genesis ...]`.
    pub genesis: bool,
    pub at: Option<kala_core::Tick>,
    pub ops: Vec<WriteOp>,
    pub span: Span,
}

/// A literal value or an edge triple used as a reference.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteValue {
    Scalar(Item),
    Edge {
        source: Item,
        attr: Attr,
        target: Item,
    },
}

/// One write; validation of store rules is left to kala-core's transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum WriteOp {
    /// `[:attribute NAME TYPE OPTION*]`. Options left out are `None`, so an
    /// existing attribute keeps its labels (resolved by the transaction, see
    /// `kala_core::Txn::declare_attribute_with`).
    Attribute {
        inferred: bool,
        name: String,
        value_type: kala_core::ValueType,
        options: kala_core::AttributeOptions,
        span: Span,
    },
    NodeType {
        inferred: bool,
        name: String,
        span: Span,
    },
    Rename {
        old: String,
        new: String,
        span: Span,
    },
    Deprecate {
        name: String,
        span: Span,
    },
    Cardinality {
        name: String,
        cardinality: kala_core::Cardinality,
        span: Span,
    },
    Migrate {
        name: String,
        options: kala_core::MigrationOptions,
        span: Span,
    },
    WorldSetting {
        key: String,
        value: String,
        span: Span,
    },
    Create {
        node_type: String,
        handle: Option<String>,
        span: Span,
    },
    Fact {
        op: kala_core::Op,
        subject: WriteValue,
        attr: Attr,
        value: WriteValue,
        at: Option<kala_core::Tick>,
        precision: kala_core::Precision,
        span: Span,
    },
    Correct {
        entry: kala_core::EntryId,
        replacement: Option<Replacement>,
        span: Span,
    },
}

/// Fields replaced by a correction; omitted fields retain their current values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Replacement {
    pub value: Option<WriteValue>,
    pub at: Option<kala_core::Tick>,
    pub op: Option<kala_core::Op>,
    pub precision: Option<kala_core::Precision>,
}

/// A disjunction `(or clause (and clause ...))`.
#[derive(Clone, Debug, PartialEq)]
pub struct Or {
    /// Each branch is a conjunction; its container is not a negation.
    pub branches: Vec<Not>,
    pub span: Span,
}

impl Query {
    /// Every attribute the query text names, in no particular order and
    /// possibly repeated: patterns in `:where`, inside `(at ..)`, history
    /// patterns, `(during ..)`, `(not ..)`, `(or ..)` branches and every
    /// `:rules` body, including rules no clause calls.
    ///
    /// For callers that refuse queries by attribute (before compiling, so a
    /// refused query is never run). The attribute position is always a
    /// constant keyword today; if a non-constant attribute position is ever
    /// added, it must be reported here so such callers can fail closed.
    pub fn attributes(&self) -> Vec<&Attr> {
        let mut out = Vec::new();
        let body = Body {
            clauses: &self.clauses,
            nots: &self.nots,
            ors: &self.ors,
            durings: &self.durings,
        };
        body.attributes(&mut out);
        for rule in &self.rules {
            Body {
                clauses: &rule.clauses,
                nots: &rule.nots,
                ors: &rule.ors,
                durings: &rule.durings,
            }
            .attributes(&mut out);
        }
        out
    }
}

/// The attribute-carrying parts of a query, rule or negation body. Rule
/// calls, predicates and set functions name no attribute.
struct Body<'q> {
    clauses: &'q [Clause],
    nots: &'q [Not],
    ors: &'q [Or],
    durings: &'q [During],
}

impl<'q> Body<'q> {
    fn of(n: &'q Not) -> Self {
        Body {
            clauses: &n.clauses,
            nots: &n.nots,
            ors: &n.ors,
            durings: &n.durings,
        }
    }

    /// Walks with an explicit stack: nesting depth is bounded by the parser,
    /// but this keeps the walk off the native stack regardless.
    fn attributes(self, out: &mut Vec<&'q Attr>) {
        let mut stack = vec![self];
        while let Some(body) = stack.pop() {
            for clause in body.clauses {
                match clause {
                    Clause::Pattern(p) => out.push(&p.a),
                    Clause::At { patterns, .. } => out.extend(patterns.iter().map(|p| &p.a)),
                    Clause::Pred(_) => {}
                }
            }
            out.extend(body.durings.iter().map(|d| &d.pattern.a));
            stack.extend(body.nots.iter().map(Body::of));
            stack.extend(
                body.ors
                    .iter()
                    .flat_map(|o| o.branches.iter().map(Body::of)),
            );
        }
    }
}
