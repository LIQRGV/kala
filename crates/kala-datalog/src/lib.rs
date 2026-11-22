//! Kala's Datalog front-end: a parser for an EDN subset, a compiler from
//! the parsed query to the shared IR (`kala_query`), and a greedy planner
//! that picks the index each pattern reads.
//!
//! Datalog semantics (results are sets) are decided here; the executor stays
//! language-neutral. Design: docs/slice-6-decisions.md.

#![forbid(unsafe_code)]

pub mod ast;
mod compile;
mod error;
mod lex;
mod parse;
mod plan;

pub use compile::{ParamKind, Prepared};
pub use error::{DatalogError, ErrorKind, Span};

use kala_core::World;
use kala_query::Plan;

/// Largest query text accepted, in bytes. Checked before anything else.
pub const MAX_QUERY_BYTES: usize = 64 * 1024;

/// Deepest bracket nesting accepted (`[`/`(` levels, the outer vector
/// included). Real queries need 3.
pub const MAX_DEPTH: usize = 32;

/// Most patterns in one query, counting those inside `(at T ..)`.
pub const MAX_PATTERNS: usize = 256;

/// Parse Datalog query text into an [`ast::Query`].
///
/// The grammar is an EDN subset:
/// `[:find ?v.. (:in $? ?p..)? (:at T)? (:canon C)? :where clause+]`, with
/// clauses `[e a v]`, `(at T [e a v]..)` and `[(op x y)]`. Names are not
/// resolved here; that is the compiler's job.
///
/// Never panics. Errors carry a [`Span`] (except possibly the size limit)
/// and are located, so their `Display` is `line L, column C: message`.
/// Limits: [`MAX_QUERY_BYTES`], [`MAX_DEPTH`], [`MAX_PATTERNS`]
/// ([`ErrorKind::Limit`]).
pub fn parse(text: &str) -> Result<ast::Query, DatalogError> {
    parse::parse(text)
}

/// Compile a parsed query against `world` and plan it.
///
/// Resolves attribute names (inverse names allowed; `:handle` is the
/// built-in entity <-> handle pseudo-attribute), handle and ID constants,
/// and types every variable and `:in` parameter from its uses. Then a
/// greedy, deterministic planner orders the patterns and picks each one's
/// index (see the crate's `plan` module).
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
    let q = parse(text)?;
    compile::compile(world, &q).map_err(|e| e.locate(text))
}
