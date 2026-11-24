//! Genesis-only creation and atomic, validated script restore.
use super::{transact, transfer};
use crate::{
    cli::{CreateArgs, TransactArgs},
    error::CliError,
    escape, out,
};
use kala_core::{AttrDef, Cardinality, Kind, Schema, World};
use kala_datalog::ast::{WriteOp, WriteTxn};
use serde_json::json;
use std::path::Path;

fn user(s: &str) -> CliError {
    CliError::User(s.into())
}
fn schema(ops: impl IntoIterator<Item = WriteOp>) -> Result<Schema, CliError> {
    let mut schema = Schema::new();
    // Like a live `[:world-setting]`, a setting that changes nothing is a
    // no-op, not a log entry (the default schema mode is infer).
    let mut settings = std::collections::HashMap::from([("schema".to_owned(), "infer".to_owned())]);
    for op in ops {
        match op {
            WriteOp::NodeType {
                name,
                inferred: false,
                ..
            } => schema = schema.node_type(&name),
            WriteOp::Attribute {
                name,
                value_type,
                options,
                inferred: false,
                ..
            } => {
                let mut def = AttrDef::new(
                    &name,
                    value_type,
                    options.kind.unwrap_or(Kind::State),
                    options.cardinality.unwrap_or(Cardinality::One),
                );
                if let Some(v) = options.direction {
                    def.direction = Some(v);
                }
                if let Some(v) = options.indexed {
                    def.indexed = v;
                }
                def.inverse = options.inverse;
                def.reverse_cardinality = options.reverse_cardinality;
                schema = schema.attribute(def);
            }
            WriteOp::WorldSetting { key, value, .. } => {
                if settings.get(&key) != Some(&value) {
                    schema = schema.setting(&key, &value);
                    settings.insert(key, value);
                }
            }
            _ => {
                return Err(user(
                    "genesis accepts only declared node types, attributes and world settings",
                ));
            }
        }
    }
    Ok(schema)
}
fn reserved(ops: &[WriteOp]) -> bool {
    let reserved = |name: &str| name.starts_with(kala_core::RESERVED_NAME_PREFIX);
    ops.iter().any(|op| match op {
        WriteOp::NodeType { name, .. } => reserved(name),
        WriteOp::Attribute { name, options, .. } => {
            reserved(name) || options.inverse.as_deref().is_some_and(reserved)
        }
        _ => false,
    })
}
fn reserved_error() -> CliError {
    user(&format!(
        "names starting with {} are reserved for Kala",
        kala_core::RESERVED_NAME_PREFIX
    ))
}
pub fn run(args: &CreateArgs) -> Result<String, CliError> {
    let (path, source) = (args.world.as_path(), args.schema.as_deref());
    transfer::absent(path)?;
    let mut ops = Vec::new();
    if let Some(source) = source {
        let text = transfer::input(source)?;
        let forms: Vec<_> = kala_datalog::restore_forms(&text)
            .collect::<Result<_, _>>()
            .map_err(|e| user(&e.to_string()))?;
        if forms.iter().any(|f| f.genesis) && (forms.len() != 1 || !forms[0].genesis) {
            return Err(user("a genesis wrapper must be the only schema form"));
        }
        for form in forms {
            if form.at.is_some() {
                return Err(user("schema forms cannot set a tick"));
            }
            ops.extend(form.ops);
        }
    }
    // Genesis may hold reserved names only when restoring a dump of Kala's
    // own accounts database; a user schema never may.
    if reserved(&ops) {
        return Err(reserved_error());
    }
    let stage = transfer::Staging::new(path)?;
    let world =
        World::create(&stage.path, schema(ops)?).map_err(|e| CliError::at_path(path, &e))?;
    transact::refuse_system(&world)?;
    if path.file_name().is_some_and(|n| n == "system.kala") {
        return Err(user(
            "system is reserved; use kala user or transact --create to restore accounts",
        ));
    }
    drop(world);
    stage.place(path, false, false)?;
    eprintln!("event=world_created");
    Ok(format!("created {}\n", escape::path(path)))
}
/// `kala transact --create`: replays a dump into a new world.
pub fn replay(args: &TransactArgs) -> Result<String, CliError> {
    let (path, source, as_json) = (args.world.as_path(), args.input.as_path(), args.json);
    transfer::absent(path)?;
    let text = transfer::input(source)?;
    let mut forms = kala_datalog::restore_forms(&text);
    let (stage, mut world, system) = restore_genesis(path, forms.next())?;
    restore_transactions(&mut world, forms, system)?;
    let transactions = world.latest_tx().0;
    let entries = world.entries().count();
    place_restore(path, stage, world, system)?;
    restored_report(path, transactions, entries, system, as_json)
}

