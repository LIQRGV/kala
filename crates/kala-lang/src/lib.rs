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
pub mod params;
pub use params::ParamPolicy;

use kala_core::World;
use kala_query::Prepared;

/// What kind of failure a [`LangError`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The text could not be parsed or compiled (limits included).
    Syntax,
    /// A named procedure does not exist.
    Procedure,
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

/// A prepared metadata statement whose rows must come from the server adapter.
#[derive(Clone, Debug)]
pub struct ServerStatement {
    /// Binding rules of the preparing front-end.
    pub parameter_policy: ParamPolicy,
    /// Metadata table the adapter must supply.
    pub request: kala_query::ServerRequest,
    /// Shared plan that projects and filters the supplied table.
    pub prepared: Prepared,
}
pub use kala_query::{DatabaseSelection, ServerRequest};

/// Adapter-level statement classification, before opening a writable handle.
pub enum Statement {
    /// An ordinary query.
    Query,
    /// A server-supplied metadata table.
    Server(Box<ServerStatement>),
    /// One or more Datalog transaction forms, parsed incrementally by `write_forms`.
    Writes,
    /// A parsed statement over the shared write primitives.
    CypherWrite(Box<write::WriteStatement>),
    /// A transaction mode directive, admitted before opening a writer.
    Concurrency(kala_core::Concurrency),
}

/// Classifies prefix-stripped text through its selected front-end.
pub fn classify(
    front_end: &dyn FrontEnd,
    text: &str,
    refuse: &dyn Fn(&str) -> bool,
) -> Result<Statement, LangError> {
    front_end.classify(text, refuse)
}

/// A query language Kala parses: parse, refuse attributes in the AST, compile.
pub trait FrontEnd: Sync {
    /// Classify a complete RUN without opening a world. Read-only front-ends
    /// may supply server metadata through the existing preparation hook.
    fn classify(&self, text: &str, refuse: &dyn Fn(&str) -> bool) -> Result<Statement, LangError> {
        Ok(match self.prepare_server(text, refuse)? {
            Some(statement) => Statement::Server(Box::new(statement)),
            None => Statement::Query,
        })
    }

