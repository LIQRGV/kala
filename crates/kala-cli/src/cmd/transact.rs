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
    /// The precision stored; `null` for a no-op, which stores nothing (a
    /// precision change on a held fact needs `:correct`).
    precision: Option<&'static str>,
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
    /// The committed transaction; `null` when the form changed nothing, so
    /// nothing was appended.
    tx: Option<u64>,
    changed: usize,
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
    let opened = if acknowledge {
        World::open(path)
    } else {
        World::open_refusing_damaged(path)
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
/// System worlds can be changed only through account management.
pub(crate) fn refuse_system(world: &World) -> Result<(), CliError> {
    if kala_bolt::system::is_system(world) {
        return Err(user("system world is managed only by `kala user`"));
    }
    Ok(())
}
fn user(s: impl Into<String>) -> CliError {
    CliError::User(s.into())
}
fn kind(op: &WriteOp) -> &'static str {
    match op {
        WriteOp::Attribute { .. } => "attribute",
        WriteOp::NodeType { .. } => "node-type",
        WriteOp::Rename { .. } => "rename",
        WriteOp::Deprecate { .. } => "deprecate",
        WriteOp::Cardinality { .. } => "cardinality",
        WriteOp::Migrate { .. } => "migrate",
        WriteOp::WorldSetting { .. } => "world-setting",
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
    refuse_system(w)?;
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
        let mut run = || -> Result<(usize, Tick, Option<Precision>, Option<EntityId>), CliError> {
            match op {
                WriteOp::Attribute {
                    name,
                    value_type,
                    options,
                    ..
                } => {
                    tx.declare_attribute_with(name, *value_type, options)
                        .map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::NodeType { name, .. } => {
                    tx.declare_node_type(name).map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::Rename { old, new, .. } => {
                    tx.rename_attribute(old, new).map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::Deprecate { name, .. } => {
                    tx.deprecate_attribute(name).map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::Cardinality {
                    name, cardinality, ..
                } => {
                    tx.set_cardinality(name, *cardinality).map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::WorldSetting { key, value, .. } => {
                    tx.world_setting(key, value).map_err(core_err)?;
                    Ok((0, fallback, None, None))
                }
                WriteOp::Migrate { name, options, .. } => {
                    let report = tx
                        .migrate_attribute(name, options.clone())
                        .map_err(|e| migrate_err(&tx, e))?;
                    Ok((
                        report.change_points + report.edge_properties,
                        fallback,
                        None,
                        None,
                    ))
                }
                WriteOp::Create {
                    node_type, handle, ..
                } => {
                    tx.infer_node_type(node_type).map_err(core_err)?;
                    let id = tx
                        .create_entity(node_type, Some(handle))
                        .map_err(core_err)?;
                    refs.insert(format!(":{handle}"), id);
                    // Numeric IDs may also refer to entities created earlier in this transaction.
                    refs.insert(format!("{node_type}#{}", id.seq()), id);
                    Ok((0, fallback, None, Some(id)))
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
                    if !tx
                        .infer_write_attribute(&attr.name, &v, *op)
                        .map_err(core_err)?
                    {
                        return Ok((0, at.unwrap_or(fallback), None, None));
                    }
                    let r = match op {
                        Op::Assert => tx.assert_with_precision(e, &attr.name, v, *at, *precision),
                        Op::Retract => tx.retract_with_precision(e, &attr.name, v, *at, *precision),
                    }
                    .map_err(core_err)?;
                    // A no-op stores nothing, so it reports no precision (changing a
                    // held fact's precision needs `:correct`).
                    Ok((
                        r.changed,
                        r.tick,
                        (r.changed > 0).then_some(*precision),
                        None,
                    ))
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
                    // The chain's stored precision, so a no-op or delete
                    // still reports the truth.
                    Ok((
                        r.changed,
                        r.tick,
                        Some(
                            replacement
                                .as_ref()
                                .and_then(|r| r.precision)
                                .or(precisions[index])
                                .unwrap_or(Precision::Exact),
                        ),
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
            precision: precision.map(out::precision_str),
            changed,
            entries: Vec::new(),
            entity: None,
        });
    }
    let created_any = created.iter().any(Option::is_some);
    let result = tx.commit().map_err(core_err)?;
    let mut ids = result.entries.iter();
    for (op, entity) in ops.iter_mut().zip(created) {
        op.entity = entity.map(|e| w.format_id(e));
        op.entries = ids.by_ref().take(op.changed).map(|id| id.0).collect();
    }
    Ok(Report {
        form: 1,
        dry_run: false,
        tx: (!result.entries.is_empty()
            || !result.edges.is_empty()
            || !result.schema.is_empty()
            || created_any)
            .then_some(result.tx.0),
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
    let mut s = match report.tx {
        Some(tx) => format!(
            "form {}: {}tx#{tx}: {} changed\n",
            report.form,
            if report.dry_run {
                "dry-run "
            } else {
                "committed "
            },
            report.changed
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

pub fn run(
    path: &Path,
    input: &Path,
    dry_run: bool,
    json: bool,
    acknowledge_recovery: bool,
) -> Result<Done, CliError> {
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
        open_write(path, acknowledge_recovery)?
    };
    refuse_system(&w)?;
    if dry_run && w.recovery().cause == Some(kala_core::CutCause::Damaged) {
        return Err(user(
            "dry-run refused: damaged log needs recovery acknowledgement before writes",
        ));
    }
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
        let mut report = action().map_err(|e| match e {
            CliError::Store(s) => CliError::Store(format!(
                "form {} failed; {count} prior forms {}: {s}",
                i + 1,
                if dry_run {
                    "previewed (nothing committed)"
                } else {
                    "committed"
                }
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

/// [`core_err`], with a migration refusal's entities shown by handle (else
/// `TYPE#n`) and its values as `kala asof` prints them (strings quoted and
/// escaped, cut like any echoed value). kala-core's own message has only IDs.
fn migrate_err(tx: &Txn, e: kala_core::Error) -> CliError {
    use kala_core::{Error as E, ErrorEntity};
    let entity = |e: &ErrorEntity| escape::label(tx.handle_of(e.id).unwrap_or(&e.formatted));
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
                            formatted: tx.format_id(id),
                        })
                    });
                    format!("{} @{} {value}", entity(&x.entity), x.tick.0)
                })
                .collect();
            format!(
                "{count} unconvertible change points migrating :{} ({} -> {}): {}",
                escape::value(attr),
                super::schema::type_name(*from),
                super::schema::type_name(*to),
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
