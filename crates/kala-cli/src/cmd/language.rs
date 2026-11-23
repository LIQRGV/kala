//! One interface over the query languages `kala query` / `kala explain`
//! accept, so commands never branch on the language name.
//!
//! Each language is a [`FrontEnd`] in [`LANGUAGES`]: it parses and compiles
//! the text to the shared plan, and parses `--param` texts the way that
//! language types its parameters. `--lang` is restricted to [`NAMES`], so
//! adding a language is one more `FrontEnd` and one more table entry.
use kala_core::World;
use kala_query::{ParamKind, Prepared, QueryParams};

use super::query::{parse_cypher_params, parse_params};
use super::read::Ctx;
use crate::error::CliError;

/// A query language the CLI can prepare.
pub(crate) trait FrontEnd: Sync {
    /// The `--lang` value that selects it.
    fn name(&self) -> &'static str;
    /// Parses and compiles `text` against `world`. Front-end messages escape
    /// the input they echo; `main` escapes the whole line again.
    fn prepare(&self, world: &World, text: &str) -> Result<Prepared, CliError>;
    /// Parses the `--param` texts by the kinds the prepared query declares.
    fn params(
        &self,
        ctx: &Ctx,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, CliError>;
}

/// Datalog: `:in` parameters, each a scalar of its inferred kind.
struct Datalog;

impl FrontEnd for Datalog {
    fn name(&self) -> &'static str {
        "datalog"
    }
    fn prepare(&self, world: &World, text: &str) -> Result<Prepared, CliError> {
        kala_datalog::prepare(world, text).map_err(|e| CliError::User(format!("query: {e}")))
    }
    fn params(
        &self,
        ctx: &Ctx,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, CliError> {
        Ok(parse_params(ctx, kinds, given)?.into())
    }
}

/// Cypher: `$name` parameters, which may also be NULL or lists.
struct Cypher;

impl FrontEnd for Cypher {
    fn name(&self) -> &'static str {
        "cypher"
    }
    fn prepare(&self, world: &World, text: &str) -> Result<Prepared, CliError> {
        kala_cypher::prepare(world, text).map_err(|e| CliError::User(format!("query: {e}")))
    }
    fn params(
        &self,
        ctx: &Ctx,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, CliError> {
        parse_cypher_params(ctx, kinds, given)
    }
}

/// Every language, in `--lang` help order. The first is the default.
pub(crate) const LANGUAGES: [&dyn FrontEnd; 2] = [&Datalog, &Cypher];

/// The `--lang` values, in the same order as [`LANGUAGES`].
pub(crate) const NAMES: [&str; 2] = ["datalog", "cypher"];

/// The front-end for a `--lang` value. clap only accepts [`NAMES`], so an
/// unknown name is a usage error that can't normally be reached.
pub(crate) fn front_end(name: &str) -> Result<&'static dyn FrontEnd, CliError> {
    LANGUAGES
        .into_iter()
        .find(|l| l.name() == name)
        .ok_or_else(|| CliError::Usage(format!("unknown query language `{name}`")))
}

#[cfg(test)]
mod tests {
    use super::{LANGUAGES, NAMES, front_end};

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
