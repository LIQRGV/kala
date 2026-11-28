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
use crate::escape;
use kala_lang::params::BindError;

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
    /// Bind the parameters used by one statement in a transaction file.
    pub(crate) fn write_params(
        &self,
        world: &World,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, CliError> {
        self.0
            .parameter_policy()
            .bind_write_text(world, kinds, given)
            .map_err(|e| self.bind_error(e, given))
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
            .map_err(|e| self.bind_error(e, given))
    }
    /// A binding error as the author reads it. The shared seam names the
    /// parameter and the kind its position takes, never the value (over
    /// Bolt it may be a secret). Here the author typed the value, so a
    /// refused scalar text is quoted back, as the slice-5 arguments of that
    /// kind are.
    fn bind_error(&self, error: BindError, given: &[(String, String)]) -> CliError {
        use kala_core::ValueType;
        let policy = self.0.parameter_policy();
        let BindError::Type { name, kind } = &error else {
            return CliError::User(error.to_string());
        };
        let bare = |n: &str| n.trim_start_matches(['?', '$']).to_owned();
        let text = given
            .iter()
            .find(|(n, _)| bare(n) == bare(name))
            .map(|(_, text)| text)
            // A list or a quoted JSON text is not one scalar to quote back.
            .filter(|text| !policy.text_is_json(kind, text));
        let Some(text) = text else {
            return CliError::User(error.to_string());
        };
        let name = escape::value(name);
        let text = escape::value(text);
        CliError::User(match kind {
            ParamKind::Entity | ParamKind::Value(ValueType::Ref) => format!(
                "unknown entity `{text}`: not a handle or an ID of this world (parameter `{name}`)"
            ),
            ParamKind::Tick | ParamKind::Canon => format!(
                "`{text}` is not {} for parameter `{name}`",
                kala_lang::params::expected(kind)
            ),
            _ => format!(
                "`{text}` is not {} (the type of parameter `{name}`)",
                kala_lang::params::expected(kind)
            ),
        })
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
