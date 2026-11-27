//! Query-language selection and one interface over the query front-ends.
//!
//! [`detect`] reads the optional first-line prefix once (`// kala:datalog`,
//! `// kala:cypher`, `// kala:gremlin`; anything else is Cypher, as drivers
//! assume) and returns the [`FrontEnd`] (kala-lang) with the text it should
//! parse. Every front-end is then driven the same way by `query::run`, and
//! its errors become Bolt failures here.
use std::borrow::Cow;

use crate::error::{Failure, SYNTAX, TYPE};
use kala_lang::{Cypher, Datalog, ErrorKind, FrontEnd, LangError};

/// A front-end error on the wire: a syntax or type failure with the
/// front-end's message, or the protected-attribute refusal (the hook passed
/// to [`FrontEnd::prepare`] names the secret attributes).
impl From<LangError> for Failure {
    fn from(e: LangError) -> Self {
        match e.kind {
            ErrorKind::Procedure => {
                Failure::new("Neo.ClientError.Procedure.ProcedureNotFound", e.message)
            }
            ErrorKind::Syntax => Failure::new(SYNTAX, e.message),
            ErrorKind::Type => Failure::new(TYPE, e.message),
            ErrorKind::Refused => crate::query::protected(),
        }
    }
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
