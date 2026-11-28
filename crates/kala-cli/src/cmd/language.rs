//! One interface over the query languages `kala query` / `kala explain`
//! accept, so commands never branch on the language name.
//!
//! The languages are kala-lang's [`FrontEnd`]s (`kala_lang::LANGUAGES`); `--lang` is
//! restricted to [`NAMES`]. A [`Language`] is one of them as the CLI drives
//! it: it parses and compiles the text to the shared plan, and parses
//! `--param` texts the way that language types its parameters.
use kala_core::World;
use kala_lang::FrontEnd;
pub(crate) use kala_lang::NAMES;
use kala_query::{ParamKind, Prepared, QueryParams};

use crate::error::CliError;

/// A query language the CLI can prepare: a shared front-end plus the CLI's
/// own error mapping and `--param` parsing.
#[derive(Clone, Copy)]
pub(crate) struct Language(&'static dyn FrontEnd);

impl Language {
    /// The `--lang` value that selects it.
    #[cfg(test)]
    pub(crate) fn name(&self) -> &'static str {
        self.0.name()
    }
    /// Parses and compiles `text` against `world`. Front-end messages escape
    /// the input they echo; `main` escapes the whole line again.
    pub(crate) fn prepare(&self, world: &World, text: &str) -> Result<Prepared, CliError> {
        if self
            .0
            .prepare_server(text, &kala_lang::refuse_nothing)
            .map_err(|e| CliError::User(format!("query: {e}")))?
            .is_some()
        {
            return Err(CliError::User(
                "query: server metadata is available only over Bolt".into(),
            ));
        }
        self.0
            .prepare(world, text, &kala_lang::refuse_nothing)
            .map_err(|e| CliError::User(format!("query: {e}")))
    }
    /// Parses the `--param` texts by the kinds the prepared query declares:
    /// each a scalar of its inferred kind, or (where the language's
    /// parameters are nullable) also NULL or a list.
    pub(crate) fn params(
        &self,
        world: &World,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, CliError> {
        self.0
            .parameter_policy()
            .bind_text(world, kinds, given)
            .map_err(|e| CliError::User(e.to_string()))
    }
}

/// The language for a `--lang` value. clap only accepts [`NAMES`], so an
/// unknown name is a usage error that can't normally be reached.
pub(crate) fn front_end(name: &str) -> Result<Language, CliError> {
    kala_lang::front_end(name)
        .map(Language)
        .ok_or_else(|| CliError::Usage(format!("unknown query language `{name}`")))
}

#[cfg(test)]
mod tests {
    use super::{NAMES, front_end};
    use kala_lang::LANGUAGES;

    #[test]
    fn names_match_the_front_ends_in_order() {
        let names: Vec<_> = LANGUAGES.iter().map(|l| l.name()).collect();
        assert_eq!(names, NAMES);
    }

    #[test]
    fn every_name_finds_its_front_end_and_others_are_refused() {
        for name in NAMES {
            assert_eq!(front_end(name).map(|l| l.name()).ok(), Some(name));
        }
        assert!(front_end("gremlin").is_err());
    }
}
