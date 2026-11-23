//! Located front-end errors, safe to display on the wire.
use std::fmt;
/// Syntax, type or input-limit error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Syntax,
    Type,
    Limit,
}
/// A source position and an escaped, value-independent diagnostic.
#[derive(Clone, Debug)]
pub struct CypherError {
    pub kind: ErrorKind,
    pub offset: usize,
    pub line: usize,
    pub column: usize,
    pub message: String,
}
impl CypherError {
    pub(crate) fn new(
        text: &str,
        offset: usize,
        kind: ErrorKind,
        message: impl Into<String>,
    ) -> Self {
        let offset = offset.min(text.len());
        let prefix = text.get(..offset).unwrap_or("");
        Self {
            kind,
            offset,
            line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
            message: message.into(),
        }
    }
}
impl fmt::Display for CypherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {}: {}",
            self.line,
            self.column,
            self.message.escape_debug()
        )
    }
}
impl std::error::Error for CypherError {}
