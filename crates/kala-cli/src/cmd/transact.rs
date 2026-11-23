//! Terminal writes are adapters over the native transaction API.
use super::{Done, query::read_text_file, read::core_err as read_core_err};
use crate::{error::CliError, escape, names, out};
use kala_core::{Correction, EntityId, Op, Precision, Tick, Txn, Value, World};
use kala_datalog::ast::{Item, WriteOp, WriteTxn, WriteValue};
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
    precision: &'static str,
    changed: usize,
    entries: Vec<u64>,
    entity: Option<String>,
}
#[derive(Serialize)]
struct Warnings {
    introduced: Vec<String>,
    resolved: Vec<String>,
}
#[derive(Serialize)]
pub struct Report {
    form: usize,
    dry_run: bool,
    tx: u64,
    changed: usize,
    entries: Vec<u64>,
    edges: Vec<String>,
    ops: Vec<Operation>,
    warnings: Warnings,
}

pub fn open_write(path: &Path) -> Result<World, CliError> {
    World::open(path).map_err(|e| match e {
        kala_core::Error::WorldLocked => CliError::Store(format!(
            "{}: world is locked by another writer",
            escape::path(path)
        )),
        e => CliError::at_path(path, &e),
    })
}
fn user(s: impl Into<String>) -> CliError {
    CliError::User(s.into())
}
fn kind(op: &WriteOp) -> &'static str {
    match op {
        WriteOp::Create { .. } => "create",
        WriteOp::Fact { op: Op::Assert, .. } => "assert",
        WriteOp::Fact {
            op: Op::Retract, ..
        } => "retract",
        WriteOp::Correct { .. } => "correct",
    }
}
fn remember(w: &World, item: &Item, refs: &mut HashMap<String, EntityId>) {
    let resolved = match item {
        Item::Keyword(s, _) => w.entity_by_handle(s).map(|id| (format!(":{s}"), id)),
        Item::Id(s, _) => w.parse_id(s).map(|id| (s.clone(), id)),
        _ => None,
    };
    if let Some((k, id)) = resolved {
        refs.insert(k, id);
    }
}
fn remember_value(w: &World, v: &WriteValue, refs: &mut HashMap<String, EntityId>) {
    match v {
        WriteValue::Scalar(i) => remember(w, i, refs),
        WriteValue::Edge { source, target, .. } => {
            remember(w, source, refs);
            remember(w, target, refs);
        }
    }
}
fn reference(i: &Item, refs: &HashMap<String, EntityId>) -> Result<EntityId, CliError> {
    let key = match i {
        Item::Keyword(s, _) => format!(":{s}"),
        Item::Id(s, _) => s.clone(),
        _ => return Err(user("expected a reference")),
    };
    refs.get(&key)
        .copied()
        .ok_or_else(|| user(format!("unknown entity `{}`", escape::value(&key))))
}
fn subject(
    tx: &Txn<'_>,
    v: &WriteValue,
    refs: &HashMap<String, EntityId>,
) -> Result<EntityId, CliError> {
    match v {
        WriteValue::Scalar(i) => reference(i, refs),
        WriteValue::Edge {
            source,
            attr,
            target,
        } => tx
            .edge_id(
                reference(source, refs)?,
                &attr.name,
                reference(target, refs)?,
            )
            .map(EntityId::from)
            .ok_or_else(|| user("unknown edge triple (the edge must already exist)")),
    }
}
fn value(
    tx: &Txn<'_>,
    v: &WriteValue,
    refs: &HashMap<String, EntityId>,
) -> Result<Value, CliError> {
    Ok(match v {
        WriteValue::Scalar(Item::Str(s, _)) => Value::Str(s.clone()),
        WriteValue::Scalar(Item::Int(n, _)) => Value::I64(*n),
        WriteValue::Scalar(Item::Float(n, _)) => Value::f64(*n).map_err(|e| user(e.to_string()))?,
        WriteValue::Scalar(Item::Bool(b, _)) => Value::Bool(*b),
        _ => Value::Ref(subject(tx, v, refs)?),
    })
}

