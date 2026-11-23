//! `kala query <world> [QUERY] [-f FILE] [--param name=value].. [--json]` and
//! `kala explain <world> [QUERY] [-f FILE] [--param ..]`: Cypher or Datalog
//! from the command line, through their front-ends and the shared executor.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use kala_core::World;
use kala_core::{Value, ValueType};
use kala_datalog::{MAX_QUERY_BYTES, ParamKind, Prepared};
use kala_query::{Datum, Limits, ParameterInput, Params, QueryError, QueryParams, ResultSet};
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
    /// `?a` for a variable, the aggregate's text (`(count ?c)`) for an
    /// aggregate, in `:find` order.
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
            let _ = writeln!(s, "  {}", row_text(r));
        }
        s
    }
}

/// A result row's text: each value as [`value_text`] prints it, two spaces
/// apart. Shared with `kala doctor --checks`.
pub(crate) fn row_text(r: &[JValue]) -> String {
    let vals: Vec<String> = r.iter().map(value_text).collect();
    vals.join("  ")
}

/// A result's columns (see [`column_name`]) and its rows as JSON values,
/// entities shown at the result's own default tick and canon. Shared with
/// `kala doctor --checks`.
pub(crate) fn table(world: &World, rs: &ResultSet) -> (Vec<String>, Vec<Vec<JValue>>) {
    let l = Labeler::new(world, rs.canon, rs.tick);
    let columns = rs.columns.iter().map(|v| column_name(&v.0)).collect();
    let rows = rs
        .rows
        .iter()
        .map(|r| r.iter().map(|d| l.datum(d)).collect())
        .collect();
    (columns, rows)
}

/// `kala query`: runs the query and prints its rows. Entities are shown
/// (names, labels) at the query's resolved default tick and canon.
pub fn run(world: &Path, q: &QueryArgs, json: bool) -> Result<String, CliError> {
    let (ctx, prepared, params) = prepare(world, q)?;
    let rs = kala_query::run_with_values(&ctx.world, &prepared.plan, &params, &Limits::default())
        .map_err(|e| query_err(e, "query"))?;
    Ok(render_result(&ctx.world, &rs, json))
}

/// The shared one-shot and REPL result format.
pub(crate) fn render_result(world: &World, rs: &ResultSet, json: bool) -> String {
    let (columns, rows) = table(world, rs);
    let out = Query {
        command: "query",
        canon: rs.canon.0,
        tick: rs.tick.0,
        columns,
        rows,
    };
    render(&out, json)
}

/// A result column's printed name: `?a` for a variable; an aggregate
/// column is already named by its text (`(count ?c)`), and variable names
/// never start with `(`.
pub(crate) fn column_name(name: &str) -> String {
    if name.starts_with('(') {
        name.to_string()
    } else {
        format!("?{name}")
    }
}

/// `kala explain`: prints the plan. Every parameter must be given and is
/// checked as `query` checks it, but nothing is executed.
pub fn explain(world: &Path, q: &QueryArgs) -> Result<String, CliError> {
    let (ctx, prepared, _params) = prepare(world, q)?;
    Ok(prepared.plan.explain(&ctx.world))
}

/// Reads the query text, opens the world, prepares the query and parses
/// its parameters.
fn prepare(world: &Path, q: &QueryArgs) -> Result<(Ctx, Prepared, QueryParams), CliError> {
    let text = match (&q.query, &q.file) {
        (Some(t), None) => t.clone(),
        (None, Some(f)) => read_text_file(f, MAX_QUERY_BYTES, "the query")?,
        // clap's required `source` group already refuses both and neither.
        _ => {
            return Err(CliError::Usage(
                "give exactly one of QUERY and `-f FILE`".to_string(),
            ));
        }
    };
    let ctx = Ctx::open(world, None)?;
    let front_end = super::language::front_end(&q.lang)?;
    let prepared = front_end.prepare(&ctx.world, &text)?;
    let params = front_end.params(&ctx, &prepared.params, &q.params)?;
    Ok((ctx, prepared, params))
}

