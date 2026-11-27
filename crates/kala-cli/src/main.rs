//! The `kala` command line: a thin layer over the public kala-core API.
//!
//! Exit codes: 0 ok; 1 user error or findings; 2 usage error; 3 store error
//! (a broken or unreadable world, a writer lock held through the busy
//! timeout, a world file replaced under the command) or a failed write to
//! stdout/stderr (a closed pipe is not a failure); 4 a conflict: another
//! writer changed something a write form had read, nothing was written and
//! running the form again is safe.
//! Every human-readable line goes through [`escape`] before it is printed.

mod cli;
mod cmd;
mod error;
mod escape;
mod names;
mod out;

use std::ffi::OsString;
use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use cli::Cli;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(e) => return usage(e, &args),
    };
    match cmd::dispatch(&cli.command) {
        Ok(done) => {
            let code = if done.findings {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            };
            emit(Stream::Stdout, &escape::text(&done.text), code)
        }
        Err(e) => emit(
            Stream::Stderr,
            &format!("kala: error: {}\n", escape::text(e.message())),
            e.exit_code(),
        ),
    }
}

/// Handles a clap error: help and version go to stdout with exit 0, every
/// other error to stderr with exit 2. Never `err.exit()`.
fn usage(e: clap::Error, args: &[OsString]) -> ExitCode {
    if !e.use_stderr() {
        return emit(
            Stream::Stdout,
            &escape::text(&e.render().to_string()),
            ExitCode::SUCCESS,
        );
    }
    let rendered = if args.get(1).is_some_and(|arg| arg == "user") {
        user_render(args, 2)
    } else if args.get(1).is_some_and(|arg| arg == "help")
        && args.get(2).is_some_and(|arg| arg == "user")
    {
        // `kala help user ...`: clap's help subcommand echoes an unknown
        // subcommand name, which may be a mistyped password.
        user_render(args, 3)
    } else {
        echo_safe_render(e, args)
    };
    emit(Stream::Stderr, &escape::text(&rendered), ExitCode::from(2))
}

/// What clap may echo in place of a redacted `kala user` argument.
const REDACTED: &str = "<redacted>";

/// Renders a `kala user` usage error with clap's own diagnosis (the missing
/// argument, the unknown subcommand, the unexpected argument) but none of
/// the argument values: a mistaken `--password=...`, `--password VALUE` or
/// extra positional may hold a password, and argv must never be echoed
/// back. The command line is parsed again with every value replaced by
/// [`REDACTED`]; only a declared `kala user` subcommand name (or `help`)
/// in the subcommand position `sub` and the flag names `kala user`
/// declares (plus `--password`, the flag people reach for) survive. A
/// mistyped subcommand may itself be a password, so it is redacted too.
/// For `kala help user ...` (`sub` = 3) everything after the subcommand
/// position is redacted: clap's help subcommand takes only names. Values
/// carry no validation in these subcommands, so the redacted line fails
/// the same way; if it doesn't, a generic message is shown instead.
fn user_render(args: &[OsString], sub: usize) -> String {
    let (flags, subcommands) = user_names();
    let redacted: Vec<OsString> = args
        .iter()
        .enumerate()
        .map(|(i, arg)| {
            let Some(text) = arg.to_str() else {
                return OsString::from(REDACTED);
            };
            if i < sub {
                return arg.clone();
            }
            if i == sub && !text.starts_with('-') {
                return if subcommands.iter().any(|name| name == text) {
                    arg.clone()
                } else {
                    OsString::from(REDACTED)
                };
            }
            if sub > 2 && i > sub {
                return OsString::from(REDACTED);
            }
            if text == "--" {
                return arg.clone();
            }
            if !text.starts_with('-') {
                return OsString::from(REDACTED);
            }
            let (name, value) = match text.split_once('=') {
                Some((name, _)) => (name, Some(REDACTED)),
                None => (text, None),
            };
            if !flags.iter().any(|flag| flag == name) {
                return OsString::from(format!("--{REDACTED}"));
            }
            OsString::from(match value {
                Some(value) => format!("{name}={value}"),
                None => name.to_string(),
            })
        })
        .collect();
    match Cli::try_parse_from(&redacted) {
        Err(e) if e.use_stderr() => echo_safe_render(e, &redacted),
        _ => "error: invalid kala user arguments; use `kala user --help` (passwords require --password-stdin or a hidden prompt)\n".into(),
    }
}

/// Every flag name `kala user` and its subcommands declare, as typed
/// (`--trust`, `-h`), plus `--password`; and every `kala user` subcommand
/// name, plus `help`.
fn user_names() -> (Vec<String>, Vec<String>) {
    use clap::CommandFactory;
    let mut flags = vec!["--help".to_string(), "-h".to_string(), "--password".into()];
    let mut subcommands = vec!["help".to_string()];
    let command = Cli::command();
    let Some(user) = command.find_subcommand("user") else {
        return (flags, subcommands);
    };
    subcommands.extend(user.get_subcommands().map(|cmd| cmd.get_name().to_string()));
    for cmd in std::iter::once(user).chain(user.get_subcommands()) {
        for arg in cmd.get_arguments() {
            flags.extend(arg.get_long().map(|long| format!("--{long}")));
            flags.extend(arg.get_short().map(|short| format!("-{short}")));
        }
    }
    (flags, subcommands)
}

/// Renders a usage error so that every argument it echoes is escaped as a
/// user value (newlines included) and truncated. clap echoes arguments
/// verbatim, and escaping the rendered text as a whole has to keep the
/// layout's newlines; so when an argument would need escaping, the same
/// command line is parsed again with every argument already escaped, and
/// that error is shown. If the escaped line happens to parse (or ends in help),
/// the original error is shown, escaped as text.
fn echo_safe_render(e: clap::Error, args: &[OsString]) -> String {
    let needs = args
        .iter()
        .skip(1)
        .any(|a| a.to_str().is_none_or(escape::value_needs_escape));
    if needs {
        let safe: Vec<OsString> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i == 0 {
                    a.clone()
                } else {
                    OsString::from(escape::value(&a.to_string_lossy()))
                }
            })
            .collect();
        if let Err(e2) = Cli::try_parse_from(&safe)
            && e2.use_stderr()
        {
            return e2.render().to_string();
        }
    }
    e.render().to_string()
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

/// Writes `s` to `stream` and returns `code`, or exit 3 if the write failed
/// for any reason other than a closed pipe (`kala ... | head` is the
/// reader's choice and keeps `code`). A full disk or a dead terminal must
/// not look like success to a script. A failed stdout write is reported on
/// stderr if possible; a failed stderr write has nowhere to be reported.
fn emit(stream: Stream, s: &str, code: ExitCode) -> ExitCode {
    let written = match stream {
        Stream::Stdout => print_to(&mut std::io::stdout().lock(), s),
        Stream::Stderr => print_to(&mut std::io::stderr().lock(), s),
    };
    match written {
        Ok(()) => code,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => code,
        Err(e) => {
            if let Stream::Stdout = stream {
                let _ = print_to(
                    &mut std::io::stderr().lock(),
                    &format!(
                        "kala: error: writing output: {}\n",
                        escape::line(&e.to_string())
                    ),
                );
            }
            ExitCode::from(3)
        }
    }
}

/// Writes all of `s` to `w` and flushes it.
fn print_to(w: &mut dyn Write, s: &str) -> std::io::Result<()> {
    w.write_all(s.as_bytes())?;
    w.flush()
}
