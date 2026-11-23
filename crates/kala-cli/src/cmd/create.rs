//! Genesis-only creation and atomic, validated script restore.
use super::{transact, transfer};
use crate::{error::CliError, escape, out};
use kala_core::{AttrDef, Cardinality, Kind, Schema, World};
use kala_datalog::ast::{WriteOp, WriteTxn};
use serde_json::json;
use std::path::Path;

fn user(s: &str) -> CliError {
    CliError::User(s.into())
}
fn schema(ops: impl IntoIterator<Item = WriteOp>) -> Result<Schema, CliError> {
    let mut schema = Schema::new();
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
            WriteOp::WorldSetting { key, value, .. } => schema = schema.setting(&key, &value),
            _ => {
                return Err(user(
                    "genesis accepts only declared node types, attributes and world settings",
                ));
            }
        }
    }
    Ok(schema)
}
pub fn run(path: &Path, source: Option<&Path>) -> Result<String, CliError> {
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
    stage.place(path, false)?;
    eprintln!("event=world_created");
    Ok(format!("created {}\n", escape::path(path)))
}
pub fn replay(path: &Path, source: &Path, as_json: bool) -> Result<String, CliError> {
    transfer::absent(path)?;
    let text = transfer::input(source)?;
    let mut forms = kala_datalog::restore_forms(&text);
    // Restore diagnostics never echo dump content: it may contain account
    // hashes, even when a malicious input fails before declaring its marker.
    let parse_error = |_: kala_datalog::DatalogError| {
        user(
            "invalid restore form (content withheld); expected a valid genesis followed by write transactions",
        )
    };
    let first = forms
        .next()
        .ok_or_else(|| user("dump must start with [:genesis ...]"))?
        .map_err(parse_error)?;
    if !first.genesis {
        return Err(user("dump must start with [:genesis ...]"));
    }
    let stage = transfer::Staging::new(path)?;
    let genesis =
        schema(first.ops).map_err(|_| user("invalid genesis schema (content withheld)"))?;
    let mut world = World::create(&stage.path, genesis).map_err(|_| user("cannot create restore world: invalid genesis or unavailable storage (content withheld)"))?;
    for (i, form) in forms.enumerate() {
        let form: WriteTxn = form.map_err(parse_error)?;
        if form.genesis {
            return Err(user("genesis must be the first and only genesis form"));
        }
        transact::execute_restore(&mut world, &form, None).map_err(|_| {
            CliError::User(format!(
                "restore transaction {} failed validation or storage write (content withheld)",
                i + 1
            ))
        })?;
    }
    let transactions = world.latest_tx().0;
    let entries = world.entries().count();
    let system = kala_bolt::system::is_system(&world);
    if path.file_name().is_some_and(|n| n == "system.kala") && !system {
        return Err(user("system.kala requires an accounts database dump"));
    }
    if system {
        kala_bolt::system::validate_restore(&world)
            .map_err(|_| user("invalid accounts database restore"))?;
    }
    drop(world);
    stage.place(path, false)?;
    eprintln!("event=world_restored transactions={transactions} entries={entries} system={system}");
    if system {
        eprintln!("warning: restored an accounts database; protect it like the server directory");
    }
    if as_json {
        Ok(out::to_json(
            &json!({"command":"transact","create":true,"transactions":transactions,"entries":entries,"world":path}),
        ))
    } else {
        Ok(format!(
            "replayed {transactions} transactions, {entries} entries into {}\n",
            escape::path(path)
        ))
    }
}
