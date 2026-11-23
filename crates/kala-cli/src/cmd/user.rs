//! Account administration: credentials enter through a bounded line or hidden prompt.
use crate::{
    cli::{UserCommand, UserCredentials},
    error::CliError,
};
use kala_bolt::{auth::Credentials, system};
use std::io::{self, BufRead, IsTerminal, Read};

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

pub fn run(command: &UserCommand) -> Result<String, CliError> {
    let result = match command {
        UserCommand::Add {
            dir,
            account,
            credentials,
        }
        | UserCommand::Set {
            dir,
            account,
            credentials,
        } => {
            let password = password(credentials)?;
            let password = password.as_ref().map(Credentials::expose);
            if matches!(command, UserCommand::Add { .. }) {
                system::add(dir, account, password)
            } else {
                system::set(dir, account, password)
            }
        }
        UserCommand::Grant {
            dir,
            account,
            privilege,
            database,
        } => system::grant(dir, account, privilege, database),
        UserCommand::Revoke {
            dir,
            account,
            privilege,
            database,
        } => system::revoke(dir, account, privilege, database),
        UserCommand::Drop { dir, account } => system::drop_account(dir, account),
        UserCommand::List { dir } => {
            let accounts = system::list(dir).map_err(|e| CliError::User(e.to_string()))?;
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
    result.map_err(|e| CliError::User(e.to_string()))?;
    Ok("account updated\n".into())
}
