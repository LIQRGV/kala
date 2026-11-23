//! The command line's shape (clap derive). Only argument shapes live here;
//! what each command does is in `cmd/`.
//!
//! Security conditions (slice 5): no argfile expansion, no env-var arguments,
//! no `allow_hyphen_values` / `trailing_var_arg`; negative ticks go through
//! `--at` with `allow_negative_numbers`.

// The doc comments on the items below are the `--help` text clap prints, not
// rustdoc: `<database>` and `[IPv6]` are meant literally.
#![allow(rustdoc::broken_intra_doc_links, rustdoc::invalid_html_tags)]

use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

/// Kala: a temporal graph database for story and game worlds.
#[derive(Debug, Parser)]
#[command(name = "kala", version, propagate_version = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// A canon (author-timeline position): `tx#N` or `N`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canon(pub u64);

pub(crate) fn parse_canon(s: &str) -> Result<Canon, String> {
    let digits = s.strip_prefix("tx#").unwrap_or(s);
    digits
        .parse::<u64>()
        .map(Canon)
        .map_err(|_| "expected `tx#N` or `N` (N a non-negative integer)".to_string())
}

/// The tick and canon options shared by point-in-time reads.
#[derive(Debug, Args)]
pub struct ReadAt {
    /// Read at this story tick (default: the latest tick); same as `@T`.
    #[arg(long = "at", value_name = "T", value_parser = clap::value_parser!(i64), allow_negative_numbers = true)]
    pub at: Option<i64>,
    /// Read at this canon: `tx#N` or `N` (default: the latest).
    #[arg(long, value_name = "C", value_parser = parse_canon)]
    pub canon: Option<Canon>,
    /// Print one JSON document instead of text.
    #[arg(long)]
    pub json: bool,
}

/// A query's text (inline or from a file) and its parameters.
#[derive(Debug, Args)]
#[command(group(ArgGroup::new("source").required(true).args(["query", "file"])))]
pub struct QueryArgs {
    /// Query language (the REPL remains Datalog-only).
    #[arg(
        long,
        default_value = crate::cmd::language::NAMES[0],
        value_parser = clap::builder::PossibleValuesParser::new(crate::cmd::language::NAMES)
    )]
    pub lang: String,
    /// The query text.
    pub query: Option<String>,
    /// Read the query text from this file (a regular file, UTF-8).
    #[arg(short = 'f', value_name = "FILE")]
    pub file: Option<PathBuf>,
    /// A query parameter, `name=value` (`name` with or without `?`).
    #[arg(long = "param", value_name = "NAME=VALUE", value_parser = parse_param)]
    pub params: Vec<(String, String)>,
}

