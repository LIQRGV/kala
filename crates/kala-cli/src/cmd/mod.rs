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
pub(crate) mod language;
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
        Command::Dump(args) => dump::run(args),
        Command::Create(args) => create::run(args),
        Command::Export(args) => export::run(args),
        Command::Schema(args) => schema::run(args),
        Command::Serve(args) => serve::run(args),
        Command::User(args) => user::run(args),
        Command::Load(args) => load::run(args),
        Command::Asof(args) => asof::run(args),
        Command::Neighbors(args) => neighbors::run(args),
        Command::History(args) => history::run(args),
        Command::Find(args) => find::run(args),
        Command::List(args) => list::run(args),
        Command::Edge(args) => edge::run(args),
        Command::Log(args) => log::run(args),
        Command::Info(args) => info::run(args),
        Command::Query(args) => query::run(args),
        Command::Explain(args) => query::explain(args),
        Command::Transact(args) if args.create => create::replay(args),
        // These report findings or an exit status of their own.
        Command::Transact(args) => return transact::run(args),
        Command::Repl(args) => return repl::run(args),
        Command::Doctor(args) => return doctor::run(args),
    };
    text.map(Done::from)
}
