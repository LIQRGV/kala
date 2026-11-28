//! Kala's Datalog front-end: a parser for an EDN subset, a compiler from
//! the parsed query to the shared IR (`kala_query`), and a greedy planner
//! that picks the index each pattern reads.
//!
//! Datalog semantics (results are sets) are decided here; the executor stays
//! language-neutral. Design: docs/slice-6-decisions.md.

#![forbid(unsafe_code)]

pub mod apply;
pub mod ast;
mod checks;
mod compile;
mod error;
mod lex;
mod parse;
mod plan;
mod session;

pub use checks::Check;
pub use compile::{ParamKind, Prepared};
pub use error::{DatalogError, ErrorKind, Span};
pub use session::{SessionDefaults, SessionRule, parse_rules, prepare_with};

use kala_core::World;
use kala_query::Plan;

/// Largest query text accepted, in bytes. Checked before anything else.
pub const MAX_QUERY_BYTES: usize = 64 * 1024;

/// Deepest bracket nesting accepted (`[`/`(` levels, the outer vector
/// included). Real queries need 3.
pub const MAX_DEPTH: usize = 32;

/// Most patterns in one query, counting those inside `(at T ..)`.
pub const MAX_PATTERNS: usize = 256;

/// Most clauses in one query, of every kind: patterns, predicates and
/// `(at T ..)` wrappers, nested ones included. Bounds plan size (every
/// predicate is an operator) independently of [`MAX_PATTERNS`].
pub const MAX_CLAUSES: usize = 512;

/// Most rule definitions in one query's `:rules` section. Every definition
/// counts, several bodies of one rule name included.
pub const MAX_RULES: usize = 64;

/// Largest story-check file accepted by [`parse_checks`], in bytes.
pub const MAX_CHECKS_BYTES: usize = 1 << 20;

/// Most checks in one story-check file.
pub const MAX_CHECKS: usize = 256;

/// Parse Datalog query text into an [`ast::Query`].
///
/// The grammar is an EDN subset:
/// `[:find col.. (:in $? ?p..)? (:at T)? (:canon C)? (:rules [def+])? :where clause+]`,
/// where a `:find` column is a variable `?v` or an aggregate `(count ?v)`,
/// `(count-distinct ?v)`, `(min ?v)`, `(max ?v)` or `(sum ?v)` (grouped by
/// the plain variables, over every binding of the `:where` variables),
/// with clauses `[e a v]`, history patterns `[e a v t]` / `[e a v t op]`
/// (change points at every tick; `t` a variable, integer or `_`, `op` a
/// variable, `_`, `:assert` or `:retract`; never inside `(at ..)`),
/// `(at T [e a v]..)`, `[(op x y)]`, interval sets `(during [e a v] ?s)`
/// (never inside `(at ..)`), the set functions `[(intersect ?a ?b) ?c]`,
/// `[(union ?a ?b) ?c]`, `[(difference ?a ?b) ?c]` and the filters
/// `[(not-empty ?s)]`, `[(contains ?s T)]`, rule calls
/// `(name arg..)` and negations `(not clause+)` (nestable); a rule definition is `[(name ?v+) clause+]` (several
/// definitions of one name are a union). Names are not resolved here; that
/// is the compiler's job.
///
/// Limits: [`MAX_QUERY_BYTES`], [`MAX_DEPTH`], [`MAX_PATTERNS`],
/// [`MAX_CLAUSES`] (both counted over rule bodies and `:where` together)
/// and [`MAX_RULES`] ([`ErrorKind::Limit`]).
///
/// Never panics. Errors carry a [`Span`] (except possibly the size limit)
/// and are located, so their `Display` is `line L, column C: message`.
pub fn parse(text: &str) -> Result<ast::Query, DatalogError> {
    parse::parse(text)
}

