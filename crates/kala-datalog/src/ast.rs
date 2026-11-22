//! The parsed form of a Datalog query, before any schema lookup.
//!
//! Names are kept as written (attribute names, handles, IDs); the compiler
//! resolves them against a world. Every node carries the [`Span`] of the text
//! it came from, so later errors can point at it.

pub use crate::error::Span;
pub use kala_query::CmpOp;

/// A whole query: `[:find .. :in .. :at .. :canon .. :where ..]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    /// Variables returned, in order (`:find ?a ?b`). Never empty.
    pub find: Vec<Sym>,
    /// Inputs in order (`:in $ ?who`); may be empty.
    pub inputs: Vec<Input>,
    /// Query default tick (`:at T`); `None` means the world's latest tick.
    pub at: Option<TickExpr>,
    /// Canon to read (`:canon C`, i.e. `tx <= C`); `None` means the latest.
    pub canon: Option<CanonExpr>,
    /// The `:where` clauses in query order. Never empty.
    pub clauses: Vec<Clause>,
    /// The whole outer vector, `[` to `]`.
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

/// A three-element pattern `[e a v]`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    /// Entity position.
    pub e: Item,
    /// The attribute (a keyword).
    pub a: Attr,
    /// Value position.
    pub v: Item,
    /// The whole vector, `[` to `]`.
    pub span: Span,
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
