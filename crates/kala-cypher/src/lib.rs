//! Bounded Cypher read front-end, compiling to Kala's shared logical IR.
pub mod ast;
mod compile;
mod error;
mod lex;
mod parse;
pub mod write;
pub use error::{CypherError, ErrorKind};
pub use kala_query::{ParamKind, Prepared};
pub use lex::statements;
/// Maximum UTF-8 query size, matching Datalog.
pub const MAX_QUERY_BYTES: usize = 64 * 1024;
/// Maximum expression nesting.
pub const MAX_DEPTH: usize = 32;
/// Maximum pattern and expression elements.
pub const MAX_PATTERNS: usize = 256;
/// Parse a supported Cypher read query.
pub fn parse(text: &str) -> Result<ast::Query, CypherError> {
    match parse_statement(text)? {
        ast::Statement::Read(query) => Ok(query),
        _ => Err(CypherError::new(
            text,
            0,
            ErrorKind::Syntax,
            "write statements require kala transact --lang cypher",
        )),
    }
}
/// Parse one complete statement before opening a writer or checking privileges.
pub fn parse_statement(text: &str) -> Result<ast::Statement, CypherError> {
    let mut stmt = parse::parse(text)?;
    ast::row_params::resolve(&mut stmt);
    Ok(stmt)
}
/// Parse and compile against the world's schema.
pub fn prepare(world: &kala_core::World, text: &str) -> Result<Prepared, CypherError> {
    prepare_parsed(world, text, &parse(text)?)
}
/// Compile an inspected AST (e.g. after a secret-attribute check).
pub fn prepare_parsed(
    world: &kala_core::World,
    text: &str,
    q: &ast::Query,
) -> Result<Prepared, CypherError> {
    compile::compile(world, text, q)
}
