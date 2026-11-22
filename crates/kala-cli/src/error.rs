//! The CLI's error type and its exit codes.

use std::process::ExitCode;

/// Why a command failed, and so which exit code it ends with. Most usage
/// errors (exit 2) come from clap and are handled in `main`; [`CliError::Usage`]
/// is for the argument shapes clap can't check (an `@T` in an optional slot).
#[derive(Debug)]
pub enum CliError {
    /// Exit 2: the command line is malformed (a bad `@T`, two ticks).
    Usage(String),
    /// Exit 1: the user's input is wrong (a bad fixture, a missing file, an
    /// existing world without `--force`, a command not built yet).
    User(String),
    /// Exit 3: the store is broken or unusable (a corrupt log, not a Kala
    /// log, an I/O error other than not-found).
    Store(String),
}

impl CliError {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            CliError::Usage(_) => ExitCode::from(2),
            CliError::User(_) => ExitCode::from(1),
            CliError::Store(_) => ExitCode::from(3),
        }
    }

    /// The message, unescaped (`main` escapes it once when printing).
    pub fn message(&self) -> &str {
        match self {
            CliError::Usage(m) | CliError::User(m) | CliError::Store(m) => m,
        }
    }

    /// An I/O error: not-found is the user's (a wrong path), anything else is
    /// the store's.
    pub fn io(kind: std::io::ErrorKind, msg: String) -> CliError {
        if kind == std::io::ErrorKind::NotFound {
            CliError::User(msg)
        } else {
            CliError::Store(msg)
        }
    }

    /// A kala-core error about the world file at `path`, as
    /// `<path>: <error>` (the path escaped), classified by [`CliError::core`].
    pub fn at_path(path: &std::path::Path, e: &kala_core::Error) -> CliError {
        CliError::core(e, format!("{}: {e}", crate::escape::path(path)))
    }

    /// A kala-core error: I/O as [`CliError::io`], damaged or foreign logs
    /// as store errors, everything else (bad input, a world locked by
    /// another process) as user errors.
    pub fn core(e: &kala_core::Error, msg: String) -> CliError {
        use kala_core::Error as E;
        match e {
            E::Io { kind, .. } => CliError::io(*kind, msg),
            E::BadMagic | E::UnsupportedVersion(_) | E::Corrupt(_) => CliError::Store(msg),
            _ => CliError::User(msg),
        }
    }
}
