//! `kala query <world> [QUERY] [-f FILE] [--param name=value].. [--json]` and
//! `kala explain <world> [QUERY] [-f FILE] [--param ..]`: Datalog from the
//! command line, through `kala_datalog::prepare` and `kala_query::run`.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use kala_core::{Value, ValueType};
use kala_datalog::{MAX_QUERY_BYTES, ParamKind, Prepared};
use kala_query::{Limits, Params, QueryError};
use serde::Serialize;

use super::read::{Ctx, parse_value};
use crate::cli::{QueryArgs, parse_canon};
use crate::error::CliError;
use crate::escape;
use crate::out::{JValue, Labeler, Text, render, value_text};

#[derive(Serialize)]
struct Query {
    command: &'static str,
    canon: u64,
    tick: i64,
    /// `?a`, in `:find` order.
    columns: Vec<String>,
    rows: Vec<Vec<JValue>>,
}

impl Text for Query {
    fn text(&self) -> String {
        let mut s = String::new();
        let n = self.rows.len();
        let _ = writeln!(
            s,
            "query at tick {}, canon tx#{}: {n} {}",
            self.tick,
            self.canon,
            if n == 1 { "row" } else { "rows" }
        );
        let cols: Vec<String> = self.columns.iter().map(|c| escape::value(c)).collect();
        let _ = writeln!(s, "  {}", cols.join("  "));
        if self.rows.is_empty() {
            s.push_str("  (no rows)\n");
        }
        for r in &self.rows {
            let vals: Vec<String> = r.iter().map(value_text).collect();
            let _ = writeln!(s, "  {}", vals.join("  "));
        }
        s
    }
}

/// `kala query`: runs the query and prints its rows. Entities are shown
/// (names, labels) at the query's resolved default tick and canon.
pub fn run(world: &Path, q: &QueryArgs, json: bool) -> Result<String, CliError> {
    let (ctx, prepared, params) = prepare(world, q)?;
    let rs = kala_query::run(&ctx.world, &prepared.plan, &params, &Limits::default())
        .map_err(query_err)?;
    let l = Labeler::new(&ctx.world, rs.canon, rs.tick);
    let out = Query {
        command: "query",
        canon: rs.canon.0,
        tick: rs.tick.0,
        columns: rs.columns.iter().map(|v| format!("?{}", v.0)).collect(),
        rows: rs
            .rows
            .iter()
            .map(|r| r.iter().map(|v| l.value(v)).collect())
            .collect(),
    };
    Ok(render(&out, json))
}

/// `kala explain`: prints the plan. Every parameter must be given and is
/// checked as `query` checks it, but nothing is executed.
pub fn explain(world: &Path, q: &QueryArgs) -> Result<String, CliError> {
    let (ctx, prepared, _params) = prepare(world, q)?;
    Ok(prepared.plan.explain(&ctx.world))
}

/// Reads the query text, opens the world, prepares the query and parses
/// its parameters.
fn prepare(world: &Path, q: &QueryArgs) -> Result<(Ctx, Prepared, Params), CliError> {
    let text = match (&q.query, &q.file) {
        (Some(t), None) => t.clone(),
        (None, Some(f)) => read_query_file(f)?,
        // clap's required `source` group already refuses both and neither.
        _ => {
            return Err(CliError::Usage(
                "give exactly one of QUERY and `-f FILE`".to_string(),
            ));
        }
    };
    let ctx = Ctx::open(world, None)?;
    // Datalog messages escape the input they echo; `main` escapes the
    // whole line again.
    let prepared = kala_datalog::prepare(&ctx.world, &text)
        .map_err(|e| CliError::User(format!("query: {e}")))?;
    let params = parse_params(&ctx, &prepared.params, &q.params)?;
    Ok((ctx, prepared, params))
}