/// Stage every op, then commit once. A resolution or native write error drops
/// the transaction, so even newly allocated handles are rolled back.
pub fn execute(w: &mut World, form: &WriteTxn, default: Option<Tick>) -> Result<Report, CliError> {
    let default = form.at.or(default);
    let fallback = default.unwrap_or_else(|| w.latest_tick());
    let mut refs = HashMap::new();
    for op in &form.ops {
        match op {
            WriteOp::Fact { subject, value, .. } => {
                remember_value(w, subject, &mut refs);
                remember_value(w, value, &mut refs);
            }
            WriteOp::Correct {
                replacement: Some(r),
                ..
            } => {
                if let Some(v) = &r.value {
                    remember_value(w, v, &mut refs);
                }
            }
            _ => {}
        }
    }
    // Corrections retain omitted precision, including deletions and no-ops.
    // Use the core's chain resolver; a bad target is still reported by its op.
    let precisions: Vec<_> = form
        .ops
        .iter()
        .map(|op| match op {
            WriteOp::Correct { entry, .. } => w.current_change(*entry).ok().map(|c| c.precision),
            _ => None,
        })
        .collect();
    let mut tx = w.begin();
    if let Some(t) = default {
        tx.set_default_tick(t);
    }
    let mut ops = Vec::new();
    let mut created = Vec::new();
    for (index, op) in form.ops.iter().enumerate() {
        let mut run = || -> Result<(usize, Tick, Precision, Option<EntityId>), CliError> {
            match op {
                WriteOp::Create {
                    node_type, handle, ..
                } => {
                    let id = tx
                        .create_entity(node_type, Some(handle))
                        .map_err(core_err)?;
                    refs.insert(format!(":{handle}"), id);
                    // Numeric IDs may also refer to entities created earlier in this transaction.
                    refs.insert(format!("{node_type}#{}", id.seq()), id);
                    Ok((0, fallback, Precision::Exact, Some(id)))
                }
                WriteOp::Fact {
                    op,
                    subject: s,
                    attr,
                    value: v,
                    at,
                    precision,
                    ..
                } => {
                    let e = subject(&tx, s, &refs)?;
                    let v = value(&tx, v, &refs)?;
                    let r = match op {
                        Op::Assert => tx.assert_with_precision(e, &attr.name, v, *at, *precision),
                        Op::Retract => tx.retract_with_precision(e, &attr.name, v, *at, *precision),
                    }
                    .map_err(core_err)?;
                    Ok((r.changed, r.tick, *precision, None))
                }
                WriteOp::Correct {
                    entry, replacement, ..
                } => {
                    let c = match replacement {
                        None => Correction::Delete,
                        Some(r) => Correction::Replace {
                            value: r.value.as_ref().map(|v| value(&tx, v, &refs)).transpose()?,
                            tick: r.at,
                            op: r.op,
                            precision: r.precision,
                        },
                    };
                    let r = tx.correct(*entry, c).map_err(core_err)?;
                    Ok((
                        r.changed,
                        r.tick,
                        replacement
                            .as_ref()
                            .and_then(|r| r.precision)
                            .or(precisions[index])
                            .unwrap_or(Precision::Exact),
                        None,
                    ))
                }
            }
        };
        let (changed, tick, precision, entity) = run().map_err(|e| {
            let msg = format!("op {} (:{}): {}", index + 1, kind(op), e.message());
            match e {
                CliError::Store(_) => CliError::Store(msg),
                _ => user(msg),
            }
        })?;
        created.push(entity);
        ops.push(Operation {
            position: index + 1,
            kind: kind(op),
            tick: tick.0,
            precision: out::precision_str(precision),
            changed,
            entries: Vec::new(),
            entity: None,
        });
    }
    let result = tx.commit().map_err(core_err)?;
    let mut ids = result.entries.iter();
    for (op, entity) in ops.iter_mut().zip(created) {
        op.entity = entity.map(|e| w.format_id(e));
        op.entries = ids.by_ref().take(op.changed).map(|id| id.0).collect();
    }
    Ok(Report {
        form: 1,
        dry_run: false,
        tx: result.tx.0,
        changed: result.changed,
        entries: result.entries.iter().map(|e| e.0).collect(),
        edges: result
            .edges
            .iter()
            .map(|e| w.format_id((*e).into()))
            .collect(),
        ops,
        warnings: Warnings {
            introduced: result
                .warnings
                .introduced
                .iter()
                .map(|o| names::overlap_line(w, o))
                .collect(),
            resolved: result
                .warnings
                .resolved
                .iter()
                .map(|o| names::overlap_line(w, o))
                .collect(),
        },
    })
}

