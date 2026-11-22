//! A read-only snapshot session. Both input backends feed the same bounded
//! command loop; only the terminal backend installs a signal handler.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kala_core::{Tick, TxId};
use kala_datalog::{Completeness, MAX_QUERY_BYTES, SessionDefaults};
use kala_query::Limits;
use rustyline::error::ReadlineError;
use rustyline::history::MemHistory;

use super::Done;
use super::query::{parse_params, query_err, read_text_file, render_result};
use super::read::Ctx;
use super::repl_editor::SafeHelper;
use super::repl_history::{History, MAX_HISTORY_ENTRIES, safe_entry};
use crate::cli::{Canon, parse_canon};
use crate::error::CliError;
use crate::escape;

const HELP: &str = "Read-only Datalog session (snapshot until .reload)\n\
.at [T]          set/clear default story tick\n\
.canon [C]       set/clear default canon (N or tx#N)\n\
.param NAME VALUE / .unparam NAME / .params\n\
.rule [[(name ?x) ...]] / .unrule NAME / .rules\n\
.explain QUERY   show the plan (multiline allowed)\n\
.load FILE       run a query file\n\
.schema          show node types and attributes\n\
.info / .reload  inspect/reopen the snapshot\n\
.json on|off     select query output format\n\
.limits          show per-query resource limits\n\
.cancel          discard incomplete input\n\
.help / .quit / .exit\n";

type Editor = rustyline::Editor<SafeHelper, MemHistory>;

enum Input<'a> {
    Plain(io::StdinLock<'a>),
    Terminal(Box<Editor>),
}

enum Event {
    Line(String),
    Interrupted,
    Eof,
    Invalid(String),
}

impl Input<'_> {
    fn next(&mut self, continuation: bool) -> io::Result<Event> {
        match self {
            Self::Plain(reader) => read_line(reader),
            Self::Terminal(editor) => {
                match editor.readline(if continuation { "  ...> " } else { "kala> " }) {
                    Ok(line) if line.len() <= MAX_QUERY_BYTES => {
                        // In-memory history only. Files are handled by History.
                        if safe_entry(&line) {
                            let _ = editor.add_history_entry(line.as_str());
                        }
                        Ok(Event::Line(line))
                    }
                    Ok(_) => Ok(Event::Invalid(format!(
                        "input exceeds {MAX_QUERY_BYTES} bytes; buffer discarded"
                    ))),
                    Err(ReadlineError::Interrupted) => Ok(Event::Interrupted),
                    Err(ReadlineError::Eof) => Ok(Event::Eof),
                    Err(ReadlineError::Io(e)) => Err(e),
                    Err(e) => Err(io::Error::other(e.to_string())),
                }
            }
        }
    }
}

/// Consume one physical line in bounded chunks, draining an oversized line
/// before reporting it so that the next command can still be read.
fn read_line(reader: &mut impl BufRead) -> io::Result<Event> {
    let mut line = Vec::new();
    let mut oversized = false;
    let mut saw_input = false;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            if !saw_input {
                return Ok(Event::Eof);
            }
            break;
        }
        saw_input = true;
        let end = buf.iter().position(|b| *b == b'\n');
        let used = end.map_or(buf.len(), |n| n + 1);
        let content = end.unwrap_or(used);
        if !oversized && line.len().saturating_add(content) <= MAX_QUERY_BYTES + 1 {
            line.extend_from_slice(&buf[..content]);
        } else {
            oversized = true;
            line.clear();
        }
        reader.consume(used);
        if end.is_some() {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if oversized || line.len() > MAX_QUERY_BYTES {
        return Ok(Event::Invalid(format!(
            "input exceeds {MAX_QUERY_BYTES} bytes; buffer discarded"
        )));
    }
    Ok(match String::from_utf8(line) {
        Ok(line) => Event::Line(line),
        Err(_) => Event::Invalid("input is not UTF-8; buffer discarded".into()),
    })
}

#[derive(Clone, Copy)]
enum Pending {
    Query,
    Explain,
    Rules,
}

struct Session<'a> {
    path: &'a Path,
    ctx: Ctx,
    defaults: SessionDefaults,
    params: BTreeMap<String, String>,
    json: bool,
    buffer: String,
    pending: Pending,
    cancel: Arc<AtomicBool>,
}

enum Action {
    Continue,
    Output(String),
    Quit,
}