/// Compile a parsed query against `world` and plan it.
///
/// Resolves attribute names (inverse names allowed; `:kala_handle` is the
/// built-in entity <-> handle pseudo-attribute), handle and ID constants,
/// and types every variable and `:in` parameter from its uses. Then a
/// greedy, deterministic planner orders the patterns and picks each one's
/// index (see the crate's `plan` module).
///
/// Rules: a rule's head variables are local to each definition (and shadow
/// a same-named parameter there); other variables are local too, while
/// parameters are global. Body patterns read at the query default tick
/// unless wrapped in `(at T ..)`. Each rule `:where` reaches becomes one
/// [`kala_query::RelDef`] whose rules are its bodies, each planned as a
/// conjunctive query projected onto the head; calls become
/// [`kala_query::Op::Rel`]. Strata nest: one `Fixpoint` per relation, the
/// rule that calls no other outermost, so a relation's rules only read
/// relations of enclosing `Fixpoint`s; a recursive component is one
/// semi-naive `Fixpoint`.
///
/// History patterns compile to [`kala_query::Op::History`]: they read every
/// tick at the query's canon (`:at` and `(at ..)` don't apply); a tick
/// variable they bind is an `i64` like any other, an op variable binds an
/// op. Joining two of them on a tick variable only matches ticks where both
/// changed, never overlaps of what holds.
///
/// Interval sets: `(during [e a v] ?s)` compiles to
/// [`kala_query::Op::During`] (every tick at the query's canon, like a
/// history pattern; free `e` / `v` bind one row per pair that ever held);
/// the functions compile to [`kala_query::Op::Apply`], placed once their
/// arguments are bound. An interval set is its own type: never in a
/// pattern position or a tick, comparable only with `=` / `!=` to another
/// set; a parameter can't be one. A function's output that is already
/// bound is an equality constraint, as is `during`'s.
///
/// Negation: `(not clause+)` keeps a row iff no way satisfies all its
/// clauses under the row's bindings. Variables a positive clause of the
/// enclosing body binds are shared; the others are local to the `not`
/// (a predicate or tick variable inside must be bound by one or the
/// other, else [`ErrorKind::UnboundVariable`]). It compiles to
/// [`kala_query::Op::Not`], placed once its shared variables are bound. A
/// `not` reaching, through calls, a rule of the recursive component of the
/// rule containing it is [`ErrorKind::Stratification`].
///
/// Errors carry the span of what they are about, but are not located: use
/// [`DatalogError::locate`] with the query text, or call [`prepare`].
/// Never panics.
pub fn compile(world: &World, q: &ast::Query) -> Result<Plan, DatalogError> {
    compile::compile(world, q).map(|p| p.plan)
}

/// Parse, compile and plan query text; the result also says what kind of
/// value each `:in` parameter takes. Errors are located in `text`, so
/// their `Display` is `line L, column C: message`. Never panics.
pub fn prepare(world: &World, text: &str) -> Result<Prepared, DatalogError> {
    prepare_parsed(world, text, &parse(text)?)
}

/// [`prepare`] for a query already parsed from `text` (so a caller can
/// inspect the [`ast::Query`], e.g. [`ast::Query::attributes`], before
/// compiling it). `q` must be `parse(text)`'s result: errors are located in
/// `text`. Never panics.
pub fn prepare_parsed(world: &World, text: &str, q: &ast::Query) -> Result<Prepared, DatalogError> {
    compile::compile(world, q).map_err(|e| e.locate(text))
}

/// Parse a story-check file (`kala doctor --checks FILE`): exactly one
/// top-level vector of checks `[:name [:find ...]]`, each a keyword name
/// and one query vector, in the query lexer's EDN subset (`;` comments,
/// commas are whitespace). Names are unique.
///
/// The queries are not compiled (that needs a world, see [`prepare`]):
/// each [`Check::query`] is the query vector's exact source text, at most
/// [`MAX_QUERY_BYTES`] and nested at most [`MAX_DEPTH`] deep.
///
/// Limits: [`MAX_CHECKS_BYTES`], [`MAX_CHECKS`] and the per-query ones
/// ([`ErrorKind::Limit`]); every shape error and a duplicate name (located
/// at its second use) are [`ErrorKind::Syntax`]. Never panics; errors are
/// located in `text` (except possibly the size limit).
pub fn parse_checks(text: &str) -> Result<Vec<Check>, DatalogError> {
    checks::parse_checks(text)
}

/// Whether more input could complete the current top-level EDN form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completeness {
    /// The form has no unclosed brackets or strings.
    Complete,
    /// More input is needed (also used for empty input).
    Incomplete,
    /// Input contains a lexical or bracket error. Extending an unfinished
    /// bare token (e.g. `1e` to `1e3`) may still make it valid.
    Invalid,
}

/// Inspect query input using the Datalog lexer without compiling it.
pub fn completeness(text: &str) -> Completeness {
    CompletenessScanner::new().push(text)
}

pub use lex::CompletenessScanner;

mod write;
pub use write::{
    FormKind, WriteTemplate, form_kind, parse_write, parse_writes, prepare_write_forms,
    restore_forms, write_forms,
};
/// Most operations per transaction.
pub const MAX_WRITE_OPS: usize = 256;
/// Most transactions per input file.
pub const MAX_WRITE_FORMS: usize = 256;
