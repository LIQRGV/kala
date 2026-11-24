//! Terminal writes are adapters over the native transaction API.
use super::{Done, query::read_text_file, read::core_err as read_core_err};
use crate::{cli::TransactArgs, error::CliError, escape, names, out};
use kala_core::{EntityId, Tick, Value, World};
use kala_datalog::ast::WriteTxn;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;

#[derive(Serialize)]
struct Operation {
    position: usize,
    kind: &'static str,
    tick: i64,
    /// The precision stored; `null` for a no-op, which stores nothing (a
    /// precision change on a held fact needs `:correct`).
    precision: Option<&'static str>,
    changed: usize,
    declared: usize,
    entries: Vec<u64>,
    entity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<bool>,
    #[serde(skip)]
    handle: Option<String>,
}
#[derive(Serialize)]
struct Warnings {
    introduced: Vec<String>,
    resolved: Vec<String>,
    #[serde(skip_serializing_if = "is_zero")]
    introduced_omitted: u128,
    #[serde(skip_serializing_if = "is_zero")]
    resolved_omitted: u128,
}
#[derive(Serialize)]
pub struct Report {
    form: usize,
    dry_run: bool,
    /// The committed transaction; `null` when the form changed nothing, so
    /// nothing was appended.
    tx: Option<u64>,
    changed: usize,
    declared: usize,
    entries: Vec<u64>,
    edges: Vec<String>,
    ops: Vec<Operation>,
    warnings: Warnings,
    schema: Vec<serde_json::Value>,
    #[serde(skip)]
    schema_text: Vec<String>,
}

/// Opens `path` for writing. A damaged last transaction (fully present,
/// checksum fails) may have been acknowledged to a writer, so it is never
/// cut without `acknowledge`: kala-core refuses it under the writer lock,
/// before anything is cut or quarantined, so retrying can't get past it.
/// System worlds are refused once the writer has opened (so a system world
/// with an acknowledged damaged tail is recovered before it is refused).
pub fn open_write(path: &Path, acknowledge: bool) -> Result<World, CliError> {
    let wait = busy_timeout();
    let opened = if acknowledge {
        World::open_waiting(path, wait)
    } else {
        World::open_refusing_damaged_waiting(path, wait)
    };
    let mut w = opened.map_err(|e| match e {
        kala_core::Error::WorldLocked => CliError::Store(format!(
            "{}: world is locked by another writer",
            escape::path(path)
        )),
        kala_core::Error::RecoveryRequired { bytes } => CliError::Store(format!(
            "{}: the last transaction is damaged ({bytes} bytes); opening the world \
             for writing would cut it to a quarantine file. Inspect it with \
             `kala info`, then rerun with --acknowledge-recovery",
            escape::path(path)
        )),
        e => CliError::at_path(path, &e),
    })?;
    refuse_system(&w)?;
    if w.recovery().cause == Some(kala_core::CutCause::Damaged) {
        w.acknowledge_recovery();
    }
    Ok(w)
}
/// How long a write command waits for the world's writer lock: kala-core's
/// default (5 s), which is also what the opened world's commits wait.
///
/// `KALA_BUSY_TIMEOUT_MS` (milliseconds; `0` = fail at once) overrides it.
/// It is an internal knob for the test suite, not a documented interface:
/// the `--busy-timeout` flag replaces it.
pub(crate) fn busy_timeout() -> std::time::Duration {
    std::env::var("KALA_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(
            kala_core::log::DEFAULT_BUSY_TIMEOUT,
            std::time::Duration::from_millis,
        )
}
/// System worlds can be changed only through account management.
pub(crate) fn refuse_system(world: &World) -> Result<(), CliError> {
    if kala_system::is_system(world) {
        return Err(user("system world is managed only by `kala user`"));
    }
    Ok(())
}
fn is_zero(n: &u128) -> bool {
    *n == 0
}
fn user(s: impl Into<String>) -> CliError {
    CliError::User(s.into())
}
/// Apply one form after enforcing the CLI's system-world policy. The shared
/// applier refuses a system world too (its default policy); the CLI's own
/// check comes first so its message names `kala user`.
///
/// Another process may commit to the world between this form's start and
/// its commit. kala-core then commits the form over those transactions
/// (final IDs, the form's own latest tick) unless one of them wrote
/// something the form read (a `:set`, a conditional create): that is a
/// conflict ([`kala_core::Error::Conflict`]), nothing is written, and the
/// error says to run the form again.
pub fn execute(w: &mut World, form: &WriteTxn, default: Option<Tick>) -> Result<Report, CliError> {
    refuse_system(w)?;
    let report = kala_lang::write::execute(w, form, default).map_err(write_err)?;
    Ok(committed_report(w, report))
}
/// Create-mode restores own a private file. `system` says the genesis of the
/// dump being restored declared the system marker: only then may the applier
/// write a system world.
pub(super) fn execute_restore(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
    system: bool,
) -> Result<Report, CliError> {
    let policy = kala_lang::write::WritePolicy {
        allow_system: system,
    };
    let report = kala_lang::write::execute_with(w, form, default, policy).map_err(write_err)?;
    Ok(committed_report(w, report))
}
fn write_err(e: kala_lang::write::WriteError) -> CliError {
    use kala_lang::write::Failure;
    let error = match e.cause {
        Failure::Store(e) => core_err(e),
        Failure::Migration { error, labels } => migrate_err(&labels, *error),
        Failure::UnknownEntity(s) => user(format!("unknown entity `{}`", escape::value(&s))),
        Failure::SystemWorld => user("system world is managed only by `kala user`"),
        cause => user(
            kala_lang::write::WriteError {
                operation: None,
                cause,
            }
            .to_string(),
        ),
    };
    let Some((index, verb)) = e.operation else {
        return error;
    };
    let message = format!("op {index} (:{verb}): {}", error.message());
    match error {
        CliError::Store(_) => CliError::Store(message),
        _ => user(message),
    }
}