impl Session<'_> {
    fn incomplete(&self) -> bool {
        !self.buffer.is_empty() || !matches!(self.pending, Pending::Query)
    }

    fn discard(&mut self) {
        self.buffer.clear();
        self.pending = Pending::Query;
    }

    fn query(&mut self, text: &str, explain: bool) -> Result<String, CliError> {
        self.cancel.store(false, Ordering::Relaxed);
        let prepared = kala_datalog::prepare_with(&self.ctx.world, text, &self.defaults)
            .map_err(|e| CliError::User(format!("query: {e}")))?;
        let given: Vec<_> = prepared
            .params
            .iter()
            .filter_map(|(name, _)| {
                self.params
                    .get(name)
                    .map(|value| (name.clone(), value.clone()))
            })
            .collect();
        let params = parse_params(&self.ctx, &prepared.params, &given)?;
        if explain {
            return Ok(prepared.plan.explain(&self.ctx.world));
        }
        let rs = kala_query::run_with_cancel(
            &self.ctx.world,
            &prepared.plan,
            &params,
            &Limits::default(),
            &self.cancel,
        )
        .map_err(|e| query_err(e, "query"))?;
        Ok(render_result(&self.ctx.world, &rs, self.json))
    }

    fn append(&mut self, line: &str) -> Result<Action, CliError> {
        let separator = usize::from(!self.buffer.is_empty());
        if self
            .buffer
            .len()
            .saturating_add(separator)
            .saturating_add(line.len())
            > MAX_QUERY_BYTES
        {
            self.discard();
            return Err(user(format!(
                "input exceeds {MAX_QUERY_BYTES} bytes; buffer discarded"
            )));
        }
        if separator != 0 {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);
        if kala_datalog::completeness(&self.buffer) == Completeness::Incomplete {
            return Ok(Action::Continue);
        }
        let text = std::mem::take(&mut self.buffer);
        let kind = self.pending;
        self.pending = Pending::Query;
        match kind {
            Pending::Query => self.query(&text, false).map(Action::Output),
            Pending::Explain => self.query(&text, true).map(Action::Output),
            Pending::Rules => {
                let rules =
                    kala_datalog::parse_rules(&text).map_err(|e| user(format!("rule: {e}")))?;
                let bytes: usize = self
                    .defaults
                    .rules
                    .iter()
                    .chain(&rules)
                    .map(|r| r.source().len())
                    .sum();
                if bytes > MAX_QUERY_BYTES
                    || self.defaults.rules.len() + rules.len() > kala_datalog::MAX_RULES
                {
                    return Err(user("session rules exceed the query limits"));
                }
                self.defaults.rules.extend(rules);
                Ok(Action::Continue)
            }
        }
    }

    fn line(&mut self, line: &str) -> Result<Action, CliError> {
        let trimmed = line.trim();
        // While a multi-line input is pending, every line is input (it may
        // continue a string that starts with `.`); only an exact `.cancel`
        // is a command.
        if self.incomplete() {
            if trimmed == ".cancel" {
                self.discard();
                return Ok(Action::Continue);
            }
            return self.append(line);
        }
        if !trimmed.starts_with('.') {
            if self.buffer.is_empty() && (trimmed.is_empty() || trimmed.starts_with(';')) {
                return Ok(Action::Continue);
            }
            return self.append(line);
        }
        let (command, arg) = split(trimmed);
        match command {
            ".quit" | ".exit" => {
                no_arg(arg)?;
                Ok(Action::Quit)
            }
            ".cancel" => {
                no_arg(arg)?;
                self.discard();
                Ok(Action::Continue)
            }
            ".help" => {
                no_arg(arg)?;
                Ok(Action::Output(HELP.into()))
            }
            ".at" => {
                self.defaults.at = if arg.is_empty() {
                    None
                } else {
                    Some(Tick(
                        arg.parse()
                            .map_err(|_| user("expected a signed 64-bit tick"))?,
                    ))
                };
                Ok(Action::Continue)
            }
            ".canon" => {
                self.defaults.canon = if arg.is_empty() {
                    None
                } else {
                    Some(TxId(parse_canon(arg).map_err(user)?.0))
                };
                Ok(Action::Continue)
            }
            ".param" => {
                let (raw, value) = split(arg);
                let name = param_name(raw)?;
                if value.is_empty() {
                    return Err(user("usage: .param NAME VALUE"));
                }
                let bytes: usize = self
                    .params
                    .iter()
                    .filter(|(n, _)| n.as_str() != name)
                    .map(|(n, v)| n.len() + v.len())
                    .sum();
                if bytes.saturating_add(name.len()).saturating_add(value.len()) > MAX_QUERY_BYTES {
                    return Err(user("session parameters exceed the query limit"));
                }
                self.params.insert(name.into(), value.into());
                Ok(Action::Continue)
            }
            ".unparam" => {
                self.params.remove(param_name(arg)?);
                Ok(Action::Continue)
            }
            ".params" => {
                no_arg(arg)?;
                let mut out = String::from("parameters:\n");
                for (name, value) in &self.params {
                    let _ = writeln!(out, "  ?{} = {}", escape::line(name), escape::line(value));
                }
                Ok(Action::Output(out))
            }
            ".rule" | ".explain" => {
                self.pending = if command == ".rule" {
                    Pending::Rules
                } else {
                    Pending::Explain
                };
                self.append(arg)
            }
            ".rules" => {
                no_arg(arg)?;
                let mut out = String::from("rules:\n");
                for rule in &self.defaults.rules {
                    let _ = writeln!(out, "  {}", escape::line(rule.source()));
                }
                Ok(Action::Output(out))
            }
            ".unrule" => {
                if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                    return Err(user("usage: .unrule NAME"));
                }
                self.defaults.rules.retain(|r| r.name() != arg);
                Ok(Action::Continue)
            }
            ".json" => {
                self.json = match arg {
                    "on" => true,
                    "off" => false,
                    _ => return Err(user("usage: .json on|off")),
                };
                Ok(Action::Continue)
            }
            ".load" => {
                if arg.is_empty() {
                    return Err(user("usage: .load FILE"));
                }
                let text = read_text_file(Path::new(arg), MAX_QUERY_BYTES, "the query")?;
                self.query(&text, false).map(Action::Output)
            }
            ".info" => {
                no_arg(arg)?;
                Ok(Action::Output(super::info::snapshot(
                    self.path,
                    &self.ctx.world,
                    self.json,
                )))
            }
            ".reload" => {
                no_arg(arg)?;
                self.ctx = Ctx::open(self.path, None)?;
                Ok(Action::Output(format!(
                    "reloaded: latest tx tx#{}, latest tick {}\n",
                    self.ctx.world.latest_tx().0,
                    self.ctx.world.latest_tick().0
                )))
            }
            ".limits" => {
                no_arg(arg)?;
                let l = Limits::default();
                Ok(Action::Output(format!(
                    "limits: max_rows={}, max_steps={}, max_bytes={}\n",
                    l.max_rows, l.max_steps, l.max_bytes
                )))
            }
            ".schema" => {
                no_arg(arg)?;
                Ok(Action::Output(self.schema()))
            }
            _ => Err(user(format!(
                "unknown command `{}`; use .help for commands",
                escape::value(command)
            ))),
        }
    }

    fn schema(&self) -> String {
        let mut out = String::from("node types:\n");
        for name in self.ctx.world.node_types() {
            let _ = writeln!(out, "  {}", escape::line(name));
        }
        out.push_str(
            "attributes (type, kind, cardinality, direction, inverse, reverse, indexed):\n",
        );
        for a in self.ctx.world.attributes() {
            let ty = match a.value_type {
                kala_core::ValueType::Str => "string",
                kala_core::ValueType::Ref => "ref",
                kala_core::ValueType::Bool => "bool",
                kala_core::ValueType::I64 => "i64",
                kala_core::ValueType::F64 => "f64",
            };
            let direction = match a.direction {
                Some(kala_core::RefDirection::Directed) => "directed",
                Some(kala_core::RefDirection::Bidirectional) => "bidirectional",
                None => "-",
            };
            let _ = writeln!(
                out,
                "  {}: {}, {}, {}, {}, {}, {}, indexed={}",
                escape::line(&a.name),
                ty,
                format!("{:?}", a.kind).to_lowercase(),
                format!("{:?}", a.cardinality).to_lowercase(),
                direction,
                a.inverse
                    .as_deref()
                    .map(escape::line)
                    .unwrap_or_else(|| "-".into()),
                a.reverse_cardinality
                    .map(|c| format!("{c:?}").to_lowercase())
                    .unwrap_or_else(|| "-".into()),
                a.indexed
            );
        }
        out
    }
}