fn restore_genesis(
    path: &Path,
    first: Option<Result<WriteTxn, kala_datalog::DatalogError>>,
) -> Result<(transfer::Staging, World, bool), CliError> {
    // Diagnostics never echo the content of an accounts database dump (it
    // holds password hashes). Until the genesis says what the dump is, a
    // failure is reported without content, even for a malicious input that
    // fails before declaring the system marker.
    let withheld = || {
        user(
            "invalid restore form (content withheld); expected a valid genesis followed by write transactions",
        )
    };
    let first = first
        .ok_or_else(|| user("dump must start with [:genesis ...]"))?
        .map_err(|_| withheld())?;
    if !first.genesis {
        return Err(user("dump must start with [:genesis ...]"));
    }
    let reserved_names = reserved(&first.ops);
    let stage = transfer::Staging::new(path)?;
    let genesis =
        schema(first.ops).map_err(|_| user("invalid genesis schema (content withheld)"))?;
    let world = World::create(&stage.path, genesis).map_err(|_| user("cannot create restore world: invalid genesis or unavailable storage (content withheld)"))?;
    let system = kala_system::is_system(&world);
    // Only Kala's own accounts database may carry reserved names.
    if reserved_names && !system {
        return Err(reserved_error());
    }
    Ok((stage, world, system))
}

fn restore_transactions(
    world: &mut World,
    forms: impl Iterator<Item = Result<WriteTxn, kala_datalog::DatalogError>>,
    system: bool,
) -> Result<(), CliError> {
    // An ordinary world's dump holds no secrets: name the failing form and
    // the reason, so an author can fix an edited dump.
    let failed = |i: usize, e: &str| {
        if system {
            CliError::User(format!(
                "restore transaction {i} failed validation or storage write (content withheld)"
            ))
        } else {
            CliError::User(format!("restore transaction {i}: {e}"))
        }
    };
    for (i, form) in forms.enumerate() {
        let form: WriteTxn = form.map_err(|e| failed(i + 1, &e.to_string()))?;
        if form.genesis {
            return Err(user("genesis must be the first and only genesis form"));
        }
        transact::execute_restore(world, &form, None).map_err(|e| failed(i + 1, e.message()))?;
    }
    Ok(())
}

fn place_restore(
    path: &Path,
    stage: transfer::Staging,
    world: World,
    system: bool,
) -> Result<(), CliError> {
    if path.file_name().is_some_and(|n| n == "system.kala") && !system {
        return Err(user("system.kala requires an accounts database dump"));
    }
    if system {
        kala_system::validate_restore(&world)
            .map_err(|_| user("invalid accounts database restore"))?;
    }
    drop(world);
    // An accounts database is private from the start: the staging directory
    // is 0700, so narrowing the file here leaves no wider-permission window.
    #[cfg(unix)]
    if system {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stage.path, std::fs::Permissions::from_mode(0o600))
            .map_err(transfer::io)?;
    }
    stage.place(path, false, system)?;
    Ok(())
}

fn restored_report(
    path: &Path,
    transactions: u64,
    entries: usize,
    system: bool,
    as_json: bool,
) -> Result<String, CliError> {
    eprintln!("event=world_restored transactions={transactions} entries={entries} system={system}");
    if system {
        eprintln!("warning: restored an accounts database; protect it like the server directory");
    }
    if as_json {
        Ok(out::to_json(
            &json!({"command":"transact","create":true,"transactions":transactions,"entries":entries,"world":path.to_string_lossy()}),
        ))
    } else {
        Ok(format!(
            "replayed {transactions} transactions, {entries} entries into {}\n",
            escape::path(path)
        ))
    }
}
