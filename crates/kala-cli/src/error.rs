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
    /// log, an I/O error other than not-found), or can't be written right
    /// now (its writer lock stayed held, its file was replaced under this
    /// command, its damaged tail needs acknowledging).
    Store(String),
    /// Exit 4: an optimistic commit was refused because another writer
    /// changed something the transaction had read. Nothing was written and
    /// running the same form again is safe.
    Conflict(Box<Conflicted>),
}

/// What [`CliError::Conflict`] reports.
#[derive(Debug)]
pub struct Conflicted {
    /// The whole message, unescaped.
    pub message: String,
    /// The kind of read that was invalidated
    /// ([`kala_core::ConflictKey::kind`]): `entity-attribute`, `handle`, ...
    pub key: &'static str,
    /// The form's op that made the read (1-based position and verb), when
    /// one of its ops did.
    pub op: Option<(usize, &'static str)>,
    /// How many transactions other writers committed while this one ran.
    pub applied: usize,
}

impl CliError {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            CliError::Usage(_) => ExitCode::from(2),
            CliError::User(_) => ExitCode::from(1),
            CliError::Store(_) => ExitCode::from(3),
            CliError::Conflict(_) => ExitCode::from(CONFLICT_EXIT),
        }
    }

    /// The message, unescaped (`main` escapes it once when printing).
    pub fn message(&self) -> &str {
        match self {
            CliError::Usage(m) | CliError::User(m) | CliError::Store(m) => m,
            CliError::Conflict(c) => &c.message,
        }
    }

    /// The same error with its message rewritten (a prefix saying which
    /// op or form failed): the kind, and so the exit code, stays.
    pub fn map_message(self, f: impl FnOnce(&str) -> String) -> CliError {
        match self {
            CliError::Usage(m) => CliError::Usage(f(&m)),
            CliError::User(m) => CliError::User(f(&m)),
            CliError::Store(m) => CliError::Store(f(&m)),
            CliError::Conflict(mut c) => {
                c.message = f(&c.message);
                CliError::Conflict(c)
            }
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

    /// A kala-core error, with `msg` as its message (`<context>: <the
    /// error's own text>`): I/O as [`CliError::io`]; damaged or foreign
    /// logs as store errors; a world that can't be written right now
    /// ([`unwritable`]) as a store error that says, in the CLI's own words,
    /// what happened and what to do; a conflict as [`CliError::Conflict`];
    /// everything else (bad input) as a user error.
    pub fn core(e: &kala_core::Error, msg: String) -> CliError {
        use kala_core::Error as E;
        match e {
            E::Io { kind, .. } => CliError::io(*kind, msg),
            E::BadMagic | E::UnsupportedVersion(_) | E::Corrupt(_) => CliError::Store(msg),
            // kala-core's text already says nothing was written and to run
            // the transaction again. (`kala transact` and the REPL say it in
            // the form's own terms instead: `cmd::transact::conflict_err`.)
            E::Conflict(conflict) => CliError::Conflict(Box::new(Conflicted {
                message: msg,
                key: conflict.key.kind(),
                op: None,
                applied: conflict.applied,
            })),
            _ => match unwritable(e) {
                // The context `msg` begins with is kept; kala-core's text
                // (written for a library caller: "reopen the world") is
                // replaced by the CLI's.
                Some(text) => match msg.strip_suffix(&e.to_string()) {
                    Some(context) => CliError::Store(format!("{context}{text}")),
                    None => CliError::Store(format!("{msg}: {text}")),
                },
                None => CliError::User(msg),
            },
        }
    }
}

/// The exit code of a refused optimistic commit ([`CliError::Conflict`]).
pub const CONFLICT_EXIT: u8 = 4;

/// What every conflict message ends with.
pub const CONFLICT_ADVICE: &str = "nothing was written; run it again";

/// For a kala-core error that means "this world can't be written right
/// now, though nothing is wrong with what you asked": what happened and
/// what to do about it, as the CLI says it. `None` for every other error.
pub fn unwritable(e: &kala_core::Error) -> Option<String> {
    use kala_core::Error as E;
    Some(match e {
        E::WorldLocked => "world is locked by another writer: a commit elsewhere held the \
             writer lock for the whole busy timeout; nothing was written; run the command \
             again, or wait longer with --busy-timeout"
            .to_string(),
        E::LogDiverged => "the world file was cut back, rewritten or replaced by another \
             process (for example `kala load --force`) while this command had it open; \
             nothing was written; run the command again (in a REPL session: quit and \
             start it again)"
            .to_string(),
        E::RecoveryRequired { bytes } => format!(
            "the last transaction is damaged ({bytes} bytes); writing to the world \
             would cut it to a quarantine file; nothing was written. Inspect it with \
             `kala info`, then rerun with --acknowledge-recovery"
        ),
        E::RecoveryUnacknowledged => "the world ends in a damaged last transaction that \
             nobody has acknowledged (it is still in the file, and is left out of the \
             world); nothing was written. Inspect it with `kala info`, then rerun with \
             --acknowledge-recovery, which cuts it to a quarantine file"
            .to_string(),
        _ => return None,
    })
}