fn user(message: impl Into<String>) -> CliError {
    CliError::User(message.into())
}
fn split(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map_or((text, ""), |(a, b)| (a, b.trim()))
}
fn no_arg(arg: &str) -> Result<(), CliError> {
    if arg.is_empty() {
        Ok(())
    } else {
        Err(user("this command takes no argument"))
    }
}
fn param_name(raw: &str) -> Result<&str, CliError> {
    let name = raw.strip_prefix('?').unwrap_or(raw);
    if name.is_empty() || name.chars().any(char::is_whitespace) {
        Err(user("expected a parameter NAME"))
    } else {
        Ok(name)
    }
}

/// Writes and flushes each response. Broken pipes are the reader's choice;
/// any other failed output write keeps the CLI's exit-3 contract.
fn output(stream: &mut impl Write, text: &str) -> Result<(), CliError> {
    match stream
        .write_all(text.as_bytes())
        .and_then(|_| stream.flush())
    {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(CliError::Store(format!("writing output: {e}"))),
    }
}

pub fn run(
    world: &Path,
    at: Option<i64>,
    canon: Option<Canon>,
    history_path: Option<&Path>,
) -> Result<Done, CliError> {
    let ctx = Ctx::open(world, None).map_err(|e| CliError::Store(e.message().into()))?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let mut history = history_path
        .map(|path| History::open(path, world))
        .transpose()?;
    // Save the history on every way out, errors included; an error from the
    // session itself wins over one from saving.
    let result = drive(world, at, canon, ctx, interactive, &mut history);
    if let Some(h) = &mut history {
        match (h.save(), &result) {
            (Err(e), Ok(_)) => return Err(e),
            (Err(e), Err(_)) => {
                // The session error is returned; still say the history was lost.
                let _ = output(
                    &mut io::stderr().lock(),
                    &format!("warning: {}\n", escape::line(e.message())),
                );
            }
            (Ok(()), _) => {}
        }
    }
    result
}