/// The report of a committed form: each op gets the entries it stored and
/// the entity it created.
fn committed_report(w: &World, result: kala_core::write::WriteReport) -> Report {
    let ops = result
        .ops
        .iter()
        .map(|op| Operation {
            position: op.position,
            kind: op.kind,
            tick: op.tick.0,
            precision: op.precision.map(out::precision_str),
            changed: op.changed,
            declared: op.declared,
            entries: op.entries.iter().map(|e| e.0).collect(),
            entity: op.entity.map(|e| w.format_id(e)),
            created: op.created,
            handle: op
                .created
                .and_then(|_| op.entity.and_then(|e| w.handle_of(e).map(str::to_owned))),
        })
        .collect();
    let overlap_lines = |overlaps: &[_]| -> Vec<String> {
        overlaps.iter().map(|o| names::overlap_line(w, o)).collect()
    };
    Report {
        form: 1,
        dry_run: false,
        tx: result.tx.map(|tx| tx.0),
        schema: result
            .schema
            .iter()
            .map(super::schema::change_json)
            .collect(),
        schema_text: result
            .schema
            .iter()
            .map(super::schema::change_text)
            .collect(),
        changed: result.changed,
        declared: result.declared,
        entries: result.entries.iter().map(|e| e.0).collect(),
        edges: result
            .edges
            .iter()
            .map(|e| w.format_id((*e).into()))
            .collect(),
        ops,
        warnings: Warnings {
            introduced: overlap_lines(&result.warnings.introduced),
            resolved: overlap_lines(&result.warnings.resolved),
            introduced_omitted: result.warnings.introduced_omitted,
            resolved_omitted: result.warnings.resolved_omitted,
        },
    }
}

fn counts(changed: usize, declared: usize) -> String {
    let mut text = format!("{changed} changed");
    if declared > 0 {
        let _ = write!(text, ", {declared} declared");
    }
    text
}

