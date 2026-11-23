//! Query-language selection and one interface over the query front-ends.
//!
//! [`detect`] reads the optional first-line prefix once (`// kala:datalog`,
//! `// kala:cypher`, `// kala:gremlin`; anything else is Cypher, as drivers
//! assume) and returns the [`FrontEnd`] with the text it should parse. Every
//! front-end is then driven the same way by `query::run`.
use std::borrow::Cow;

use crate::error::{Failure, SYNTAX, TYPE};
use kala_core::World;
use kala_query::Prepared;

/// A query language Kala parses: parse, refuse secrets in the AST, compile.
pub(crate) trait FrontEnd: Sync {
    /// The language's name, for logs.
    fn name(&self) -> &'static str;
    /// Whether parameters may be NULL or lists (Cypher), or must be scalars
    /// of their inferred kind (Datalog).
    fn nullable_params(&self) -> bool;
    /// Parses and compiles `text`. `secret` names attributes that must not be
    /// read: it is checked on the AST before compiling, so it also covers
    /// parts of the query the compiler drops (unused rules, empty matches).
    /// The caller checks the compiled plan as well.
    fn prepare(
        &self,
        world: &World,
        text: &str,
        secret: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, Failure>;
}

/// The front-end a query selected, and the text to give it.
pub(crate) struct Source<'a> {
    pub front_end: &'static dyn FrontEnd,
    pub text: Cow<'a, str>,
}

/// Selects the language from the query's first line. A recognised prefix is
/// replaced by an empty line, so error positions still count the lines the
/// client sent; without one the whole text is Cypher (its `//` comments
/// included). Gremlin is refused here, before anything is parsed.
pub(crate) fn detect(text: &str) -> Result<Source<'_>, Failure> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let front_end: &'static dyn FrontEnd = match first.trim_end_matches([' ', '\r']) {
        "// kala:gremlin" => return Err(Failure::new(SYNTAX, "Gremlin is not supported yet")),
        "// kala:datalog" => &Datalog,
        "// kala:cypher" => &Cypher,
        _ => {
            return Ok(Source {
                front_end: &Cypher,
                text: Cow::Borrowed(text),
            });
        }
    };
    Ok(Source {
        front_end,
        text: Cow::Owned(format!("\n{rest}")),
    })
}

struct Datalog;

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
        secret: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, Failure> {
        let syntax = |e: kala_datalog::DatalogError| Failure::new(SYNTAX, e.to_string());
        let query = kala_datalog::parse(text).map_err(syntax)?;
        if query.attributes().iter().any(|a| secret(&a.name)) {
            return Err(crate::query::protected());
        }
        kala_datalog::prepare_parsed(world, text, &query).map_err(syntax)
    }
}

struct Cypher;

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
        secret: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, Failure> {
        let error = |e: kala_cypher::CypherError| {
            let code = if e.kind == kala_cypher::ErrorKind::Type {
                TYPE
            } else {
                SYNTAX
            };
            Failure::new(code, e.to_string())
        };
        let query = kala_cypher::parse(text).map_err(error)?;
        if query.attributes().iter().any(|a| secret(a)) {
            return Err(crate::query::protected());
        }
        kala_cypher::prepare_parsed(world, text, &query).map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(text: &str) -> (&'static str, String) {
        let s = detect(text).unwrap_or_else(|f| panic!("{text:?}: {f:?}"));
        (s.front_end.name(), s.text.into_owned())
    }

    #[test]
    fn a_prefix_selects_its_language_and_keeps_its_line() {
        assert_eq!(
            pick("// kala:datalog\n[:find ?a]"),
            ("datalog", "\n[:find ?a]".into())
        );
        assert_eq!(
            pick("// kala:datalog  \r\n[:find ?a]"),
            ("datalog", "\n[:find ?a]".into())
        );
        assert_eq!(
            pick("// kala:cypher\nRETURN 1"),
            ("cypher", "\nRETURN 1".into())
        );
        assert_eq!(pick("// kala:datalog"), ("datalog", "\n".into()));
    }

    #[test]
    fn no_or_unknown_prefix_is_cypher_with_the_whole_text() {
        for text in [
            "MATCH (n) RETURN n",
            "// a comment\nMATCH (n) RETURN n",
            "// kala:sql\nx",
            "",
        ] {
            assert_eq!(pick(text), ("cypher", text.into()));
        }
    }

    #[test]
    fn gremlin_is_refused_before_parsing() {
        let Err(f) = detect("// kala:gremlin\ng.V()") else {
            panic!("Gremlin must be refused");
        };
        assert!(format!("{f:?}").contains("Gremlin is not supported yet"));
    }

    #[test]
    fn only_cypher_takes_nullable_parameters() {
        assert!(
            pick("RETURN 1").0 == "cypher"
                && detect("RETURN 1").unwrap().front_end.nullable_params()
        );
        assert!(
            !detect("// kala:datalog\n[]")
                .unwrap()
                .front_end
                .nullable_params()
        );
    }
}
