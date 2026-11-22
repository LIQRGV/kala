//! The `kala` command line: a thin layer over the public kala-core API.
//!
//! Exit codes: 0 ok; 1 user error or findings; 2 usage error; 3 store error
//! or a failed write to stdout/stderr (a closed pipe is not a failure).
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
    let rendered = echo_safe_render(e, args);
    emit(Stream::Stderr, &escape::text(&rendered), ExitCode::from(2))
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
