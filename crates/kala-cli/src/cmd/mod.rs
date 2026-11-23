//! One module per subcommand. Each returns the text for stdout, or a
//! [`CliError`]; `main` prints and picks the exit code.

pub mod asof;
pub mod create;
pub mod doctor;
pub mod dump;
pub mod edge;
pub mod export;
pub mod find;
pub mod history;
pub mod info;
pub mod list;
pub mod load;
pub mod log;
pub mod neighbors;
pub mod query;
mod read;
pub mod repl;
mod repl_editor;
mod repl_history;
pub mod schema;
pub mod serve;
pub mod transact;
mod transfer;
pub mod user;

use crate::cli::Command;
use crate::error::CliError;

/// What a command that ran prints on stdout, and whether it found
/// error-severity findings (`kala doctor`: exit 1 with its report).
pub struct Done {
    pub text: String,
    pub findings: bool,
}

impl From<String> for Done {
    fn from(text: String) -> Done {
        Done {
            text,
            findings: false,
        }
    }
}

/// Runs a parsed command.
pub fn dispatch(cmd: &Command) -> Result<Done, CliError> {
    let text = match cmd {
        Command::Dump {
            world,
            output,
            all,
            force,
        } => dump::run(world, output.as_deref(), *all, *force),
        Command::Create { world, schema } => create::run(world, schema.as_deref()),
        Command::Export {
            world,
            at,
            canon,
            output,
        } => export::run(world, *at, *canon, output.as_deref()),
        Command::Schema { world, canon, json } => schema::run(world, *canon, *json),
        Command::Serve {
            dir,
            listen,
            auth,
            insecure_allow_plaintext,
            default_db,
            max_connections,
            idle_timeout,
        } => serve::run(
            dir,
            listen,
            auth,
            *insecure_allow_plaintext,
            default_db.clone(),
            *max_connections,
            *idle_timeout,
        ),
        Command::User { command } => user::run(command),
        Command::Load {
            fixture,
            world,
            force,
        } => load::run(fixture, world, *force),
        Command::Asof {
            world,
            entity,
            attr,
            tick,
            read,
        } => asof::run(world, entity, attr.as_deref(), tick.as_deref(), read),
        Command::Neighbors {
            world,
            entity,
            attr,
            tick,
            incoming,
            out: _,
            both,
            read,
        } => neighbors::run(
            world,
            entity,
            attr,
            tick.as_deref(),
            neighbors::Flags {
                incoming: *incoming,
                both: *both,
            },
            read,
        ),
        Command::History {
            world,
            entity,
            attr,
            canon,
            json,
        } => history::run(world, entity, attr, *canon, *json),
        Command::Find {
            world,
            attr,
            value,
            tick,
            ignore_case,
            read,
        } => find::run(world, attr, value, tick.as_deref(), *ignore_case, read),
        Command::List {
            world,
            node_type,
            tick,
            read,
        } => list::run(world, node_type, tick.as_deref(), read),
        Command::Edge { world, edge, read } => edge::run(world, edge, read),
        Command::Log {
            world,
            from,
            limit,
            json,
        } => log::run(world, *from, *limit, *json),
        Command::Info { world, json } => info::run(world, *json),
        Command::Query { world, query, json } => query::run(world, query, *json),
        Command::Explain { world, query } => query::explain(world, query),
        Command::Transact {
            create,
            world,
            input,
            dry_run,
            json,
            acknowledge_recovery,
        } => {
            if *create {
                return create::replay(world, input, *json).map(Done::from);
            }
            return transact::run(world, input, *dry_run, *json, *acknowledge_recovery);
        }
        Command::Repl {
            write,
            world,
            at,
            canon,
            history,
            acknowledge_recovery,
        } => {
            return repl::run(
                world,
                *at,
                *canon,
                history.as_deref(),
                *write,
                *acknowledge_recovery,
            );
        }
        Command::Doctor {
            world,
            canon,
            since,
            checks,
            json,
        } => {
            return doctor::run(
                world,
                doctor::Options {
                    canon: *canon,
                    since: *since,
                    checks: checks.as_deref(),
                    json: *json,
                },
            );
        }
    };
    text.map(Done::from)
}