/// The `--param` texts, each parsed by the kind the query gives it.
/// Unknown, duplicate (`who` and `?who` are one name) and missing
/// parameters are user errors.
pub(crate) fn parse_params(
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
        ParamKind::Scalar => Ok(if let Ok(n) = s.parse::<i64>() {
            Value::I64(n)
        } else if let Ok(b) = s.parse::<bool>() {
            Value::Bool(b)
        } else if let Some(f) = numeric_float(s) {
            Value::f64(f).map_err(|_| CliError::User("invalid float parameter".into()))?
        } else {
            Value::Str(s.to_owned())
        }),
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

/// `s` as a float, only when it looks numeric:
/// `^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$`. Rust's `f64` parser also
/// takes `nan`, `inf` and `infinity`; those words stay strings here, since a
/// scalar parameter is guessed from its text.
fn numeric_float(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_digits = digits(&mut i);
    let mut frac_digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = digits(&mut i);
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// A query (or checks) file's text: a regular file only (a directory, FIFO
/// or device is refused before it is opened, so it can't hang or read
/// without bound), at most `max` bytes, UTF-8. `what` names the content in
/// messages (`the query`). Every failure is the user's except an I/O error
/// other than not-found on a regular file.
pub(crate) fn read_text_file(path: &Path, max: usize, what: &str) -> Result<String, CliError> {
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
    let cap = u64::try_from(max).map_or(u64::MAX, |m| m.saturating_add(1));
    file.take(cap).read_to_end(&mut buf).map_err(io)?;
    if buf.len() > max {
        return Err(CliError::User(format!(
            "{shown}: {what} is longer than {max} bytes"
        )));
    }
    String::from_utf8(buf).map_err(|_| CliError::User(format!("{shown}: {what} is not UTF-8")))
}

/// An executor error, its message prefixed by `whose` (`query`, `check
/// :name`): kala-core errors keep their slice-5 exit codes, everything
/// else is the user's (exit 1).
pub(crate) fn query_err(e: QueryError, whose: &str) -> CliError {
    match e {
        QueryError::Core(c) => CliError::core(&c, format!("{whose}: {c}")),
        QueryError::RowLimit { limit } => CliError::User(format!(
            "{whose}: stopped: an operator produced more than {limit} rows; narrow the query"
        )),
        QueryError::StepBudget { limit } => CliError::User(format!(
            "{whose}: stopped: the query took more than {limit} steps; narrow the query"
        )),
        QueryError::MemoryBudget { limit } => CliError::User(format!(
            "{whose}: stopped: the query needed more than {limit} bytes of memory; narrow the query"
        )),
        e => CliError::User(format!("{whose}: {e}")),
    }
}

pub(crate) fn parse_cypher_params(
    ctx: &Ctx,
    kinds: &[(String, ParamKind)],
    given: &[(String, String)],
) -> Result<QueryParams, CliError> {
    if given
        .iter()
        .try_fold(0usize, |n, (k, v)| {
            n.checked_add(k.len())?.checked_add(v.len())
        })
        .is_none_or(|n| n > 1 << 20)
    {
        return Err(CliError::User("query parameters exceed 1 MiB".into()));
    }
    let mut params = QueryParams::new();
    for (name, text) in given {
        let name = name.strip_prefix(['?', '$']).unwrap_or(name);
        let kind = kinds
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, k)| k)
            .ok_or_else(|| CliError::User("unknown Cypher parameter".into()))?;
        if params.contains_key(name) {
            return Err(CliError::User("duplicate Cypher parameter".into()));
        }
        let nullable = !matches!(kind, ParamKind::Tick | ParamKind::Canon);
        let value =
            if nullable && (text == "null" || text.starts_with('[') || text.starts_with('"')) {
                let value: serde_json::Value = serde_json::from_str(text)
                    .map_err(|_| CliError::User("invalid JSON query parameter".into()))?;
                json_parameter(ctx, kind, &value, 0)?
            } else {
                Datum::Value(parse_param(ctx, name, kind, text)?)
            };
        params.insert(name.into(), value);
    }
    for (name, _) in kinds {
        if !params.contains_key(name) {
            return Err(CliError::User(format!(
                "missing parameter {}",
                escape::value(name)
            )));
        }
    }
    Ok(params)
}
fn json_parameter(
    ctx: &Ctx,
    kind: &ParamKind,
    value: &serde_json::Value,
    depth: usize,
) -> Result<Datum, CliError> {
    use serde_json::Value as Json;
    let bad = || CliError::User("wrong type for JSON query parameter".into());
    if depth > 32 {
        return Err(CliError::User(
            "query parameter nesting limit exceeded".into(),
        ));
    }
    let value = match value {
        Json::Null => return Ok(Datum::Null),
        Json::Array(values) => {
            return values
                .iter()
                .map(|v| json_parameter(ctx, kind, v, depth + 1))
                .collect::<Result<Vec<_>, _>>()
                .map(Datum::List);
        }
        Json::String(s) if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::Str)) => {
            Value::Str(s.clone())
        }
        Json::String(s) if matches!(kind, ParamKind::Entity | ParamKind::Value(ValueType::Ref)) => {
            ctx.defined_entity(s).map(Value::Ref)?
        }
        Json::Bool(b) if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::Bool)) => {
            Value::Bool(*b)
        }
        Json::Number(n)
            if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::I64))
                && n.is_i64() =>
        {
            Value::I64(n.as_i64().ok_or_else(bad)?)
        }
        Json::Number(n) if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::F64)) => {
            Value::f64(n.as_f64().ok_or_else(bad)?).map_err(|_| bad())?
        }
        _ => return Err(bad()),
    };
    Ok(Datum::Value(value))
}