pub fn render(report: &Report, json: bool) -> String {
    if json {
        return out::to_json(report);
    }
    let mut s = match report.tx {
        Some(tx) => format!(
            "form {}: {}tx#{tx}: {}\n",
            report.form,
            if report.dry_run {
                "dry-run "
            } else {
                "committed "
            },
            counts(report.changed, report.declared)
        ),
        None => format!(
            "form {}: {}: 0 changed\n",
            report.form,
            if report.dry_run {
                "dry-run, nothing to commit"
            } else {
                "nothing committed"
            }
        ),
    };
    for change in &report.schema_text {
        let _ = writeln!(s, "  {change}");
    }
    for op in &report.ops {
        let _ = write!(s, "  op {} (:{}): tick {}", op.position, op.kind, op.tick);
        if let Some(p) = op.precision.filter(|p| *p != "exact") {
            let _ = write!(s, " ({p})");
        }
        let _ = write!(s, ", {}", counts(op.changed, op.declared));
        if let Some(e) = &op.entity {
            let verb = if op.created == Some(false) {
                "exists"
            } else {
                "created"
            };
            let _ = write!(s, ", {verb} {e}");
            if let Some(handle) = &op.handle {
                let _ = write!(s, " {}", escape::label(handle));
            }
        }
        for id in &op.entries {
            let _ = write!(s, ", entry#{id}");
        }
        s.push('\n');
    }
    for e in &report.edges {
        let _ = writeln!(s, "  new {e}");
    }
    for o in &report.warnings.introduced {
        let _ = writeln!(s, "warning: {o}");
    }
    if report.warnings.introduced_omitted > 0 {
        let _ = writeln!(
            s,
            "… and {} more overlap warnings (run kala doctor to list them)",
            report.warnings.introduced_omitted
        );
    }
    for o in &report.warnings.resolved {
        let _ = writeln!(s, "resolved warning: {o}");
    }
    // Doctor lists current overlaps only; resolved ones no longer exist, so
    // this line has no doctor hint.
    if report.warnings.resolved_omitted > 0 {
        let _ = writeln!(
            s,
            "… and {} more resolved overlap warnings",
            report.warnings.resolved_omitted
        );
    }
    s
}

pub fn run(args: &TransactArgs) -> Result<Done, CliError> {
    let (dry_run, json) = (args.dry_run, args.json);
    let text = read_input(&args.input)?;
    let mut w = open_target(args)?;
    let forms = kala_datalog::write_forms(&text).map_err(|e| user(e.to_string()))?;
    // A dry run previews every form on one discarded copy, so later forms
    // see earlier ones exactly as a real run would.
    let mut preview = if dry_run {
        Some(w.preview_copy().map_err(core_err)?)
    } else {
        None
    };
    let mut count = 0;
    for (i, form) in forms.enumerate() {
        let action = || -> Result<Report, CliError> {
            let form = form.map_err(|e| user(e.to_string()))?;
            match &mut preview {
                Some(copy) => execute(copy, &form, None),
                None => execute(&mut w, &form, None),
            }
        };
        let mut report = action().map_err(|e| form_failed(i + 1, count, dry_run, e))?;
        report.form = i + 1;
        report.dry_run = dry_run;
        let text = escape::text(&render(&report, json));
        io::stdout()
            .write_all(text.as_bytes())
            .and_then(|()| io::stdout().flush())
            .map_err(|e| CliError::Store(format!("writing transaction result: {e}")))?;
        count += 1;
    }
    if count == 0 {
        return Err(user("expected at least one write transaction"));
    }
    Ok(String::new().into())
}

/// The write forms' text, from a file or from stdin (`-`).
fn read_input(input: &Path) -> Result<String, CliError> {
    if input != Path::new("-") {
        return read_text_file(input, kala_datalog::MAX_QUERY_BYTES, "the transactions");
    }
    let mut bytes = Vec::new();
    io::stdin()
        .take(kala_datalog::MAX_QUERY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| user(format!("reading stdin: {e}")))?;
    if bytes.len() > kala_datalog::MAX_QUERY_BYTES {
        return Err(user("input exceeds the 65536-byte limit"));
    }
    String::from_utf8(bytes).map_err(|_| user("input is not UTF-8"))
}

