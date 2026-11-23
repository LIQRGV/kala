//! Read-only schema inspection and write-report presentation.
use super::read::Ctx;
use crate::{cli::SchemaArgs, error::CliError, escape, out};
use kala_core::{AttrDef, Cardinality, Kind, RefDirection, SchemaChange, ValueType};
use serde_json::{Value, json};
use std::fmt::Write as _;

pub(super) fn type_name(t: ValueType) -> &'static str {
    match t {
        ValueType::Bool => "bool",
        ValueType::I64 => "i64",
        ValueType::F64 => "f64",
        ValueType::Str => "string",
        ValueType::Ref => "ref",
    }
}
pub(super) fn cardinality(c: Cardinality) -> &'static str {
    match c {
        Cardinality::One => "one",
        Cardinality::Many => "many",
    }
}

pub fn run(args: &SchemaArgs) -> Result<String, CliError> {
    let ctx = Ctx::open(&args.world, args.canon)?;
    let view = ctx.view();
    if args.json {
        return Ok(out::to_json(&document(&ctx.world, ctx.canon)));
    }
    let mut text = format!("schema at canon tx#{}\nnode types:\n", ctx.canon.0);
    for (name, inferred, first) in view.node_type_definitions() {
        let _ = writeln!(
            text,
            "  {} ({}, tx#{})",
            escape::value(name),
            if inferred { "inferred" } else { "declared" },
            first.0
        );
    }
    text.push_str("attributes:\n");
    for d in view.attributes() {
        let _ = write!(text, "  {}", def_text(d));
        let _ = writeln!(
            text,
            " ({}, tx#{})",
            if d.inferred { "inferred" } else { "declared" },
            ctx.world.attribute_first_tx(&d.name).map_or(0, |tx| tx.0)
        );
    }
    text.push_str("world settings:\n");
    for (key, value) in view.settings() {
        let _ = writeln!(text, "  :{} :{}", escape::value(key), escape::value(value));
    }
    Ok(text)
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::State => "state",
        Kind::Event => "event",
    }
}
fn direction_name(d: RefDirection) -> &'static str {
    match d {
        RefDirection::Directed => "directed",
        RefDirection::Bidirectional => "bidirectional",
    }
}

/// `:name type kind cardinality [direction] [inverse :x] [reverse c]
/// indexed|unindexed [deprecated]`: a definition's labels as `kala schema`
/// lists them (without origin and first tx), shared with `kala log`.
pub(super) fn def_text(d: &AttrDef) -> String {
    let mut text = format!(
        ":{} {} {} {}",
        escape::value(&d.name),
        type_name(d.value_type),
        kind_name(d.kind),
        cardinality(d.cardinality)
    );
    if let Some(direction) = d.direction {
        let _ = write!(text, " {}", direction_name(direction));
    }
    if let Some(inverse) = &d.inverse {
        let _ = write!(text, " inverse :{}", escape::value(inverse));
    }
    if let Some(reverse) = d.reverse_cardinality {
        let _ = write!(text, " reverse {}", cardinality(reverse));
    }
    text.push_str(if d.indexed { " indexed" } else { " unindexed" });
    if d.deprecated {
        text.push_str(" deprecated");
    }
    text
}

/// A definition's labels as JSON fields, named like the write report's
/// `attribute` record (`name`, `type`, `inferred`) plus the `kala schema`
/// labels. `kind` is the record's tag there, so state/event is
/// `attribute_kind`.
pub(super) fn def_json(d: &AttrDef) -> Value {
    json!({
        "name": d.name,
        "type": type_name(d.value_type),
        "attribute_kind": kind_name(d.kind),
        "cardinality": cardinality(d.cardinality),
        "direction": d.direction.map(direction_name),
        "inverse": d.inverse,
        "reverse_cardinality": d.reverse_cardinality.map(cardinality),
        "indexed": d.indexed,
        "deprecated": d.deprecated,
        "inferred": d.inferred,
    })
}

