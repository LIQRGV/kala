//! Account administration: credentials enter through a bounded line or hidden prompt.
use crate::{
    cli::{UserArgs, UserCommand, UserCredentials, UserWrite},
    error::CliError,
    escape,
};
use kala_system::{
    auth::Credentials,
    system::{self, Applied, Change, Recovery, SystemError},
};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;

const MAX_PASSWORD_BYTES: usize = 4096;

fn password(options: &UserCredentials) -> Result<Option<Credentials>, CliError> {
    if options.trust {
        return Ok(None);
    }
    let value = if options.password_stdin {
        let mut bytes = Vec::new();
        io::stdin()
            .lock()
            .take((MAX_PASSWORD_BYTES + 2) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| CliError::User("could not read password from stdin".into()))?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        String::from_utf8(bytes).map_err(|_| CliError::User("password must be UTF-8".into()))?
    } else if io::stdin().is_terminal() {
        rpassword::prompt_password("Password: ")
            .map_err(|_| CliError::User("could not read password from terminal".into()))?
    } else {
        return Err(CliError::Usage(
            "use --password-stdin or --trust (passwords are never accepted on argv)".into(),
        ));
    };
    if value.is_empty() || value.len() > MAX_PASSWORD_BYTES {
        return Err(CliError::User(
            "password must contain 1 to 4096 bytes".into(),
        ));
    }
    Ok(Some(Credentials::new(value)))
}

/// The server directory must already exist; `kala user` only ever creates
/// `system.kala` inside it.
pub(crate) fn require_directory(dir: &Path) -> Result<(), CliError> {
    if dir.is_dir() {
        Ok(())
    } else {
        Err(CliError::User(format!(
            "{}: no such directory",
            escape::path(dir)
        )))
    }
}

fn system_error(dir: &Path, e: SystemError) -> CliError {
    match e {
        // Same contract and wording as `kala transact`: a damaged tail may
        // hold an acknowledged write, so it is never cut without the flag.
        SystemError::RecoveryRequired { bytes } => CliError::Store(format!(
            "{}: the last transaction is damaged ({bytes} bytes); opening the world \
             for writing would cut it to a quarantine file. Inspect it with \
             `kala info`, then rerun with --acknowledge-recovery",
            escape::path(&dir.join("system.kala"))
        )),
        SystemError::NoSuchDirectory => {
            CliError::User(format!("{}: no such directory", escape::path(dir)))
        }
        e => CliError::User(e.to_string()),
    }
}

/// Tells the operator on stderr that an acknowledged recovery cut a
/// damaged transaction: a password rotation, revoke or drop in it was
/// rolled back. Prints nothing when nothing damaged was cut. The report
/// holds sizes and paths only, never account data.
fn warn_recovered(dir: &Path, applied: &Applied) {
    let Some(report) = &applied.recovered else {
        return;
    };
    let quarantine = report
        .quarantine
        .as_deref()
        .map_or_else(|| "no quarantine file".to_string(), escape::path);
    // A failed stderr write has nowhere to be reported (see `main::emit`).
    let _ = writeln!(
        io::stderr().lock(),
        "kala: warning: {}: cut {} bytes of a damaged transaction to {quarantine}; \
         account changes in the damaged transaction were not applied; re-run any \
         recent `kala user set/revoke/drop` and check `kala user list`",
        escape::path(&dir.join("system.kala")),
        report.bytes_cut,
    );
}

fn recovery(write: &UserWrite) -> Recovery {
    if write.acknowledge_recovery {
        Recovery::Acknowledge
    } else {
        Recovery::Refuse
    }
}

pub fn run(args: &UserArgs) -> Result<String, CliError> {
    let command = &args.command;
    let dir = match command {
        UserCommand::Add { dir, .. }
        | UserCommand::Set { dir, .. }
        | UserCommand::Grant { dir, .. }
        | UserCommand::Revoke { dir, .. }
        | UserCommand::Drop { dir, .. }
        | UserCommand::List { dir } => dir.as_path(),
    };
    // Checked before a password is read, so a typo costs no prompt.
    require_directory(dir)?;
    let report = match command {
        UserCommand::Add {
            account,
            credentials,
            write,
            ..
        }
        | UserCommand::Set {
            account,
            credentials,
            write,
            ..
        } => {
            let password = password(credentials)?;
            let password = password.as_ref().map(Credentials::expose);
            let change = if matches!(command, UserCommand::Add { .. }) {
                Change::Add { account, password }
            } else {
                Change::Set { account, password }
            };
            system::apply(dir, change, recovery(write))
        }
        UserCommand::Grant {
            account,
            privilege,
            database,
            write,
            ..
        } => system::apply(
            dir,
            Change::Grant {
                account,
                privilege,
                db: database,
            },
            recovery(write),
        ),
        UserCommand::Revoke {
            account,
            privilege,
            database,
            write,
            ..
        } => system::apply(
            dir,
            Change::Revoke {
                account,
                privilege,
                db: database,
            },
            recovery(write),
        ),
        UserCommand::Drop { account, write, .. } => {
            system::apply(dir, Change::Drop { account }, recovery(write))
        }
        UserCommand::List { .. } => {
            let accounts = system::list(dir).map_err(|e| system_error(dir, e))?;
            let mut out = String::new();
            for account in accounts {
                use std::fmt::Write;
                let _ = writeln!(
                    out,
                    "{}@{} {} {}",
                    account.user,
                    account.host,
                    account.plugin,
                    account.grants.join(" ")
                );
            }
            return Ok(out);
        }
    };
    warn_recovered(dir, &report);
    let report = report.result.map_err(|e| system_error(dir, e))?;
    Ok(format!("{report}\n"))
}
