//! Terminal writes are adapters over the native transaction API.
use super::{Done, query::read_text_file, read::core_err as read_core_err};
use crate::error::{CONFLICT_ADVICE, CliError, Conflicted};
use crate::{cli::TransactArgs, escape, names, out};
use kala_core::{Concurrency, Conflict, ConflictKey, EntityId, Tick, Value, World};
use kala_datalog::ast::WriteTxn;
use kala_lang::write::{WriteError, WritePolicy, WriteReport};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

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
/// The latest tick another writer moved while the transaction ran.
#[derive(Serialize)]
struct TickMoved {
    /// The latest tick the transaction began with: where its tick-less
    /// writes are.
    from: i64,
    /// The latest tick it was committed over.
    to: i64,
    #[serde(skip)]
    line: String,
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
    /// Present only when another writer's commit moved the world's latest
    /// tick while this transaction ran (its tick-less writes stayed on the
    /// tick it began with).
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_tick_moved: Option<TickMoved>,
    schema: Vec<serde_json::Value>,
    #[serde(skip)]
    schema_text: Vec<String>,
}

/// Opens `path` for writing. A damaged last transaction (fully present,
/// checksum fails) may have been acknowledged to a writer, so it is never
/// cut without `acknowledge`: kala-core refuses it under the writer lock,
/// before anything is cut or quarantined, so retrying can't get past it.
/// With `acknowledge` it is cut (to a quarantine file) here, before any
/// form runs. System worlds are refused once the writer has opened and
/// before anything is cut.
///
/// The open takes the world's writer lock (so does each later commit),
/// waiting for another writer up to `busy_timeout` (`--busy-timeout`;
/// `None` is kala-core's default, 5 s).
pub fn open_write(
    path: &Path,
    acknowledge: bool,
    busy_timeout: Option<Duration>,
) -> Result<World, CliError> {
    let wait = busy_timeout.unwrap_or(kala_core::log::DEFAULT_BUSY_TIMEOUT);
    let opened = if acknowledge {
        World::open_waiting(path, wait)
    } else {
        World::open_refusing_damaged_waiting(path, wait)
    };
    let mut w = opened.map_err(|e| match e {
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
        // The open only reported the damaged tail (kala-core leaves it in
        // the file until a handle that acknowledged it cuts it). The flag
        // is the acknowledgement: cut it now, under the writer lock, so
        // the command leaves the world recovered whatever its forms do.
        w.acknowledge_recovery();
        w.cut_acknowledged_tail()
            .map_err(|e| CliError::at_path(path, &e))?;
    }
    Ok(w)
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
/// `mode` is the concurrency mode of a form that doesn't begin with
/// `[:concurrency …]`. Another process may commit to the world between
/// this form's start and its commit. kala-core then commits the form over
/// those transactions (final IDs, the form's own latest tick: see
/// [`committed`]) unless, in optimistic mode, one of them wrote something
/// the form read (a `:set`, a conditional create): that is a conflict
/// ([`CliError::Conflict`], exit 4), nothing is written, and the message
/// says which op read what and to run the form again.
pub fn execute(
    w: &mut World,
    form: &WriteTxn,
    default: Option<Tick>,
    mode: Concurrency,
) -> Result<Report, CliError> {
    refuse_system(w)?;
    let outcome = kala_lang::write::execute_in(w, form, default, WritePolicy::default(), mode);
    committed(w, outcome)
}
/// What the applier made of a form, as the CLI reports it: the report of a
/// committed form, or its error in the CLI's terms. `w` is the world the
/// form was applied to, after the attempt (a refused commit leaves it
/// caught up, so the names in a conflict resolve).
fn committed(w: &World, outcome: Result<WriteReport, WriteError>) -> Result<Report, CliError> {
    match outcome {
        Ok(report) => Ok(committed_report(w, report)),
        Err(WriteError {
            operation,
            cause: kala_lang::write::Failure::Store(kala_core::Error::Conflict(conflict)),
        }) => Err(conflict_err(w, operation, &conflict)),
        Err(e) => Err(write_err(e)),
    }
}
/// A refused optimistic commit, in the author's terms: which op of the
/// form read what, that another transaction changed it, and that running
/// the form again is safe. Names are the caller's own (the handles and
/// attribute names the form used), never what the other transaction wrote.
fn conflict_err(w: &World, op: Option<(usize, &'static str)>, conflict: &Conflict) -> CliError {
    let entity = |id: EntityId| match w.handle_of(id) {
        Some(handle) => format!(":{}", escape::label(handle)),
        None => w.format_id(id),
    };
    let attr = |name: &str| format!(":{}", escape::value(name));
    let who = match op {
        Some((index, verb)) => format!("op {index} (:{verb})"),
        None => "this transaction".to_string(),
    };
    let what = match &conflict.key {
        ConflictKey::EntityAttribute { entity: e, attr: a } => format!(
            "read {} {}, which another transaction changed",
            entity(*e),
            attr(a)
        ),
        ConflictKey::Reverse { target, attr: a } => format!(
            "read what points at {} through {}, which another transaction changed",
            entity(*target),
            attr(a)
        ),
        ConflictKey::Value { attr: a } => format!(
            "looked up a value of {}, which another transaction wrote",
            attr(a)
        ),
        ConflictKey::Attribute { attr: a } => {
            format!("read {}, which another transaction changed", attr(a))
        }
        ConflictKey::TypeScan { node_type } => format!(
            "read every {} entity, and another transaction created one",
            escape::value(node_type)
        ),
        ConflictKey::AnyDefine => {
            "read the world's entities, and another transaction created one".to_string()
        }
        ConflictKey::Handle { handle } => format!(
            "found the handle :{} free, and another transaction created it",
            escape::label(handle)
        ),
        ConflictKey::Entry { entry } => format!(
            "read entry#{}, which another transaction corrected",
            entry.0
        ),
        ConflictKey::Undeclared { name } => format!(
            "found {} undeclared, and another transaction declared it",
            attr(name)
        ),
        ConflictKey::All => "read the whole world, which another transaction changed".to_string(),
        ConflictKey::ExposedId { id } => format!(
            "handed out {}, which a commit over another transaction would renumber",
            w.format_id(*id)
        ),
    };
    CliError::Conflict(Box::new(Conflicted {
        message: format!("conflict: {who} {what}; {CONFLICT_ADVICE}"),
        key: conflict.key.kind(),
        op,
        applied: conflict.applied,
    }))
}
/// The JSON document `kala transact --json` prints on stdout for a form
/// refused by a conflict (the message also goes to stderr, like every
/// error): which form to run again, and what was invalidated.
fn conflict_json(form: usize, prior: usize, conflict: &Conflicted) -> String {
    out::to_json(&serde_json::json!({
        "form": form,
        "error": "conflict",
        "key": conflict.key,
        "op": conflict.op.map(|(position, _)| position),
        "kind": conflict.op.map(|(_, kind)| kind),
        "applied": conflict.applied,
        "prior_forms_committed": prior,
        "message": conflict.message,
    }))
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
    let policy = WritePolicy {
        allow_system: system,
    };
    // As `kala_lang::write::execute_with`, on a transaction that may write
    // back a world-setting key the dumped world's log held although a live
    // write no longer accepts it (replay accepts it too; see
    // `kala_core::WORLD_SETTING_KEYS`). Only this path does: the world is
    // a new private file being rebuilt from a dump.
    let outcome = {
        let mut tx = w.begin();
        tx.set_concurrency(form.concurrency.unwrap_or_default());
        tx.set_unknown_settings_allowed(true);
        kala_lang::write::stage_with(&mut tx, form, default, policy)
            .and_then(|staged| kala_lang::write::commit(tx, staged))
    };
    committed(w, outcome)
}
fn write_err(e: WriteError) -> CliError {
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
    error.map_message(|message| format!("op {index} (:{verb}): {message}"))
}

/// The report of a committed form: each op gets the entries it stored and
/// the entity it created.
///
/// If other writers committed while the form ran, the commit renumbered
/// what the form had allocated provisionally; the report holds only the
/// committed IDs (`result.ops` and `result.edges` are final, and `w` is
/// the committed world they are formatted against), plus the warning that
/// the latest tick moved, when it did.
fn committed_report(w: &World, result: WriteReport) -> Report {
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
        latest_tick_moved: result.latest_tick_moved.map(|moved| TickMoved {
            from: moved.from.0,
            to: moved.to.0,
            line: moved.to_string(),
        }),
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
    if let Some(moved) = &report.latest_tick_moved {
        let _ = writeln!(s, "warning: {}", moved.line);
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
    let mode = crate::cli::concurrency(&args.concurrency);
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
                Some(copy) => execute(copy, &form, None, mode),
                None => execute(&mut w, &form, None, mode),
            }
        };
        let mut report = match action() {
            Ok(report) => report,
            Err(e) => {
                let e = form_failed(i + 1, count, dry_run, e);
                if let (true, CliError::Conflict(conflict)) = (json, &e) {
                    print(&conflict_json(i + 1, count, conflict))?;
                }
                return Err(e);
            }
        };
        report.form = i + 1;
        report.dry_run = dry_run;
        print(&render(&report, json))?;
        count += 1;
    }
    if count == 0 {
        return Err(user("expected at least one write transaction"));
    }
    Ok(String::new().into())
}

/// Prints one form's result on stdout at once, so the forms committed
/// before a later failure are already reported.
fn print(text: &str) -> Result<(), CliError> {
    let text = escape::text(text);
    io::stdout()
        .write_all(text.as_bytes())
        .and_then(|()| io::stdout().flush())
        .map_err(|e| CliError::Store(format!("writing transaction result: {e}")))
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
        open_write(path, args.acknowledge_recovery, args.busy_timeout)?
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
/// `count` forms before it. The error keeps its kind (a store error stays
/// exit 3, a conflict exit 4). A conflict's "run it again" is about this
/// form: when forms before it were committed, the message says where to
/// start again, since running the whole file would apply those twice.
fn form_failed(form: usize, count: usize, dry_run: bool, e: CliError) -> CliError {
    let rerun = match &e {
        CliError::Conflict(_) if count > 0 && !dry_run => format!(
            " (run form {form} and those after it: the {count} before it are already committed)"
        ),
        _ => String::new(),
    };
    e.map_message(|message| {
        format!(
            "form {form} failed; {count} prior forms {}: {message}{rerun}",
            if dry_run {
                "previewed (nothing committed)"
            } else {
                "committed"
            },
        )
    })
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

/// What `kala transact` and `kala repl --write` report when another writer
/// committed between a form's start and its commit. Two processes can't be
/// made to interleave there reliably, so these tests use two handles on one
/// world file in this process: the form is staged on one (what
/// [`execute`] does through the shared applier), the other commits, and
/// the first's commit goes through [`committed`], the function `execute`
/// reports with.
#[cfg(test)]
mod tests {
    use super::*;
    use kala_core::Schema;
    use kala_lang::write::{commit, parse_write, stage};
    use std::path::PathBuf;

    /// Two handles on a world with Greg in Varrow from 600 and a latest
    /// tick of 601.
    fn pair() -> (tempfile::TempDir, PathBuf, World, World) {
        let (dir, path, a, b, _) = seeded();
        (dir, path, a, b)
    }

    /// [`pair`], and the entry ID of "Greg in Varrow from 600".
    fn seeded() -> (tempfile::TempDir, PathBuf, World, World, u64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("world.kala");
        let mut a = World::create(&path, Schema::new()).unwrap();
        let seed = "[[:attribute :located_in :ref :one] [:attribute :tag :string :many]
              [:create :CHARACTER :greg] [:create :PLACE :varrow] [:create :PLACE :ashford]
              [:create :PLACE :carnath]
              [:assert :greg :located_in :varrow :at 600]
              [:assert :varrow :tag \"founded\" :at 601]]";
        let report = execute(
            &mut a,
            &parse_write(seed).unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .ok()
        .unwrap();
        let b = World::open(&path).unwrap();
        (dir, path, a, b, report.entries[0])
    }

    /// Stages `mine` on `b` in `mode`, commits `theirs` on `a` meanwhile,
    /// then commits `b`'s transaction and reports it as the CLI does.
    fn race(
        a: &mut World,
        b: &mut World,
        mine: &str,
        theirs: &str,
        mode: Concurrency,
    ) -> Result<Report, CliError> {
        let form = parse_write(mine).unwrap();
        let outcome = {
            let mut tx = b.begin();
            tx.set_concurrency(mode);
            let staged = stage(&mut tx, &form, None).unwrap();
            execute(
                a,
                &parse_write(theirs).unwrap(),
                None,
                Concurrency::Optimistic,
            )
            .unwrap();
            commit(tx, staged)
        };
        committed(b, outcome)
    }

    fn exit(e: &CliError) -> String {
        format!("{:?}", e.exit_code())
    }

    fn code(n: u8) -> String {
        format!("{:?}", std::process::ExitCode::from(n))
    }

    /// The other writer's form: Greg moves at the latest tick, and a later
    /// fact moves the latest tick on (so a tick-less re-run lands after it).
    const TO_ASHFORD: &str =
        "[[:set :greg :located_in :ashford] [:assert :ashford :tag \"busy\" :at 650]]";

    #[test]
    fn a_conflict_is_exit_four_and_says_which_op_read_what() {
        for (mine, theirs, key, op, want) in [
            (
                "[:set :greg :located_in :carnath]",
                TO_ASHFORD,
                "entity-attribute",
                (1, "set"),
                "conflict: op 1 (:set) read :greg :located_in, which another transaction \
                 changed; nothing was written; run it again",
            ),
            // The read is the second op's; the first is a blind write.
            (
                "[[:assert :varrow :tag \"x\" :at 700] [:set :greg :located_in :carnath]]",
                TO_ASHFORD,
                "entity-attribute",
                (2, "set"),
                "conflict: op 2 (:set) read :greg :located_in, which another transaction \
                 changed; nothing was written; run it again",
            ),
            (
                "[[:create :CHARACTER :lisa :if-missing] [:assert :lisa :tag \"b\"]]",
                "[:create :CHARACTER :lisa :if-missing]",
                "handle",
                (1, "create"),
                "conflict: op 1 (:create) found the handle :lisa free, and another \
                 transaction created it; nothing was written; run it again",
            ),
            // `ENTRY` is "Greg in Varrow from 600".
            (
                "[:correct ENTRY :replace :at 590]",
                "[:correct ENTRY :replace :at 595]",
                "entry",
                (1, "correct"),
                "conflict: op 1 (:correct) read entry#ENTRY, which another transaction \
                 corrected; nothing was written; run it again",
            ),
        ] {
            let (_dir, path, mut a, mut b, entry) = seeded();
            let named = |text: &str| text.replace("ENTRY", &entry.to_string());
            let (mine, theirs, want) = (&named(mine), &named(theirs), &named(want));
            let before = a.latest_tx();
            let e = race(&mut a, &mut b, mine, theirs, Concurrency::Optimistic)
                .err()
                .unwrap_or_else(|| panic!("{mine}: committed without a conflict"));
            assert_eq!(e.message(), want, "{mine}");
            assert_eq!(exit(&e), code(4), "{mine}");
            // Only the other writer's transaction is in the file.
            let on_disk = World::open_read_only(&path).unwrap();
            assert_eq!(on_disk.latest_tx().0, before.0 + 1, "{mine}");

            // In a file of forms: which form, what came before it, and
            // where to start again.
            let e = form_failed(2, 1, false, e);
            assert_eq!(
                e.message(),
                format!(
                    "form 2 failed; 1 prior forms committed: {want} (run form 2 and those \
                     after it: the 1 before it are already committed)"
                ),
                "{mine}"
            );
            assert_eq!(exit(&e), code(4), "{mine}: the kind survives the prefix");
            let CliError::Conflict(conflict) = &e else {
                panic!("{mine}: {e:?}");
            };
            let doc: serde_json::Value =
                serde_json::from_str(&conflict_json(2, 1, conflict)).unwrap();
            assert_eq!(
                doc,
                serde_json::json!({
                    "form": 2,
                    "error": "conflict",
                    "key": key,
                    "op": op.0,
                    "kind": op.1,
                    "applied": 1,
                    "prior_forms_committed": 1,
                    "message": e.message(),
                }),
                "{mine}"
            );

            // Running it again is safe: the form now sees the other commit.
            let again = execute(
                &mut b,
                &parse_write(mine).unwrap(),
                None,
                Concurrency::Optimistic,
            );
            let report = again.unwrap_or_else(|e| panic!("{mine}: {e:?}"));
            assert!(report.latest_tick_moved.is_none(), "{mine}");
        }
    }

    /// Every kind of invalidated read has its own sentence, in the names
    /// the author used (handles and attribute names), and its stable label
    /// for `--json`. Only `:set`, conditional creates and corrections
    /// record reads today; the query layers of later slices record the
    /// others.
    #[test]
    fn every_kind_of_read_has_a_sentence_and_a_json_label() {
        let (_dir, _path, a, _b) = pair();
        let greg = a.entity_by_handle("greg").unwrap();
        let varrow = a.entity_by_handle("varrow").unwrap();
        let attr = || "located_in".to_string();
        for (key, label, sentence) in [
            (
                ConflictKey::EntityAttribute {
                    entity: greg,
                    attr: attr(),
                },
                "entity-attribute",
                "read :greg :located_in, which another transaction changed",
            ),
            (
                ConflictKey::Reverse {
                    target: varrow,
                    attr: attr(),
                },
                "reverse",
                "read what points at :varrow through :located_in, which another transaction \
                 changed",
            ),
            (
                ConflictKey::Value { attr: attr() },
                "value",
                "looked up a value of :located_in, which another transaction wrote",
            ),
            (
                ConflictKey::Attribute { attr: attr() },
                "attribute",
                "read :located_in, which another transaction changed",
            ),
            (
                ConflictKey::TypeScan {
                    node_type: "CHARACTER".into(),
                },
                "type-scan",
                "read every CHARACTER entity, and another transaction created one",
            ),
            (
                ConflictKey::AnyDefine,
                "any-define",
                "read the world's entities, and another transaction created one",
            ),
            (
                ConflictKey::Handle {
                    handle: "lisa".into(),
                },
                "handle",
                "found the handle :lisa free, and another transaction created it",
            ),
            (
                ConflictKey::Entry {
                    entry: kala_core::EntryId(67),
                },
                "entry",
                "read entry#67, which another transaction corrected",
            ),
            (
                ConflictKey::Undeclared {
                    name: "mood".into(),
                },
                "undeclared",
                "found :mood undeclared, and another transaction declared it",
            ),
            (
                ConflictKey::All,
                "all",
                "read the whole world, which another transaction changed",
            ),
            (
                ConflictKey::ExposedId { id: greg },
                "exposed-id",
                "handed out CHARACTER#1, which a commit over another transaction would \
                 renumber",
            ),
        ] {
            let conflict = Conflict {
                key,
                operation: 0,
                applied: 3,
            };
            let e = conflict_err(&a, Some((2, "set")), &conflict);
            assert_eq!(
                e.message(),
                format!("conflict: op 2 (:set) {sentence}; nothing was written; run it again")
            );
            assert_eq!(exit(&e), code(4), "{label}");
            let CliError::Conflict(conflicted) = &e else {
                panic!("{e:?}");
            };
            assert_eq!((conflicted.key, conflicted.applied), (label, 3));
            // A read no op of the form made (none of today's forms).
            let e = conflict_err(&a, None, &conflict);
            assert_eq!(
                e.message(),
                format!("conflict: this transaction {sentence}; nothing was written; run it again")
            );
            let CliError::Conflict(conflicted) = &e else {
                panic!("{e:?}");
            };
            let doc: serde_json::Value =
                serde_json::from_str(&conflict_json(1, 0, conflicted)).unwrap();
            assert_eq!(doc["key"], label);
            assert!(doc["op"].is_null() && doc["kind"].is_null(), "{doc}");
        }
        // An entity without a handle is named by its ID; odd names are
        // escaped like every echoed value.
        let mut w = World::in_memory(Schema::new()).unwrap();
        execute(
            &mut w,
            &parse_write("[[:create :CHARACTER] [:attribute \"odd\\nname\" :string]]").unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .ok()
        .unwrap();
        let anonymous = w.parse_id("CHARACTER#1").unwrap();
        let e = conflict_err(
            &w,
            Some((1, "set")),
            &Conflict {
                key: ConflictKey::EntityAttribute {
                    entity: anonymous,
                    attr: "odd\nname".into(),
                },
                operation: 0,
                applied: 1,
            },
        );
        assert_eq!(
            e.message(),
            "conflict: op 1 (:set) read CHARACTER#1 :odd\\nname, which another transaction \
             changed; nothing was written; run it again"
        );
    }

    #[test]
    fn a_failed_form_keeps_its_kind_and_a_first_form_needs_no_restart_hint() {
        let conflict = |a: &World| {
            conflict_err(
                a,
                Some((1, "set")),
                &Conflict {
                    key: ConflictKey::All,
                    operation: 0,
                    applied: 1,
                },
            )
        };
        let (_dir, _path, a, _b) = pair();
        let e = form_failed(1, 0, false, conflict(&a));
        assert_eq!(
            e.message(),
            "form 1 failed; 0 prior forms committed: conflict: op 1 (:set) read the whole \
             world, which another transaction changed; nothing was written; run it again"
        );
        assert_eq!(exit(&e), code(4));
        for (e, want) in [
            (CliError::User("bad".into()), 1),
            (CliError::Store("broken".into()), 3),
        ] {
            let e = form_failed(3, 2, false, e);
            assert_eq!(exit(&e), code(want));
            assert!(
                e.message()
                    .starts_with("form 3 failed; 2 prior forms committed: b")
                    && !e.message().contains("run form"),
                "{}",
                e.message()
            );
        }
    }

    #[test]
    fn weak_mode_commits_the_same_forms() {
        let carnath = "[:set :greg :located_in :carnath]";
        for (mine, mode) in [
            (carnath, Concurrency::Weak),
            // The form's own mode wins over the caller's default.
            (
                "[[:concurrency :weak] [:set :greg :located_in :carnath]]",
                Concurrency::Optimistic,
            ),
        ] {
            let (_dir, _path, mut a, mut b) = pair();
            let report = race(&mut a, &mut b, mine, TO_ASHFORD, mode).unwrap();
            // The lost update is visible: an overlap warning, as for any
            // two values of a cardinality-one attribute.
            assert_eq!(report.changed, 1, "{mine}");
            assert_eq!(report.warnings.introduced.len(), 1, "{mine}");
            assert_eq!(report.ops[0].position, 1, "{mine}");
        }
        let (_dir, _path, mut a, mut b) = pair();
        let e = race(
            &mut a,
            &mut b,
            "[[:concurrency :optimistic] [:set :greg :located_in :carnath]]",
            TO_ASHFORD,
            Concurrency::Weak,
        )
        .err()
        .unwrap();
        assert_eq!(exit(&e), code(4));
    }

    #[test]
    fn a_moved_latest_tick_is_a_warning_line_and_a_json_field() {
        let (_dir, _path, mut a, mut b) = pair();
        let mut report = race(
            &mut a,
            &mut b,
            "[:assert :greg :tag \"mine\"]",
            "[:assert :varrow :tag \"later\" :at 610]",
            Concurrency::Optimistic,
        )
        .unwrap();
        report.form = 1;
        let text = render(&report, false);
        assert!(
            text.contains("  op 1 (:assert): tick 601, 1 changed"),
            "{text}"
        );
        assert_eq!(
            text.lines()
                .filter(|l| l.starts_with("warning: "))
                .collect::<Vec<_>>(),
            ["warning: latest tick moved 601 → 610 while this transaction ran"],
            "{text}"
        );
        // The line survives the escaping every printed line goes through.
        assert!(escape::text(&text).contains("601 → 610"));
        let doc: serde_json::Value = serde_json::from_str(&render(&report, true)).unwrap();
        assert_eq!(
            doc["latest_tick_moved"],
            serde_json::json!({"from": 601, "to": 610})
        );
        assert_eq!(doc["ops"][0]["tick"], 601);

        // An explicit tick is not affected, and neither is a commit nobody
        // raced: no line, no field.
        for (mine, theirs) in [
            (
                "[:assert :greg :tag \"explicit\" :at 601]",
                "[:assert :varrow :tag \"later still\" :at 620]",
            ),
            (
                "[:assert :greg :tag \"same tick\"]",
                "[:assert :varrow :tag \"not later\" :at 5]",
            ),
        ] {
            let (_dir, _path, mut a, mut b) = pair();
            let report = race(&mut a, &mut b, mine, theirs, Concurrency::Optimistic).unwrap();
            let text = render(&report, false);
            assert!(!text.contains("latest tick"), "{mine}: {text}");
            let doc: serde_json::Value = serde_json::from_str(&render(&report, true)).unwrap();
            assert!(doc.get("latest_tick_moved").is_none(), "{mine}: {doc}");
        }
        let report = execute(
            &mut b,
            &parse_write("[:assert :greg :tag \"alone\"]").unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .unwrap();
        assert!(!render(&report, false).contains("latest tick"));
    }

    #[test]
    fn a_report_over_newer_commits_shows_final_ids_only() {
        let (_dir, _path, mut a, mut b) = pair();
        let mine = parse_write(
            "[[:create :CHARACTER :kael] [:assert :kael :located_in :varrow :at 700]
              [:assert [:kael :located_in :varrow] :disguised true :at 700]]",
        )
        .unwrap();
        let (provisional, provisional_edge, outcome) = {
            let mut tx = b.begin();
            let staged = stage(&mut tx, &mine, None).unwrap();
            let entity = staged.ops[0].entity.unwrap();
            let varrow = tx.entity_by_handle("varrow").unwrap();
            let edge = tx.edge_id(entity, "located_in", varrow).unwrap();
            let names = (tx.format_id(entity), tx.format_id(edge.into()));
            // The other writer takes the same entity number and edge ID.
            execute(
                &mut a,
                &parse_write(
                    "[[:create :CHARACTER :lisa] [:assert :lisa :located_in :ashford :at 700]]",
                )
                .unwrap(),
                None,
                Concurrency::Optimistic,
            )
            .unwrap();
            (names.0, names.1, commit(tx, staged))
        };
        let report = committed(&b, outcome).unwrap();
        let kael = b.entity_by_handle("kael").unwrap();
        let varrow = b.entity_by_handle("varrow").unwrap();
        let edge = b.edge_id(kael, "located_in", varrow).unwrap();
        let (kael, edge) = (b.format_id(kael), b.format_id(edge.into()));
        assert_eq!(
            provisional,
            a.format_id(a.entity_by_handle("lisa").unwrap())
        );
        assert_ne!(kael, provisional, "the entity was renumbered");
        assert_ne!(edge, provisional_edge, "the edge was renumbered");

        let text = render(&report, false);
        assert!(
            text.contains(&format!(
                "  op 1 (:create): tick 601, 0 changed, created {kael}\n"
            )),
            "{text}"
        );
        assert!(text.contains(&format!("  new {edge}\n")), "{text}");
        assert!(!text.contains(&provisional), "{text}");
        assert!(!text.contains(&format!("{provisional_edge}\n")), "{text}");
        let doc: serde_json::Value = serde_json::from_str(&render(&report, true)).unwrap();
        assert_eq!(doc["ops"][0]["entity"], kael.as_str());
        assert_eq!(doc["edges"], serde_json::json!([edge]));
        let json = doc.to_string();
        assert!(!json.contains(&format!("\"{provisional}\"")), "{json}");
        assert!(!json.contains(&format!("\"{provisional_edge}\"")), "{json}");
        // The entries it lists are the ones in the log.
        let entries: Vec<u64> = doc["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_u64().unwrap())
            .collect();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            assert!(
                b.current_change(kala_core::EntryId(entry)).is_ok(),
                "entry#{entry}"
            );
        }
    }

    /// PR #20 review, item K: the chain read of a `:correct` belongs to
    /// that op, wherever it stands in its form.
    #[test]
    fn a_conflict_on_a_later_correct_names_that_op() {
        let (_dir, _path, mut a, mut b, entry) = seeded();
        let mine =
            format!("[[:assert :varrow :tag \"x\" :at 700] [:correct {entry} :replace :at 590]]");
        let theirs = format!("[:correct {entry} :replace :at 595]");
        let e = race(&mut a, &mut b, &mine, &theirs, Concurrency::Optimistic)
            .err()
            .expect("the corrections race");
        assert_eq!(
            e.message(),
            format!(
                "conflict: op 2 (:correct) read entry#{entry}, which another transaction \
                 corrected; nothing was written; run it again"
            )
        );
        assert_eq!(exit(&e), code(4));
        let CliError::Conflict(conflict) = &e else {
            panic!("{e:?}");
        };
        let doc: serde_json::Value = serde_json::from_str(&conflict_json(1, 0, conflict)).unwrap();
        assert_eq!(
            (&doc["op"], &doc["kind"], &doc["key"]),
            (
                &serde_json::json!(2),
                &serde_json::json!("correct"),
                &serde_json::json!("entry")
            )
        );
    }

    /// PR #20 review, item J: in weak mode a correction of an entry another
    /// writer corrected meanwhile is refused with the store's own error.
    #[test]
    fn a_weak_correct_over_another_correction_is_a_store_error_not_a_stacked_revision() {
        for theirs in [":replace :at 595", ":delete"] {
            let (_dir, path, mut a, mut b, entry) = seeded();
            let mine = format!("[:correct {entry} :replace :at 590]");
            let theirs = format!("[:correct {entry} {theirs}]");
            let before = a.latest_tx();
            let e = race(&mut a, &mut b, &mine, &theirs, Concurrency::Weak)
                .err()
                .expect("refused");
            assert_eq!(
                e.message(),
                format!(
                    "log entry#{entry} was already corrected by a newer transaction (now \
                     rev 1); nothing was written"
                ),
                "{theirs}"
            );
            let on_disk = World::open_read_only(&path).unwrap();
            assert_eq!(on_disk.latest_tx().0, before.0 + 1, "{theirs}");
        }
    }

    /// PR #20 review, item N: a conflict on an edge this form interned,
    /// which a newer commit interned too, names the committed edge.
    #[test]
    fn a_conflict_on_an_edge_interned_meanwhile_names_the_committed_edge() {
        let (_dir, _path, mut a, mut b) = pair();
        execute(
            &mut a,
            &parse_write("[[:attribute :present :ref :many] [:attribute :disguised :bool :one]]")
                .unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .ok()
        .unwrap();
        // The other writer first interns another edge (which takes the
        // number this form's edge has provisionally), then the same one.
        let e = race(
            &mut a,
            &mut b,
            "[[:assert :greg :present :carnath :at 1]
              [:set [:greg :present :carnath] :disguised true :at 1]]",
            "[[:assert :greg :present :ashford :at 1] [:assert :greg :present :carnath :at 1]
              [:assert [:greg :present :carnath] :disguised false :at 1]]",
            Concurrency::Optimistic,
        )
        .err()
        .expect("the edge's properties were read as a new edge's");
        let greg = a.entity_by_handle("greg").unwrap();
        let carnath = a.entity_by_handle("carnath").unwrap();
        let ashford = a.entity_by_handle("ashford").unwrap();
        let merged = a.format_id(a.edge_id(greg, "present", carnath).unwrap().into());
        let other = a.format_id(a.edge_id(greg, "present", ashford).unwrap().into());
        assert_eq!(
            e.message(),
            format!(
                "conflict: op 2 (:set) read {merged} :disguised, which another transaction \
                 changed; nothing was written; run it again"
            )
        );
        assert!(
            !e.message().contains(&format!("{other} ")),
            "{}",
            e.message()
        );
    }

    #[test]
    fn a_lock_held_through_the_busy_timeout_is_exit_three() {
        let (_dir, path, _a, mut b) = pair();
        let held = std::fs::File::open(&path).unwrap();
        held.try_lock().unwrap();
        b.set_busy_timeout(Duration::ZERO);
        let before = std::fs::read(&path).unwrap();
        let e = execute(
            &mut b,
            &parse_write("[:assert :greg :tag \"blocked\"]").unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .err()
        .unwrap();
        assert_eq!(exit(&e), code(3));
        assert_eq!(
            e.message(),
            "world is locked by another writer: a commit elsewhere held the writer lock for \
             the whole busy timeout; nothing was written; run the command again, or wait \
             longer with --busy-timeout"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_replaced_world_file_is_exit_three_and_says_to_run_again() {
        let (_dir, path, _a, mut b) = pair();
        // What `kala load --force` does to a world another process has open.
        std::fs::remove_file(&path).unwrap();
        drop(World::create(&path, Schema::new()).unwrap());
        let replaced = std::fs::read(&path).unwrap();
        const DIVERGED: &str = "the world file was cut back, rewritten or replaced by another \
             process (for example `kala load --force`) while this command had it open; \
             nothing was written; run the command again (in a REPL session: quit and start \
             it again)";
        let e = execute(
            &mut b,
            &parse_write("[:assert :greg :tag \"lost\"]").unwrap(),
            None,
            Concurrency::Optimistic,
        )
        .err()
        .unwrap();
        assert_eq!(exit(&e), code(3));
        assert_eq!(e.message(), DIVERGED);
        assert_eq!(std::fs::read(&path).unwrap(), replaced);
        // The REPL's catch-up before each command reports it with the path.
        let e = b
            .catch_up()
            .map_err(|e| CliError::at_path(Path::new("w.kala"), &e))
            .unwrap_err();
        assert_eq!(exit(&e), code(3));
        assert_eq!(e.message(), format!("w.kala: {DIVERGED}"));
    }

    #[test]
    fn recovery_errors_say_what_to_inspect_and_rerun() {
        use kala_core::Error as E;
        let at = |e: E| CliError::at_path(Path::new("w.kala"), &e);
        let e = at(E::RecoveryRequired { bytes: 57 });
        assert_eq!(exit(&e), code(3));
        assert_eq!(
            e.message(),
            "w.kala: the last transaction is damaged (57 bytes); writing to the world would \
             cut it to a quarantine file; nothing was written. Inspect it with `kala info`, \
             then rerun with --acknowledge-recovery"
        );
        let e = at(E::RecoveryUnacknowledged);
        assert_eq!(exit(&e), code(3));
        assert_eq!(
            e.message(),
            "w.kala: the world ends in a damaged last transaction that nobody has \
             acknowledged (it is still in the file, and is left out of the world); nothing \
             was written. Inspect it with `kala info`, then rerun with \
             --acknowledge-recovery, which cuts it to a quarantine file"
        );
        // Through a form's op, the prefix is kept and the kind too.
        let e = write_err(WriteError {
            operation: Some((2, "assert")),
            cause: kala_lang::write::Failure::Store(E::RecoveryRequired { bytes: 57 }),
        });
        assert_eq!(exit(&e), code(3));
        assert!(
            e.message()
                .starts_with("op 2 (:assert): the last transaction is damaged")
        );
        // A read-only world is still the user's mistake, not the store's.
        assert_eq!(exit(&at(E::ReadOnly)), code(1));
    }
}