/// The `--param` texts, each parsed by the kind the query gives it.
/// Unknown, duplicate (`who` and `?who` are one name) and missing
/// parameters are user errors.
fn parse_params(
    ctx: &Ctx,
    kinds: &[(String, ParamKind)],
    given: &[(String, String)],
) -> Result<Params, CliError> {
    let mut params = Params::new();
    for (raw, text) in given {
        let name = raw.strip_prefix('?').unwrap_or(raw);
        let Some((_, kind)) = kinds.iter().find(|(n, _)| n == name) else {
            return Err(CliError::User(format!(
                "unknown parameter `?{}`: the query's `:in` doesn't declare it",
                escape::value(name)
            )));
        };
        if params.contains_key(name) {
            return Err(CliError::User(format!(
                "parameter `?{}` is given twice",
                escape::value(name)
            )));
        }
        let v = parse_param(ctx, name, kind, text)?;
        params.insert(name.to_string(), v);
    }
    for (name, _) in kinds {
        if !params.contains_key(name) {
            return Err(CliError::User(format!(
                "missing parameter `?{}`: give it with `--param {}=VALUE`",
                escape::value(name),
                escape::value(name)
            )));
        }
    }
    Ok(params)
}

/// One parameter's text, as the slice-5 arguments of that kind parse.
fn parse_param(ctx: &Ctx, name: &str, kind: &ParamKind, s: &str) -> Result<Value, CliError> {
    let whose = format!("parameter `?{}`", escape::value(name));
    match kind {
        ParamKind::Entity | ParamKind::Value(ValueType::Ref) => {
            ctx.defined_entity(s).map(Value::Ref)
        }
        ParamKind::Value(ty) => parse_value(ctx, *ty, s, &whose),
        // `+5` is accepted, as `@+5` is.
        ParamKind::Tick => s.parse::<i64>().map(Value::I64).map_err(|_| {
            CliError::User(format!(
                "`{}` is not a tick (a signed 64-bit integer) for {whose}",
                escape::value(s)
            ))
        }),
        ParamKind::Canon => parse_canon(s)
            .ok()
            .and_then(|c| i64::try_from(c.0).ok())
            .map(Value::I64)
            .ok_or_else(|| {
                CliError::User(format!(
                    "`{}` is not a canon (`tx#N` or `N`) for {whose}",
                    escape::value(s)
                ))
            }),
    }
}

/// A query file's text: a regular file only (a directory, FIFO or device
/// is refused before it is opened, so it can't hang or read without
/// bound), at most [`MAX_QUERY_BYTES`], UTF-8. Every failure is the user's
/// except an I/O error other than not-found on a regular file.
fn read_query_file(path: &Path) -> Result<String, CliError> {
    let shown = escape::path(path);
    let io = |e: std::io::Error| CliError::io(e.kind(), format!("{shown}: {e}"));
    let not_regular = || CliError::User(format!("{shown}: not a regular file"));
    if !std::fs::metadata(path).map_err(io)?.is_file() {
        return Err(not_regular());
    }
    let file = std::fs::File::open(path).map_err(io)?;
    // The handle's own metadata: the file actually read, even if the path
    // was swapped after the first check.
    if !file.metadata().map_err(io)?.is_file() {
        return Err(not_regular());
    }
    let mut buf = Vec::new();
    // One byte past the limit tells "at the limit" from "over it".
    let cap = u64::try_from(MAX_QUERY_BYTES).map_or(u64::MAX, |m| m.saturating_add(1));
    file.take(cap).read_to_end(&mut buf).map_err(io)?;
    if buf.len() > MAX_QUERY_BYTES {
        return Err(CliError::User(format!(
            "{shown}: the query is longer than {MAX_QUERY_BYTES} bytes"
        )));
    }
    String::from_utf8(buf).map_err(|_| CliError::User(format!("{shown}: the query is not UTF-8")))
}

/// An executor error: kala-core errors keep their slice-5 exit codes,
/// everything else is the user's (exit 1).
fn query_err(e: QueryError) -> CliError {
    match e {
        QueryError::Core(c) => CliError::core(&c, format!("query: {c}")),
        QueryError::RowLimit { limit } => CliError::User(format!(
            "query: stopped: an operator produced more than {limit} rows; narrow the query"
        )),
        QueryError::StepBudget { limit } => CliError::User(format!(
            "query: stopped: the query took more than {limit} steps; narrow the query"
        )),
        e => CliError::User(format!("query: {e}")),
    }
}