pub fn render(report: &Report, json: bool) -> String {
    if json {
        return out::to_json(report);
    }
    let mut s = format!(
        "form {}: {}tx#{}: {} changed\n",
        report.form,
        if report.dry_run {
            "dry-run "
        } else {
            "committed "
        },
        report.tx,
        report.changed
    );
    for op in &report.ops {
        let _ = write!(s, "  op {} (:{}): tick {}", op.position, op.kind, op.tick);
        if op.precision != "exact" {
            let _ = write!(s, " ({})", op.precision);
        }
        let _ = write!(s, ", {} changed", op.changed);
        if let Some(e) = &op.entity {
            let _ = write!(s, ", created {e}");
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
    for o in &report.warnings.resolved {
        let _ = writeln!(s, "resolved warning: {o}");
    }
    s
}

pub fn run(path: &Path, input: &Path, dry_run: bool, json: bool) -> Result<Done, CliError> {
    let text = if input == Path::new("-") {
        let mut bytes = Vec::new();
        io::stdin()
            .take(kala_datalog::MAX_QUERY_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| user(format!("reading stdin: {e}")))?;
        if bytes.len() > kala_datalog::MAX_QUERY_BYTES {
            return Err(user("input exceeds the 65536-byte limit"));
        }
        String::from_utf8(bytes).map_err(|_| user("input is not UTF-8"))?
    } else {
        read_text_file(input, kala_datalog::MAX_QUERY_BYTES, "the transactions")?
    };
    let mut w = if dry_run {
        World::open_read_only(path).map_err(|e| CliError::at_path(path, &e))?
    } else {
        open_write(path)?
    };
    if dry_run && w.recovery().cause == Some(kala_core::CutCause::Damaged) {
        return Err(user(
            "dry-run refused: damaged log needs recovery acknowledgement before writes",
        ));
    }
    let forms = kala_datalog::write_forms(&text).map_err(|e| user(e.to_string()))?;
    let mut count = 0;
    for (i, form) in forms.enumerate() {
        let action = || -> Result<Report, CliError> {
            let form = form.map_err(|e| user(e.to_string()))?;
            if dry_run {
                execute(&mut w.preview_copy().map_err(core_err)?, &form, None)
            } else {
                execute(&mut w, &form, None)
            }
        };
        let mut report = action().map_err(|e| match e {
            CliError::Store(s) => CliError::Store(format!(
                "form {} failed; {count} prior forms committed: {s}",
                i + 1
            )),
            e => user(format!(
                "form {} failed; {count} prior forms {}: {}",
                i + 1,
                if dry_run {
                    "previewed (nothing committed)"
                } else {
                    "committed"
                },
                e.message()
            )),
        })?;
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

fn core_err(e: kala_core::Error) -> CliError {
    if matches!(e, kala_core::Error::SameTickConflict { .. }) {
        user(format!("same-tick conflict: {e}"))
    } else {
        read_core_err(e)
    }
}