/// The session loop of [`run`], with the world opened and the history (if
/// any) loaded.
fn drive(
    world: &Path,
    at: Option<i64>,
    canon: Option<Canon>,
    ctx: Ctx,
    interactive: bool,
    history: &mut Option<History>,
) -> Result<Done, CliError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let stdin = io::stdin();
    let mut input = if interactive {
        let flag = Arc::clone(&cancel);
        ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
            .map_err(|e| CliError::Store(format!("installing cancellation handler: {e}")))?;
        let config = rustyline::Config::builder()
            .max_history_size(MAX_HISTORY_ENTRIES)
            .map_err(|e| user(e.to_string()))?
            .enable_signals(false)
            // The helper escapes untrusted input even when NO_COLOR is set.
            .color_mode(rustyline::ColorMode::Forced)
            .build();
        let mut editor = Editor::with_config(config).map_err(|e| user(e.to_string()))?;
        editor.set_helper(Some(SafeHelper));
        if let Some(h) = history.as_ref() {
            for entry in h.entries() {
                editor
                    .add_history_entry(entry)
                    .map_err(|e| user(e.to_string()))?;
            }
        }
        Input::Terminal(Box::new(editor))
    } else {
        Input::Plain(stdin.lock())
    };
    let mut session = Session {
        path: world,
        ctx,
        defaults: SessionDefaults {
            at: at.map(Tick),
            canon: canon.map(|c| TxId(c.0)),
            rules: Vec::new(),
        },
        params: BTreeMap::new(),
        json: false,
        buffer: String::new(),
        pending: Pending::Query,
        cancel,
    };
    let mut failed = false;
    let mut printed = false;
    loop {
        let event = input
            .next(session.incomplete())
            .map_err(|e| CliError::Store(format!("reading input: {e}")))?;
        let action = match event {
            Event::Eof => {
                if session.incomplete() {
                    failed = true;
                    output(
                        &mut io::stderr().lock(),
                        "error: incomplete input at EOF; buffer discarded\n",
                    )?;
                }
                break;
            }
            Event::Interrupted => {
                session.discard();
                continue;
            }
            Event::Invalid(message) => {
                session.discard();
                Err(user(message))
            }
            Event::Line(line) => {
                if let Some(h) = history {
                    h.push(&line);
                }
                session.line(&line)
            }
        };
        match action {
            Ok(Action::Continue) => {}
            Ok(Action::Quit) => break,
            Ok(Action::Output(text)) => {
                let mut stdout = io::stdout().lock();
                if printed {
                    output(&mut stdout, "\n")?;
                }
                output(&mut stdout, &escape::text(&text))?;
                printed = true;
            }
            Err(e) => {
                failed = true;
                output(
                    &mut io::stderr().lock(),
                    &format!("error: {}\n", escape::line(e.message())),
                )?;
            }
        }
    }
    Ok(Done {
        text: String::new(),
        findings: failed && !interactive,
    })
}
