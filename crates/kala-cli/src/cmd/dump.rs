//! A deterministic write-language representation of the committed log.
use super::{schema, transfer};
use crate::{error::CliError, escape, out};
use kala_core::log::EntryBody;
use kala_core::schema::SchemaEdit;
use kala_core::{AttrDef, AttrId, Kind, Op, RefDirection, Value, World};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

pub(super) fn quoted(value: &str) -> String {
    let mut text = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => text.push_str("\\\""),
            '\\' => text.push_str("\\\\"),
            '\n' => text.push_str("\\n"),
            '\r' => text.push_str("\\r"),
            '\t' => text.push_str("\\t"),
            c if kala_core::must_escape(c) => {
                if c as u32 <= 0xffff {
                    let _ = write!(text, "\\u{:04x}", c as u32);
                } else {
                    let _ = write!(text, "\\U{:08x}", c as u32);
                }
            }
            c => text.push(c),
        }
    }
    text.push('"');
    text
}
fn bare_name(value: &str) -> bool {
    !value.is_empty()
        && value.chars().all(|c| {
            c.is_alphanumeric()
                || matches!(
                    c,
                    '_' | '-' | '*' | '+' | '!' | '?' | '<' | '>' | '=' | '/' | '.'
                )
        })
}
fn name(value: &str) -> String {
    if bare_name(value) {
        format!(":{value}")
    } else {
        quoted(value)
    }
}
fn reference(world: &World, id: kala_core::EntityId) -> String {
    // Handles are permanent and unique, so they replay to the same ID and
    // keep the script readable; IDs are the fallback.
    if let Some(handle) = world.handle_of(id).filter(|h| bare_name(h)) {
        return format!(":{handle}");
    }
    let id = world.format_id(id);
    if id.rsplit_once('#').is_some_and(|(ty, _)| {
        ty.as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            && ty.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }) {
        id
    } else {
        format!("[:id {}]", quoted(&id))
    }
}
fn value(world: &World, v: &Value) -> String {
    match v {
        Value::Bool(v) => v.to_string(),
        Value::I64(v) => v.to_string(),
        Value::F64(v) if v.is_infinite() => {
            format!("[:f64 :{}]", if *v > 0.0 { "inf" } else { "-inf" })
        }
        Value::F64(v) => format!("{v:?}"),
        Value::Str(v) => quoted(v),
        Value::Ref(v) => reference(world, *v),
    }
}
fn options(def: &AttrDef) -> String {
    let mut s = format!(
        ":{} :{} :{} :{}",
        schema::type_name(def.value_type),
        if def.kind == Kind::State {
            "state"
        } else {
            "event"
        },
        schema::cardinality(def.cardinality),
        if def.indexed { "indexed" } else { "unindexed" }
    );
    if let Some(d) = def.direction {
        let _ = write!(
            s,
            " :{}",
            if d == RefDirection::Directed {
                "directed"
            } else {
                "bidirectional"
            }
        );
    }
    if let Some(n) = &def.inverse {
        let _ = write!(s, " :inverse {}", name(n));
    }
    if let Some(c) = def.reverse_cardinality {
        let _ = write!(s, " :reverse :{}", schema::cardinality(c));
    }
    s
}
fn attribute(def: &AttrDef) -> String {
    format!(
        "[:attribute {} {}{}{}]",
        name(&def.name),
        options(def),
        if def.direction == Some(RefDirection::Directed) && def.reverse_cardinality.is_none() {
            " :reverse :none"
        } else {
            ""
        },
        if def.inferred { " :inferred" } else { "" }
    )
}
fn schema_edit(edit: &SchemaEdit, attrs: &BTreeMap<AttrId, AttrDef>) -> String {
    match edit {
        SchemaEdit::NodeType {
            name: n, inferred, ..
        } => format!(
            "[:node-type {}{}]",
            name(n),
            if *inferred { " :inferred" } else { "" }
        ),
        SchemaEdit::Setting { key, value } => {
            format!("[:world-setting {} {}]", name(key), name(value))
        }
        SchemaEdit::Migration {
            source,
            def,
            skip_unconvertible,
        } => format!(
            "[:migrate {} {}{}]",
            name(&attrs[source].name),
            options(def),
            if *skip_unconvertible {
                " :skip-unconvertible"
            } else {
                ""
            }
        ),
        SchemaEdit::Attribute { id, def } => {
            if let Some(old) = attrs.get(id) {
                if old.name != def.name {
                    return format!("[:rename {} {}]", name(&old.name), name(&def.name));
                }
                if !old.deprecated && def.deprecated {
                    return format!("[:deprecate {}]", name(&def.name));
                }
                let mut relabel = old.clone();
                relabel.cardinality = def.cardinality;
                if &relabel == def {
                    return format!(
                        "[:cardinality {} :{}]",
                        name(&def.name),
                        schema::cardinality(def.cardinality)
                    );
                }
            }
            attribute(def)
        }
    }
}
fn script(world: &World) -> Result<String, CliError> {
    let copies = world
        .migration_copy_entries()
        .map_err(|_| CliError::Store("cannot reconstruct migration operations".into()))?;
    let entries: Vec<_> = world.entries().collect();
    let mut s = String::new();
    if kala_system::is_system(world) {
        s.push_str(";; WARNING: CONTAINS ACCOUNT PASSWORD HASHES. Protect this dump like the server directory.\n");
    }
    let _ = writeln!(
        s,
        ";; Kala replayable world dump\n;; Kala version: {}\n;; Source format version: {}\n;; Latest tx: {}; entries: {}\n;; Replay: kala transact --create NEW.kala DUMP\n[:genesis",
        env!("CARGO_PKG_VERSION"),
        kala_core::log::LOG_FORMAT_VERSION,
        world.latest_tx().0,
        entries.len()
    );
    let mut attrs = BTreeMap::new();
    let mut subjects = BTreeMap::new();
    let mut current_tx = 0;
    for (i, entry) in entries.iter().enumerate() {
        if entry.tx.0 != current_tx {
            current_tx = entry.tx.0;
            let _ = writeln!(s, ";; tx#{current_tx}\n[");
        }
        let generated = copies.contains(&entry.id);
        let op = if generated {
            None
        } else {
            match &entry.body {
                EntryBody::DefineNodeType { name: n, .. } => {
                    Some(format!("[:node-type {}]", name(n)))
                }
                EntryBody::DefineAttr { def, .. } => Some(attribute(def)),
                EntryBody::Schema(edit) => Some(schema_edit(edit, &attrs)),
                EntryBody::DefineEntity { id, handle } => {
                    let ty = world
                        .node_types()
                        .nth(usize::from(id.node_type().0))
                        .ok_or_else(|| CliError::Store("undefined node type".into()))?;
                    Some(format!(
                        "[:create {}{}]",
                        name(ty),
                        handle
                            .as_ref()
                            .map_or(String::new(), |h| format!(" {}", name(h)))
                    ))
                }
                EntryBody::Fact {
                    entity,
                    attr,
                    value: v,
                    tick,
                    op,
                    precision,
                } => Some(format!(
                    "[:{} {} {} {} :at {} :precision :{}]",
                    if *op == Op::Assert {
                        "assert"
                    } else {
                        "retract"
                    },
                    reference(world, *entity),
                    name(&attrs[attr].name),
                    value(world, v),
                    tick.0,
                    out::precision_str(*precision)
                )),
                EntryBody::Deleted { of, rev, .. } => {
                    let replacement = entries.get(i + 1).is_some_and(|next| matches!(next.body, EntryBody::Revised { of: n, rev: r, .. } if n == *of && r == *rev));
                    if replacement {
                        None
                    } else {
                        Some(format!("[:correct {} :delete]", of.0))
                    }
                }
                EntryBody::Revised {
                    of,
                    entity,
                    value: v,
                    tick,
                    op,
                    precision,
                    ..
                } => {
                    // A bidirectional correction can normalize the pair by
                    // swapping its subject and value. Correct takes the old
                    // subject implicitly, so undo that normalization when
                    // choosing the replacement reference for the script.
                    let replacement = if subjects.get(of).is_some_and(|old| old != entity) {
                        Value::Ref(*entity)
                    } else {
                        v.clone()
                    };
                    Some(format!(
                        "[:correct {} :replace :value {} :at {} :op :{} :precision :{}]",
                        of.0,
                        value(world, &replacement),
                        tick.0,
                        if *op == Op::Assert {
                            "assert"
                        } else {
                            "retract"
                        },
                        out::precision_str(*precision)
                    ))
                }
                EntryBody::Commit { .. } => {
                    s.push_str("]\n");
                    None
                }
                EntryBody::DefineEdge { .. } => None,
            }
        };
        if let Some(op) = op {
            let _ = writeln!(s, "  {op}");
        }
        // Migration copies are omitted as forms but their new names take
        // effect immediately, even before the end of this transaction.
        match &entry.body {
            EntryBody::Fact { entity, .. } => {
                subjects.insert(entry.id, *entity);
            }
            EntryBody::Revised { of, entity, .. } => {
                subjects.insert(*of, *entity);
            }
            EntryBody::DefineAttr { id, def }
            | EntryBody::Schema(SchemaEdit::Attribute { id, def }) => {
                attrs.insert(*id, def.clone());
            }
            _ => {}
        }
    }
    Ok(s)
}
pub(super) fn open(path: &Path) -> Result<World, CliError> {
    World::open_read_only(path).map_err(|e| {
        CliError::core(
            &e,
            "cannot read world: invalid log or unavailable file (content withheld)".into(),
        )
    })
}
pub(super) fn recovery(world: &World) {
    if world.recovery().bytes_cut != 0 {
        eprintln!(
            "event=dump_committed_prefix warning: omitted {} bytes of torn or damaged tail",
            world.recovery().bytes_cut
        );
    }
}
pub fn run(path: &Path, output: Option<&Path>, all: bool, force: bool) -> Result<String, CliError> {
    if all {
        let output =
            output.ok_or_else(|| CliError::Usage("dump --all requires -o OUTDIR".into()))?;
        if force {
            return Err(CliError::Usage(
                "--force is not supported with --all".into(),
            ));
        }
        return dump_all(path, output);
    }
    if force && output.is_none() {
        return Err(CliError::Usage("--force needs -o FILE".into()));
    }
    if let Some(output) = output {
        let source_meta = fs::metadata(path).map_err(transfer::io)?;
        if let Ok(target_meta) = fs::metadata(output) {
            #[cfg(unix)]
            let same = {
                use std::os::unix::fs::MetadataExt;
                source_meta.dev() == target_meta.dev() && source_meta.ino() == target_meta.ino()
            };
            #[cfg(not(unix))]
            let same = {
                let _ = (source_meta, target_meta);
                fs::canonicalize(path).ok() == fs::canonicalize(output).ok()
            };
            if same {
                return Err(CliError::Usage(
                    "dump output must differ from the source world".into(),
                ));
            }
        }
    }
    let world = open(path)?;
    if kala_system::is_system(&world) {
        return Err(CliError::User(
            "single-world dump refuses system accounts; use kala dump --all DIR -o OUTDIR".into(),
        ));
    }
    recovery(&world);
    let result = transfer::output(output, &script(&world)?, force)?;
    eprintln!(
        "event=world_dumped canon={} entries={}",
        world.latest_tx().0,
        world.entries().count()
    );
    Ok(result)
}
fn dump_all(directory: &Path, output: &Path) -> Result<String, CliError> {
    // A terminal slash or `/.` would make a symlink an intermediate
    // component, bypassing O_NOFOLLOW. Components removes those spellings
    // without following symlinks or changing the meaning of `..`.
    let normalized: std::path::PathBuf = output.components().collect();
    let output = normalized.as_path();
    let mut worlds = BTreeMap::new();
    for entry in fs::read_dir(directory).map_err(transfer::io)? {
        let entry = entry.map_err(transfer::io)?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(db) = file_name.strip_suffix(".kala") else {
            continue;
        };
        if kala_system::valid_database_name(db)
            && entry.file_type().map_err(transfer::io)?.is_file()
        {
            worlds.insert(db.to_owned(), entry.path());
        }
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(output) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(transfer::io(e)),
    }
    // On Unix, open the directory itself (never through a symlink) and pin
    // it, so swapping its pathname cannot redirect account hashes during the
    // backup. Other platforms cannot open a directory as a file and rely on a
    // trusted output parent.
    #[cfg(unix)]
    let dir = {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(output)
            .map_err(transfer::io)?;
        if !dir.metadata().map_err(transfer::io)?.is_dir() {
            return Err(CliError::User("OUTDIR must be a directory".into()));
        }
        dir
    };
    #[cfg(not(unix))]
    if !fs::symlink_metadata(output).map_err(transfer::io)?.is_dir() {
        return Err(CliError::User("OUTDIR must be a directory".into()));
    }
    #[cfg(target_os = "linux")]
    let stable = {
        use std::os::fd::AsRawFd;
        std::path::PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
    };
    #[cfg(not(target_os = "linux"))]
    let stable = output.to_path_buf();
    if fs::read_dir(&stable)
        .map_err(transfer::io)?
        .next()
        .is_some()
    {
        return Err(CliError::User("OUTDIR must be empty".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        dir.set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(transfer::io)?;
    }
    let mut summary = String::new();
    for (name, path) in worlds {
        let world = open(&path)?;
        recovery(&world);
        let text = script(&world)?;
        let mut file = transfer::private_file(&stable.join(format!("{name}.dump.edn")))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(transfer::io)?;
        eprintln!(
            "event=world_dumped canon={} entries={} system={}",
            world.latest_tx().0,
            world.entries().count(),
            kala_system::is_system(&world)
        );
        let _ = writeln!(
            summary,
            "dumped {name} into {}",
            escape::path(&output.join(format!("{name}.dump.edn")))
        );
    }
    #[cfg(unix)]
    dir.sync_all().map_err(transfer::io)?;
    Ok(summary)
}
