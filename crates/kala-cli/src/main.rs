//! The `kala` command line: a thin layer over the public kala-core API.
//!
//! Exit codes: 0 ok; 1 user error or findings; 2 usage error; 3 store error.
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
            print_to(&mut std::io::stdout(), &escape::text(&done.text));
            if done.findings {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            print_to(
                &mut std::io::stderr(),
                &format!("kala: error: {}\n", escape::text(e.message())),
            );
            e.exit_code()
        }
    }
}

/// Handles a clap error: help and version go to stdout with exit 0, every
/// other error to stderr with exit 2. Never `err.exit()`.
fn usage(e: clap::Error, args: &[OsString]) -> ExitCode {
    if !e.use_stderr() {
        print_to(
            &mut std::io::stdout(),
            &escape::text(&e.render().to_string()),
        );
        return ExitCode::SUCCESS;
    }
    let rendered = echo_safe_render(e, args);
    print_to(&mut std::io::stderr(), &escape::text(&rendered));
    ExitCode::from(2)
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

/// Writes `s` to `w`, ignoring a closed pipe (`kala ... | head`).
fn print_to(w: &mut dyn Write, s: &str) {
    let _ = w.write_all(s.as_bytes());
    let _ = w.flush();
}
