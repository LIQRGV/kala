//! Read-only, typed JSON snapshots (not an import format).
use super::{dump, read::core_err, schema, transfer};
use crate::{cli::Canon, error::CliError, out};
use kala_core::log::EntryBody;
use kala_core::{Cardinality, EntityId, Tick, Value};
use serde_json::{Map, Value as Json, json};
use std::path::Path;

fn attributes(labels: &out::Labeler<'_>, entity: EntityId) -> Result<Map<String, Json>, CliError> {
    let mut attributes = Map::new();
    for def in labels.view.attributes() {
        let values = labels
            .view
            .as_of(entity, &def.name, labels.tick)
            .map_err(core_err)?;
        if values.is_empty() {
            continue;
        }
        let values: Vec<_> = values.iter().map(|v| json!(labels.value(v))).collect();
        let value = if def.cardinality == Cardinality::One && values.len() == 1 {
            values.into_iter().next().unwrap_or(Json::Null)
        } else {
            Json::Array(values)
        };
        attributes.insert(def.name.clone(), value);
    }
    Ok(attributes)
}
pub fn run(
    path: &Path,
    at: Option<i64>,
    canon: Option<Canon>,
    output: Option<&Path>,
) -> Result<String, CliError> {
    let world = dump::open(path)?;
    if kala_system::is_system(&world) {
        return Err(CliError::User(
            "export refuses the system accounts database".into(),
        ));
    }
    dump::recovery(&world);
    let canon =
        kala_core::TxId(canon.map_or(world.latest_tx().0, |c| c.0.min(world.latest_tx().0)));
    let view = world.at_canon(canon);
    let tick = at.map(Tick).unwrap_or_else(|| view.latest_tick());
    let labels = out::Labeler::new(&world, canon, tick);
    let mut entities = Vec::new();
    let mut ids = Vec::new();
    let mut edges = Vec::new();
    for entry in world.entries().filter(|e| e.tx <= canon) {
        match entry.body {
            EntryBody::DefineEntity { id, .. } => ids.push(id),
            EntryBody::DefineEdge { id, .. } => edges.push(id),
            _ => {}
        }
    }
    ids.sort_unstable();
    edges.sort_unstable();
    for id in ids {
        let node_type = world.node_types().nth(usize::from(id.node_type().0));
        entities.push(json!({"id": world.format_id(id), "node_type":node_type,"handle":world.handle_of(id),"attributes":attributes(&labels, id)?}));
    }
    let mut relationships = Vec::new();
    for id in edges {
        let Some((source, attr, target)) = view.edge(id) else {
            continue;
        };
        if !view
            .as_of_value(source, &attr, &Value::Ref(target), tick)
            .map_err(core_err)?
        {
            continue;
        }
        relationships.push(json!({"id":world.format_id(id.into()),"source":labels.entity(source),"attribute":attr,"target":labels.entity(target),"properties":attributes(&labels,id.into())?}));
    }
    // Use exactly the schema inspection contract, including latest names
    // with definitions pinned to this canon.
    let schema = schema::document(&world, canon);
    let text = out::to_json(
        &json!({"kala_version":env!("CARGO_PKG_VERSION"),"format_version":kala_core::log::LOG_FORMAT_VERSION,"tick":tick.0,"canon":canon.0,"schema":{"node_types":schema["node_types"],"attributes":schema["attributes"],"settings":schema["settings"]},"entities":entities,"edges":relationships}),
    );
    transfer::output(output, &text, false)
}
