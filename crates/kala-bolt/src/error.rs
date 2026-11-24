//! Wire errors and terminal-safe server diagnostics.
use crate::packstream::Value;
use std::fmt::Write;
pub(crate) const INVALID: &str = "Neo.ClientError.Request.Invalid";
pub(crate) const SYNTAX: &str = "Neo.ClientError.Statement.SyntaxError";
pub(crate) const TYPE: &str = "Neo.ClientError.Statement.TypeError";
pub(crate) const MISSING: &str = "Neo.ClientError.Statement.ParameterMissing";
pub(crate) const EXECUTION: &str = "Neo.ClientError.Statement.ExecutionFailed";
pub(crate) const FORBIDDEN: &str = "Neo.ClientError.Security.Forbidden";
pub(crate) const DATABASE: &str = "Neo.ClientError.Database.DatabaseNotFound";
pub(crate) const STORE: &str = "Neo.DatabaseError.General.UnknownError";
#[derive(Debug)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub message: String,
}
impl Failure {
    pub fn new(code: &'static str, message: impl AsRef<str>) -> Self {
        Self {
            code,
            message: escape(message.as_ref()),
        }
    }
    pub fn invalid() -> Self {
        Self::new(INVALID, "invalid Bolt message or session state")
    }
    /// `imp_user` on ROUTE or RUN: Kala has no impersonation, and running
    /// the request with the connection's own grants would fail open.
    pub fn impersonation() -> Self {
        Self::new(FORBIDDEN, "impersonation is not supported")
    }
    pub fn value(self) -> Value {
        Value::Structure(
            0x7f,
            vec![Value::Map(
                [
                    ("code".into(), Value::String(self.code.into())),
                    ("message".into(), Value::String(self.message)),
                ]
                .into(),
            )],
        )
    }
    pub fn store(e: impl std::fmt::Display) -> Self {
        eprintln!("event=bolt_store_error detail={:?}", escape(&e.to_string()));
        Self::new(STORE, "the world could not be read")
    }
}
pub(crate) fn escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().take(2048) {
        if kala_core::must_escape(c) {
            if c.is_control() {
                let _ = write!(out, "{}", c.escape_debug());
            } else {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
        } else {
            out.push(c);
        }
    }
    if s.chars().nth(2048).is_some() {
        out.push_str("...");
    }
    out
}
