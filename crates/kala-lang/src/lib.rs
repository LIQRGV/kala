//! One interface over the query languages Kala parses.
//!
//! Each language is a [`FrontEnd`] in [`LANGUAGES`]: it parses the text,
//! lets the caller refuse attribute names the parsed query mentions, and
//! compiles to the shared plan. Adapters (the Bolt server, the CLI) select a
//! front-end their own way (a first-line prefix, `--lang`) and map
//! [`LangError`] to their own errors, so adding a language is one more
//! `FrontEnd` and one more table entry.
#![forbid(unsafe_code)]
use std::fmt;

use kala_core::World;
use kala_query::Prepared;

/// What kind of failure a [`LangError`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The text could not be parsed or compiled (limits included).
    Syntax,
    /// The query is well-formed but ill-typed.
    Type,
    /// The parsed query names an attribute the caller refused.
    Refused,
}

/// A front-end failure, neutral to the adapter that reports it.
#[derive(Clone, Debug)]
pub struct LangError {
    /// Syntax, type or refusal.
    pub kind: ErrorKind,
    /// The front-end's own message (`line L, column C: ...`); any input it
    /// echoes is already escaped.
    pub message: String,
}

impl LangError {
    fn refused() -> Self {
        Self {
            kind: ErrorKind::Refused,
            message: "the query names a refused attribute".into(),
        }
    }
}

impl fmt::Display for LangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LangError {}

/// A query language Kala parses: parse, refuse attributes in the AST, compile.
pub trait FrontEnd: Sync {
    /// The language's name (the `--lang` value; also used in logs).
    fn name(&self) -> &'static str;
    /// Whether parameters may be NULL or lists (Cypher), or must be scalars
    /// of their inferred kind (Datalog).
    fn nullable_params(&self) -> bool;
    /// Parses and compiles `text`. `refuse` names attributes that must not be
    /// read: it is checked on the AST before compiling, so it also covers
    /// parts of the query the compiler drops (unused rules, empty matches);
    /// a refused query is [`ErrorKind::Refused`] and is never compiled.
    /// [`refuse_nothing`] accepts every attribute.
    fn prepare(
        &self,
        world: &World,
        text: &str,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, LangError>;
}

/// The `refuse` hook that accepts every attribute name.
pub fn refuse_nothing(_attribute: &str) -> bool {
    false
}

/// Datalog: `:in` parameters, each a scalar of its inferred kind.
pub struct Datalog;

impl FrontEnd for Datalog {
    fn name(&self) -> &'static str {
        "datalog"
    }
    fn nullable_params(&self) -> bool {
        false
    }
    fn prepare(
        &self,
        world: &World,
        text: &str,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, LangError> {
        let syntax = |e: kala_datalog::DatalogError| LangError {
            kind: ErrorKind::Syntax,
            message: e.to_string(),
        };
        let query = kala_datalog::parse(text).map_err(syntax)?;
        if query.attributes().iter().any(|a| refuse(&a.name)) {
            return Err(LangError::refused());
        }
        kala_datalog::prepare_parsed(world, text, &query).map_err(syntax)
    }
}

/// Cypher: `$name` parameters, which may also be NULL or lists.
pub struct Cypher;

impl FrontEnd for Cypher {
    fn name(&self) -> &'static str {
        "cypher"
    }
    fn nullable_params(&self) -> bool {
        true
    }
    fn prepare(
        &self,
        world: &World,
        text: &str,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, LangError> {
        let error = |e: kala_cypher::CypherError| {
            let kind = if e.kind == kala_cypher::ErrorKind::Type {
                ErrorKind::Type
            } else {
                ErrorKind::Syntax
            };
            LangError {
                kind,
                message: e.to_string(),
            }
        };
        let query = kala_cypher::parse(text).map_err(error)?;
        if query.attributes().iter().any(|a| refuse(a)) {
            return Err(LangError::refused());
        }
        kala_cypher::prepare_parsed(world, text, &query).map_err(error)
    }
}

/// Every language, in `--lang` help order. The first is the CLI's default.
pub const LANGUAGES: [&dyn FrontEnd; 2] = [&Datalog, &Cypher];

/// The language names, in the same order as [`LANGUAGES`].
pub const NAMES: [&str; 2] = ["datalog", "cypher"];

/// The front-end with this name, if any.
pub fn front_end(name: &str) -> Option<&'static dyn FrontEnd> {
    LANGUAGES.into_iter().find(|l| l.name() == name)
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, LANGUAGES, NAMES, front_end, refuse_nothing};
    use kala_core::{Schema, World};

    #[test]
    fn names_match_the_front_ends_in_order() {
        let names: Vec<_> = LANGUAGES.iter().map(|l| l.name()).collect();
        assert_eq!(names, NAMES);
    }

    #[test]
    fn every_name_finds_its_front_end_and_others_are_none() {
        for name in NAMES {
            assert_eq!(front_end(name).map(|l| l.name()), Some(name));
        }
        assert!(front_end("gremlin").is_none());
    }

    #[test]
    fn a_refused_attribute_stops_before_compiling() {
        // `nope` is not in the schema: compiling would be a different error.
        let world = World::in_memory(Schema::new()).unwrap();
        for (name, text) in [
            ("datalog", "[:find ?e :where [?e :nope ?v]]"),
            ("cypher", "MATCH (n) WHERE n.nope = 1 RETURN n"),
        ] {
            let l = front_end(name).unwrap();
            let refused = l.prepare(&world, text, &|a| a == "nope").unwrap_err();
            assert_eq!(refused.kind, ErrorKind::Refused, "{name}");
            let unrefused = l.prepare(&world, "(", &refuse_nothing).unwrap_err();
            assert_eq!(unrefused.kind, ErrorKind::Syntax, "{name}");
        }
    }
}