/// `name=value`, split at the first `=`; the name may start with `?`.
fn parse_param(s: &str) -> Result<(String, String), String> {
    let (name, value) = s
        .split_once('=')
        .ok_or_else(|| "expected `name=value`".to_string())?;
    Ok((name.to_string(), value.to_string()))
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Write a replayable world script.
    Dump {
        world: PathBuf,
        #[arg(short = 'o')]
        output: Option<PathBuf>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        force: bool,
    },
    /// Create an empty world, optionally declaring its genesis schema.
    Create {
        world: PathBuf,
        #[arg(long)]
        schema: Option<PathBuf>,
    },
    /// Export a JSON snapshot at a tick and canon.
    Export {
        world: PathBuf,
        #[arg(long, allow_negative_numbers = true)]
        at: Option<i64>,
        #[arg(long, value_parser = parse_canon)]
        canon: Option<Canon>,
        #[arg(short = 'o')]
        output: Option<PathBuf>,
    },
    /// Show schema definitions and settings at a canon.
    Schema {
        world: PathBuf,
        #[arg(long, value_name="C", value_parser=parse_canon)]
        canon: Option<Canon>,
        #[arg(long)]
        json: bool,
    },
    /// Serve read-only Cypher and Datalog queries over Bolt 5.
    Serve {
        /// Directory containing <database>.kala world files.
        dir: PathBuf,
        /// IP:port, [IPv6]:port, or localhost:port.
        #[arg(long, default_value = "127.0.0.1:7687")]
        listen: String,
        /// Authentication policy (accounts by default; none is loopback-only).
        #[arg(long, default_value = "accounts", value_parser = ["accounts", "none"])]
        auth: String,
        /// Allow passwords over plaintext on a non-loopback listener.
        #[arg(long)]
        insecure_allow_plaintext: bool,
        #[arg(long)]
        default_db: Option<String>,
        /// Maximum simultaneous client connections.
        #[arg(long, default_value_t = 64)]
        max_connections: usize,
        /// Seconds allowed to receive a complete message.
        #[arg(long, default_value_t = 600)]
        idle_timeout: u64,
    },
    /// Manage the server directory's accounts and database privileges.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Build a world file from a YAML fixture.
    Load {
        /// The YAML fixture to load.
        fixture: PathBuf,
        /// The world file to create.
        world: PathBuf,
        /// Replace the world file if it exists.
        #[arg(long)]
        force: bool,
    },
    /// An entity's values at a tick (every attribute, or one).
    Asof {
        /// The world file.
        world: PathBuf,
        /// A handle (`edwin`), an ID (`CHARACTER#1`) or an edge (`edge#9`).
        entity: String,
        /// The attribute (default: every attribute holding a value).
        attr: Option<String>,
        /// The tick, as `@T`.
        #[arg(value_name = "@T")]
        tick: Option<String>,
        #[command(flatten)]
        read: ReadAt,
    },
    /// An entity's neighbors through a ref attribute at a tick.
    Neighbors {
        /// The world file.
        world: PathBuf,
        /// A handle (`edwin`), an ID (`CHARACTER#1`) or an edge (`edge#9`).
        entity: String,
        /// The attribute.
        attr: String,
        /// The tick, as `@T`.
        #[arg(value_name = "@T")]
        tick: Option<String>,
        /// Follow incoming edges.
        #[arg(long = "in", conflicts_with_all = ["out", "both"])]
        incoming: bool,
        /// Follow outgoing edges.
        #[arg(long, conflicts_with = "both")]
        out: bool,
        /// Follow edges both ways.
        #[arg(long)]
        both: bool,
        #[command(flatten)]
        read: ReadAt,
    },
    /// Every change point of an entity's attribute.
    History {
        /// The world file.
        world: PathBuf,
        /// A handle (`edwin`), an ID (`CHARACTER#1`) or an edge (`edge#9`).
        entity: String,
        /// The attribute.
        attr: String,
        /// Read at this canon: `tx#N` or `N` (default: the latest).
        #[arg(long, value_name = "C", value_parser = parse_canon)]
        canon: Option<Canon>,
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Entities whose attribute holds a value at a tick.
    Find {
        /// The world file.
        world: PathBuf,
        /// The attribute.
        attr: String,
        /// The value to look for.
        value: String,
        /// The tick, as `@T`.
        #[arg(value_name = "@T")]
        tick: Option<String>,
        /// Case-insensitive match.
        #[arg(short = 'i')]
        ignore_case: bool,
        #[command(flatten)]
        read: ReadAt,
    },
    /// Every entity of a node type at a tick.
    List {
        /// The world file.
        world: PathBuf,
        /// The node type (`CHARACTER`).
        #[arg(value_name = "NODE_TYPE")]
        node_type: String,
        /// The tick, as `@T`.
        #[arg(value_name = "@T")]
        tick: Option<String>,
        #[command(flatten)]
        read: ReadAt,
    },
    /// An edge's triple and its properties at a tick.
    Edge {
        /// The world file.
        world: PathBuf,
        /// `edge#N`, or `SOURCE ATTR TARGET`, optionally followed by `@T`.
        #[arg(value_name = "EDGE", required = true, num_args = 1..=4)]
        edge: Vec<String>,
        #[command(flatten)]
        read: ReadAt,
    },
    /// Check the world's integrity (read-only).
    Doctor {
        /// The world file.
        world: PathBuf,
        /// Check schema rules at this canon: `tx#N` or `N`.
        #[arg(long, value_name = "C", value_parser = parse_canon)]
        canon: Option<Canon>,
        /// Check only what changed since this canon: `tx#N` or `N`.
        #[arg(long, value_name = "C", value_parser = parse_canon)]
        since: Option<Canon>,
        /// Also run the story checks in this file (`[[:name [:find ...]] ...]`);
        /// each one returning rows is a hint.
        #[arg(long, value_name = "FILE")]
        checks: Option<PathBuf>,
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// The log's transactions in order.
    Log {
        /// The world file.
        world: PathBuf,
        /// Start at this transaction: `tx#N` or `N`.
        #[arg(long, value_name = "C", value_parser = parse_canon)]
        from: Option<Canon>,
        /// Show at most N transactions.
        #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64))]
        limit: Option<u64>,
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Format version, latest tx and tick, counts, recovery report.
    Info {
        /// The world file.
        world: PathBuf,
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run a Datalog query.
    Query {
        /// The world file.
        world: PathBuf,
        #[command(flatten)]
        query: QueryArgs,
        /// Print one JSON document instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Commit Datalog write transactions from a file or stdin (-).
    Transact {
        /// Replay a dump into a new world, preserving its genesis.
        #[arg(long, conflicts_with_all = ["dry_run", "acknowledge_recovery"])]
        create: bool,
        world: PathBuf,
        input: PathBuf,
        /// Preview the forms in order (each sees the ones before it)
        /// without changing the log.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
        /// Accept cutting a damaged last transaction (see `kala info`) to a
        /// quarantine file before writing.
        #[arg(long)]
        acknowledge_recovery: bool,
    },
    /// Ask Datalog questions, optionally holding the world open for writing.
    Repl {
        /// Enable writes and hold the world's exclusive writer lock.
        #[arg(long, conflicts_with = "canon")]
        write: bool,
        /// The world file.
        world: PathBuf,
        /// Initial default story tick.
        #[arg(long, value_name = "T", allow_negative_numbers = true)]
        at: Option<i64>,
        /// Initial default canon: `tx#N` or `N`.
        #[arg(long, value_name = "C", value_parser = parse_canon)]
        canon: Option<Canon>,
        /// Opt in to private, bounded query history at this path.
        #[arg(long, value_name = "FILE")]
        history: Option<PathBuf>,
        /// With --write: accept cutting a damaged last transaction (see
        /// `kala info`) to a quarantine file.
        #[arg(long, requires = "write")]
        acknowledge_recovery: bool,
    },
    /// Print a Datalog query's plan without running it.
    Explain {
        /// The world file.
        world: PathBuf,
        #[command(flatten)]
        query: QueryArgs,
    },
}

/// Authentication input is never taken from argv.
#[derive(Debug, Args)]
pub struct UserCredentials {
    /// Read exactly one password line from stdin.
    #[arg(long, conflicts_with = "trust")]
    pub password_stdin: bool,
    /// Trust a loopback-only account without a password.
    #[arg(long)]
    pub trust: bool,
}

/// Recovery handling for the `kala user` write subcommands.
#[derive(Debug, Args)]
pub struct UserWrite {
    /// Accept cutting a damaged last transaction of system.kala (see
    /// `kala info`) to a quarantine file, then apply the change.
    #[arg(long)]
    pub acknowledge_recovery: bool,
}

/// Local administrative operations, serialized by the system world's writer lock.
#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Create an account (a TTY prompts without echo if no plugin flag is given).
    Add {
        dir: PathBuf,
        account: String,
        #[command(flatten)]
        credentials: UserCredentials,
        #[command(flatten)]
        write: UserWrite,
    },
    /// Change an account's authentication plugin or password.
    Set {
        dir: PathBuf,
        account: String,
        #[command(flatten)]
        credentials: UserCredentials,
        #[command(flatten)]
        write: UserWrite,
    },
    /// Grant read, write, backup or admin on a database (or *).
    Grant {
        dir: PathBuf,
        account: String,
        privilege: String,
        database: String,
        #[command(flatten)]
        write: UserWrite,
    },
    /// Revoke a grant; the last admin:* account is protected.
    Revoke {
        dir: PathBuf,
        account: String,
        privilege: String,
        database: String,
        #[command(flatten)]
        write: UserWrite,
    },
    /// Drop an account while preserving its audit history.
    Drop {
        dir: PathBuf,
        account: String,
        #[command(flatten)]
        write: UserWrite,
    },
    /// List accounts, plugins and grants (never password hashes).
    List { dir: PathBuf },
}