pub(super) fn change_json(change: &SchemaChange) -> Value {
    match change {
        SchemaChange::Attribute {
            name,
            value_type,
            inferred,
        } => {
            json!({"kind":"attribute","name":name,"type":type_name(*value_type),"inferred":inferred})
        }
        SchemaChange::NodeType { name, inferred } => {
            json!({"kind":"node_type","name":name,"inferred":inferred})
        }
        SchemaChange::Rename { old, new } => {
            json!({"kind":"rename","old":old,"new":new,"inferred":false})
        }
        SchemaChange::Deprecate { name } => {
            json!({"kind":"deprecate","name":name,"inferred":false})
        }
        SchemaChange::Cardinality { name, from, to } => {
            json!({"kind":"cardinality","name":name,"from":cardinality(*from),"to":cardinality(*to),"inferred":false})
        }
        SchemaChange::Setting { key, value } => {
            json!({"kind":"world_setting","key":key,"value":value,"inferred":false})
        }
        SchemaChange::Migration {
            name,
            from,
            to,
            report,
        } => {
            json!({"kind":"migrate","name":name,"from":type_name(*from),"to":type_name(*to),"change_points":report.change_points,"edges":report.edges,"edge_properties":report.edge_properties,"skipped":report.skipped,"dropped_inverse":report.dropped_inverse,"dropped_reverse_cardinality":report.dropped_reverse_cardinality.map(cardinality),"inferred":false})
        }
    }
}
pub(super) fn change_text(change: &SchemaChange) -> String {
    let origin = |inferred: bool| if inferred { "inferred" } else { "declared" };
    match change {
        SchemaChange::Attribute {
            name,
            value_type,
            inferred,
        } => format!(
            "declared :{} as {} ({})",
            escape::value(name),
            type_name(*value_type),
            origin(*inferred)
        ),
        SchemaChange::NodeType { name, inferred } => format!(
            "declared node type {} ({})",
            escape::value(name),
            origin(*inferred)
        ),
        SchemaChange::Rename { old, new } => {
            format!("renamed :{} to :{}", escape::value(old), escape::value(new))
        }
        SchemaChange::Deprecate { name } => format!("deprecated :{}", escape::value(name)),
        SchemaChange::Cardinality { name, from, to } => format!(
            "cardinality of :{}: {} -> {}",
            escape::value(name),
            cardinality(*from),
            cardinality(*to)
        ),
        SchemaChange::Setting { key, value } => format!(
            "world setting :{} :{}",
            escape::value(key),
            escape::value(value)
        ),
        SchemaChange::Migration {
            name,
            from,
            to,
            report,
        } => {
            let mut text = format!(
                "migrated :{} ({} -> {}): {} change points copied, {} edges created, {} edge-property facts copied, {} skipped",
                escape::value(name),
                type_name(*from),
                type_name(*to),
                report.change_points,
                report.edges,
                report.edge_properties,
                report.skipped
            );
            let mut dropped = Vec::new();
            if let Some(inverse) = &report.dropped_inverse {
                dropped.push(format!("inverse :{}", escape::value(inverse)));
            }
            if let Some(c) = report.dropped_reverse_cardinality {
                dropped.push(format!("reverse cardinality {}", cardinality(c)));
            }
            if !dropped.is_empty() {
                let _ = write!(text, "; dropped {}", dropped.join(" and "));
            }
            text
        }
    }
}

/// One pinned world's schema, shared with snapshot export.
pub(super) fn document(world: &kala_core::World, canon: kala_core::TxId) -> Value {
    let view = world.at_canon(canon);
    let nodes: Vec<_> = view
        .node_type_definitions()
        .map(|(name, inferred, first)| json!({"name":name,"inferred":inferred,"first_tx":first.0}))
        .collect();
    let attrs:Vec<_>=view.attributes().map(|d|json!({
        "name":d.name,"type":type_name(d.value_type),"kind":if d.kind==Kind::State {"state"} else {"event"},
        "cardinality":cardinality(d.cardinality),"direction":d.direction.map(|v|if v==RefDirection::Directed {"directed"} else {"bidirectional"}),
        "inverse":d.inverse,"reverse_cardinality":d.reverse_cardinality.map(cardinality),"indexed":d.indexed,"deprecated":d.deprecated,
        "inferred":d.inferred,"first_tx":world.attribute_first_tx(&d.name).map(|tx|tx.0)
    })).collect();
    json!({"command":"schema","canon":canon.0,"node_types":nodes,"attributes":attrs,"settings":view.settings()})
}