    /// Recognize and prepare a pure server-metadata pipeline without world I/O.
    /// None means ordinary world preparation. The refusal hook still covers
    /// every parsed expression, even when a projection would drop it.
    fn prepare_server(
        &self,
        _text: &str,
        _refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Option<ServerStatement>, LangError> {
        Ok(None)
    }

    /// The language's name (the `--lang` value; also used in logs).
    fn name(&self) -> &'static str;
    /// Whether parameters may be NULL or lists (Cypher), or must be scalars
    /// of their inferred kind (Datalog).
    fn parameter_policy(&self) -> ParamPolicy;
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
    fn classify(&self, text: &str, _refuse: &dyn Fn(&str) -> bool) -> Result<Statement, LangError> {
        kala_datalog::form_kind(text)
            .map(|kind| match kind {
                kala_datalog::FormKind::Query => Statement::Query,
                kala_datalog::FormKind::Write => Statement::Writes,
            })
            .map_err(|e| LangError {
                kind: ErrorKind::Syntax,
                message: e.to_string(),
            })
    }

    fn name(&self) -> &'static str {
        "datalog"
    }
    fn parameter_policy(&self) -> ParamPolicy {
        ParamPolicy::SCALARS
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

impl Cypher {
    fn server(
        text: &str,
        query: &kala_cypher::ast::Query,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Option<ServerStatement>, LangError> {
        use kala_cypher::ast::Clause;
        let Some(request) = query.server_request().cloned() else {
            return Ok(None);
        };
        if query.attributes().iter().any(|a| refuse(a)) {
            return Err(LangError::refused());
        }
        // A metadata-only authorization exception must never admit world reads
        // or reuse one supplied table for another source.
        let pure = matches!(query.clauses.first(), Some(Clause::Source(s)) if matches!(s.source, kala_query::RowSource::Server(_)))
            && query
                .clauses
                .iter()
                .skip(1)
                .all(|c| matches!(c, Clause::With(..)));
        if !pure {
            return Err(LangError {
                kind: ErrorKind::Syntax,
                message: "server metadata supports only YIELD, WHERE, WITH and RETURN".into(),
            });
        }
        let empty = World::in_memory(kala_core::Schema::new()).map_err(|_| LangError {
            kind: ErrorKind::Type,
            message: "could not initialize metadata schema".into(),
        })?;
        let prepared = kala_cypher::prepare_parsed(&empty, text, query).map_err(cypher_error)?;
        Ok(Some(ServerStatement {
            request,
            prepared,
            parameter_policy: Self.parameter_policy(),
        }))
    }
}

impl FrontEnd for Cypher {
    fn classify(&self, text: &str, refuse: &dyn Fn(&str) -> bool) -> Result<Statement, LangError> {
        match kala_cypher::parse_statement(text).map_err(cypher_error)? {
            kala_cypher::ast::Statement::Write(statement) => {
                if statement.attributes().iter().any(|a| refuse(a)) {
                    return Err(LangError::refused());
                }
                Ok(Statement::CypherWrite(Box::new(statement)))
            }
            kala_cypher::ast::Statement::Concurrency(mode) => Ok(Statement::Concurrency(mode)),
            kala_cypher::ast::Statement::Read(query) => {
                Ok(match Cypher::server(text, &query, refuse)? {
                    Some(statement) => Statement::Server(Box::new(statement)),
                    None => Statement::Query,
                })
            }
        }
    }

    fn prepare_server(
        &self,
        text: &str,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Option<ServerStatement>, LangError> {
        let query = kala_cypher::parse(text).map_err(cypher_error)?;
        Self::server(text, &query, refuse)
    }
    fn name(&self) -> &'static str {
        "cypher"
    }
    fn parameter_policy(&self) -> ParamPolicy {
        ParamPolicy::COLLECTIONS
    }
    fn prepare(
        &self,
        world: &World,
        text: &str,
        refuse: &dyn Fn(&str) -> bool,
    ) -> Result<Prepared, LangError> {
        let query = kala_cypher::parse(text).map_err(cypher_error)?;
        if query.attributes().iter().any(|a| refuse(a)) {
            return Err(LangError::refused());
        }
        kala_cypher::prepare_parsed(world, text, &query).map_err(cypher_error)
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

fn cypher_error(e: kala_cypher::CypherError) -> LangError {
    let kind = match e.kind {
        kala_cypher::ErrorKind::Type => ErrorKind::Type,
        kala_cypher::ErrorKind::Procedure => ErrorKind::Procedure,
        _ => ErrorKind::Syntax,
    };
    LangError {
        kind,
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, LANGUAGES, NAMES, front_end, refuse_nothing};
    use kala_core::{Schema, World};

    #[test]
    fn forms_that_change_existing_schema_or_settings_are_the_admin_ones() {
        use super::write::{parse_write, requires_admin, requires_read};
        for (text, admin, read) in [
            ("[:world-setting :schema :strict]", true, false),
            ("[:rename :a :b]", true, false),
            ("[:deprecate :a]", true, false),
            ("[:cardinality :a :many]", true, false),
            ("[:migrate :a :i64]", true, true),
            (
                "[[:assert :greg :alias \"x\"] [:deprecate :a]]",
                true,
                false,
            ),
            ("[:attribute :a :string]", false, false),
            ("[:node-type :FACTION]", false, false),
            ("[:assert :greg :alias \"x\"]", false, false),
            ("[:retract :greg :alias \"x\"]", false, false),
            ("[:create :CHARACTER :x]", false, false),
            ("[:create :CHARACTER :x :if-missing]", false, true),
            ("[:set :greg :alias \"x\"]", false, true),
            ("[:correct 1 :delete]", false, true),
        ] {
            let form = parse_write(text).unwrap();
            assert_eq!(requires_admin(&form), admin, "{text}");
            assert_eq!(requires_read(&form), read, "{text}");
        }
    }

    #[test]
    fn a_cypher_type_constraint_classifies_as_a_blind_schema_statement() {
        use super::{Statement, classify};
        let cypher = front_end("cypher").unwrap();
        let text = "CREATE CONSTRAINT c IF NOT EXISTS FOR (n:T) REQUIRE n.rank IS :: INTEGER";
        let Ok(Statement::CypherWrite(statement)) = classify(cypher, text, &refuse_nothing) else {
            panic!("expected a write statement")
        };
        assert!(statement.schema_only() && !statement.requires_read());
        // The refusal hook sees the declared name.
        assert!(matches!(
            classify(cypher, text, &|a| a == "rank"),
            Err(e) if e.kind == ErrorKind::Refused
        ));
        for refused in ["DROP CONSTRAINT c", "CREATE INDEX FOR (n:T) ON (n.x)"] {
            assert!(matches!(
                classify(cypher, refused, &refuse_nothing),
                Err(e) if e.kind == ErrorKind::Syntax
            ));
        }
    }

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

/// Shared write entry point; server adapters need no language-crate dependency.
///
/// A form's concurrency mode: [`execute_in`](write::execute_in) takes the
/// caller's default, [`stage`](write::stage) leaves a caller-owned
/// transaction's mode alone, and in both a form that begins with
/// `[:concurrency :weak]` (or `:optimistic`) sets its own.
/// [`commit`](write::commit) reports a conflict at the op that read.
pub mod write {
    /// Whether staging this form reads existing data before writing it.
    pub fn requires_read(form: &WriteTxn) -> bool {
        use kala_datalog::ast::WriteOp;
        form.ops.iter().any(|op| {
            matches!(
                op,
                WriteOp::Set { .. }
                    | WriteOp::Correct { .. }
                    | WriteOp::Migrate { .. }
                    | WriteOp::Create {
                        if_missing: true,
                        ..
                    }
            )
        })
    }
    /// Whether this form changes what the world's existing data or rules
    /// mean, rather than adding to them: a world setting, a rename, a
    /// deprecation, a cardinality change or a migration. Declarations
    /// (`[:attribute …]`, `[:node-type …]`, and names a write declares by
    /// inference) are not: they only add.
    ///
    /// The helper only classifies. Whether such a form needs a higher
    /// privilege is the adapter's policy (the Bolt server asks for `admin`;
    /// the CLI, whose boundary is the file's permissions, asks for nothing).
    pub fn requires_admin(form: &WriteTxn) -> bool {
        use kala_datalog::ast::WriteOp;
        form.ops.iter().any(|op| {
            matches!(
                op,
                WriteOp::WorldSetting { .. }
                    | WriteOp::Rename { .. }
                    | WriteOp::Deprecate { .. }
                    | WriteOp::Cardinality { .. }
                    | WriteOp::Migrate { .. }
            )
        })
    }
    pub use kala_core::Concurrency;
    pub use kala_core::write::{StagedWrite, WriteOperation, WriteReport};
    pub use kala_cypher::write::{
        CompiledWrite, MAX_WRITE_ROWS, WriteError as StatementError, WriteInput,
        compile as compile_statement, execute_in as execute_statement,
        prepare as prepare_statement, run as run_statement, stage as stage_statement,
    };
    pub use kala_cypher::{ast::WriteStatement, statements};
    pub use kala_datalog::apply::{
        Failure, Occupant, OccupiedBy, SameTick, WriteError, WritePolicy, commit, execute,
        execute_in, execute_with, stage, stage_with,
    };
    pub use kala_datalog::ast::WriteTxn;
    pub use kala_datalog::{parse_write, parse_writes, write_forms};
}