/// Opens the world: for writing, or read-only for a dry run (which never
/// cuts a damaged tail and so refuses one).
fn open_target(args: &TransactArgs) -> Result<World, CliError> {
    let path = args.world.as_path();
    let w = if args.dry_run {
        World::open_read_only(path).map_err(|e| CliError::at_path(path, &e))?
    } else {
        open_write(path, args.acknowledge_recovery)?
    };
    refuse_system(&w)?;
    if args.dry_run && w.recovery().cause == Some(kala_core::CutCause::Damaged) {
        return Err(user(
            "dry-run refused: damaged log needs recovery acknowledgement before writes",
        ));
    }
    Ok(w)
}

/// A form's error, saying which form failed and what happened to the
/// `count` forms before it. A store error stays a store error.
fn form_failed(form: usize, count: usize, dry_run: bool, e: CliError) -> CliError {
    let msg = format!(
        "form {form} failed; {count} prior forms {}: {}",
        if dry_run {
            "previewed (nothing committed)"
        } else {
            "committed"
        },
        e.message()
    );
    match e {
        CliError::Store(_) => CliError::Store(msg),
        _ => user(msg),
    }
}

/// [`core_err`], with a migration refusal's entities shown by handle (else
/// `TYPE#n`) and its values as `kala asof` prints them (strings quoted and
/// escaped, cut like any echoed value). kala-core's own message has only IDs.
fn migrate_err(labels: &HashMap<EntityId, String>, e: kala_core::Error) -> CliError {
    use kala_core::{Error as E, ErrorEntity};
    let entity = |e: &ErrorEntity| escape::label(labels.get(&e.id).unwrap_or(&e.formatted));
    let msg = match &e {
        E::Unconvertible {
            attr,
            from,
            to,
            count,
            examples,
        } => {
            let examples: Vec<String> = examples
                .iter()
                .map(|x| {
                    let value = message_value(&x.value, |id| {
                        entity(&ErrorEntity {
                            id,
                            formatted: labels.get(&id).cloned().unwrap_or_default(),
                        })
                    });
                    format!("{} @{} {value}", entity(&x.entity), x.tick.0)
                })
                .collect();
            format!(
                "{count} unconvertible change points migrating :{} ({} -> {}): {}",
                escape::value(attr),
                from.name(),
                to.name(),
                examples.join(", ")
            )
        }
        E::PairsBothWays { attr, pairs } => {
            let pairs: Vec<String> = pairs
                .iter()
                .map(|(a, b)| format!("{} and {}", entity(a), entity(b)))
                .collect();
            format!(
                "cannot migrate :{}: pairs have history in both directions: {}",
                escape::value(attr),
                pairs.join(", ")
            )
        }
        _ => return core_err(e),
    };
    CliError::core(&e, msg)
}

/// A value for a one-line message, as `kala asof` shows it (refs by
/// `entity`); a string over [`escape::MAX_VALUE_CHARS`] chars is cut and
/// gets `...`.
fn message_value(v: &Value, entity: impl Fn(EntityId) -> String) -> String {
    match v {
        Value::Str(s) => match s.char_indices().nth(escape::MAX_VALUE_CHARS) {
            Some((cut, _)) => format!("{}...", escape::quoted(&s[..cut])),
            None => escape::quoted(s),
        },
        Value::Ref(id) => entity(*id),
        Value::F64(f) if f.is_infinite() => (if *f > 0.0 { "inf" } else { "-inf" }).into(),
        Value::F64(f) => f.to_string(),
        Value::I64(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
    }
}

fn core_err(e: kala_core::Error) -> CliError {
    if matches!(e, kala_core::Error::SameTickConflict { .. }) {
        user(format!("same-tick conflict: {e}"))
    } else {
        read_core_err(e)
    }
}
