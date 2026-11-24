//! `kala log <world> [--from C] [--limit N]`: the log's transactions in tx
//! order with every entry, so an author can see which entry to `correct`.
//!
//! Opens the world with [`World::open_read_only`]: no lock (it works while
//! the world is open elsewhere; readers are never blocked) and nothing
//! written (a torn or damaged tail is left in the file and not shown).

use std::fmt::Write as _;

use kala_core::log::EntryBody;
use kala_core::schema::SchemaEdit;
use kala_core::{AttrId, EntityId, Op, Tick, Value, World};
use serde::Serialize;
use serde_json::json;

use super::read::open_world;
use super::schema;
use crate::cli::LogArgs;
use crate::error::CliError;
use crate::escape;
use crate::out::{JEntity, JValue, Text, entity_text, precision_str, render, value_text};

#[derive(Serialize)]
struct Log {
    command: &'static str,
    transactions: Vec<JTxn>,
}

#[derive(Serialize)]
struct JTxn {
    tx: u64,
    entries: Vec<JEntry>,
}

#[derive(Serialize)]
struct JEntry {
    entry: u64,
    #[serde(flatten)]
    body: JBody,
}

/// A fact as `fact`, `deleted` and `revised` entries show it.
#[derive(Serialize)]
struct JFact {
    entity: JEntity,
    attr: String,
    value: JValue,
    tick: i64,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JBody {
    Schema {
        change: serde_json::Value,
        #[serde(skip)]
        text: String,
    },
    DefineNodeType {
        id: u16,
        name: String,
    },
    DefineAttr {
        id: u32,
        name: String,
    },
    DefineEntity {
        /// `CHARACTER#1`.
        id: String,
        handle: Option<String>,
    },
    DefineEdge {
        /// `edge#9`.
        edge: String,
        source: JEntity,
        attr: String,
        target: JEntity,
    },
    Fact {
        #[serde(flatten)]
        fact: JFact,
        precision: &'static str,
        op: &'static str,
    },
    Deleted {
        of: u64,
        rev: u32,
        #[serde(flatten)]
        fact: JFact,
    },
    Revised {
        of: u64,
        rev: u32,
        #[serde(flatten)]
        fact: JFact,
        precision: &'static str,
        op: &'static str,
    },
    Commit {
        tx: u64,
        checksum: u32,
    },
}

impl Text for Log {
    fn text(&self) -> String {
        let mut s = String::new();
        for t in &self.transactions {
            let _ = writeln!(s, "tx#{}", t.tx);
            for e in &t.entries {
                let _ = writeln!(s, "  entry#{}  {}", e.entry, body_text(&e.body));
            }
        }
        if self.transactions.is_empty() {
            s.push_str("(no transactions)\n");
        }
        s
    }
}

/// `<label> <attr> <value> @<tick>`.
fn fact_text(f: &JFact) -> String {
    format!(
        "{} {} {} @{}",
        entity_text(&f.entity),
        escape::value(&f.attr),
        value_text(&f.value),
        f.tick
    )
}

/// ` (year)` for a tick that isn't exact; nothing for an exact one.
fn precision_text(p: &str) -> String {
    if p == "exact" {
        String::new()
    } else {
        format!(" ({p})")
    }
}

fn body_text(b: &JBody) -> String {
    match b {
        JBody::Schema { text, .. } => format!("schema {text}"),
        JBody::DefineNodeType { id, name } => {
            format!("define node type {} (id {id})", escape::value(name))
        }
        JBody::DefineAttr { id, name } => {
            format!("define attribute {} (id {id})", escape::value(name))
        }
        JBody::DefineEntity { id, handle } => match handle {
            Some(h) => format!(
                "define entity {} handle {}",
                escape::value(id),
                escape::label(h)
            ),
            None => format!("define entity {}", escape::value(id)),
        },
        JBody::DefineEdge {
            edge,
            source,
            attr,
            target,
        } => format!(
            "define edge {edge} = ({} {} {})",
            entity_text(source),
            escape::value(attr),
            entity_text(target)
        ),
        JBody::Fact {
            fact,
            precision,
            op,
        } => format!("{op} {}{}", fact_text(fact), precision_text(precision)),
        JBody::Deleted { of, rev, fact } => {
            format!("DELETED {}  (rev {rev} of entry#{of})", fact_text(fact))
        }
        JBody::Revised {
            of,
            rev,
            fact,
            precision,
            op,
        } => format!(
            "revised: {op} {}{}  (rev {rev} of entry#{of})",
            fact_text(fact),
            precision_text(precision)
        ),
        JBody::Commit { checksum, .. } => format!("commit (checksum {checksum:#010x})"),
    }
}

pub fn run(args: &LogArgs) -> Result<String, CliError> {
    let w = open_world(&args.world)?;
    let limit = args
        .limit
        .map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
    let from = args.from.map_or(0, |c| c.0);
    let mut transactions: Vec<JTxn> = Vec::new();
    // Every returned transaction is a run of entries ending in its
    // `Commit`, which carries the tx.
    let mut pending = Vec::new();
    for e in w.entries() {
        pending.push(JEntry {
            entry: e.id.0,
            body: body(&w, &e.body),
        });
        let EntryBody::Commit { tx, .. } = e.body else {
            continue;
        };
        let entries = std::mem::take(&mut pending);
        if tx.0 < from {
            continue;
        }
        if transactions.len() == limit {
            break;
        }
        transactions.push(JTxn { tx: tx.0, entries });
    }
    let out = Log {
        command: "log",
        transactions,
    };
    Ok(render(&out, args.json))
}

/// An entity as the log shows it: handle, else ID; no name (the log isn't
/// read at a tick).
fn ent(n: &World, id: EntityId) -> JEntity {
    JEntity::new(n, id, None)
}

/// The attribute's name, else `attr#<id>` (a world defines every attribute
/// its log uses, so the fallback is never shown).
fn attr_name(n: &World, id: AttrId) -> String {
    n.attr_name(id)
        .map_or_else(|| format!("attr#{}", id.0), str::to_string)
}

fn fact(n: &World, entity: EntityId, attr: AttrId, value: &Value, tick: Tick) -> JFact {
    JFact {
        entity: ent(n, entity),
        attr: attr_name(n, attr),
        value: if kala_system::is_system(n) && n.attr_name(attr) == Some("password_hash") {
            JValue::redacted()
        } else {
            JValue::new(value, |e| ent(n, e))
        },
        tick: tick.0,
    }
}

/// A schema entry, structured like the write report's `schema` records
/// (`kind`, `name`, `type`, `inferred`, …; see `schema::change_json`).
///
/// Names: what the entry itself carries (a definition logs its full
/// definition, name included, so a rename shows the new name and the old
/// attribute's later entries show `age@old`). The one ID-only reference, a
/// migration's source attribute, resolves through the world's (latest) name
/// table like every fact's attribute in the log, and so does its type.
fn schema_body(n: &World, edit: &SchemaEdit) -> JBody {
    let (change, text) = match edit {
        SchemaEdit::Attribute { id, def } => {
            let mut change = schema::def_json(def);
            change["kind"] = "attribute".into();
            change["id"] = id.0.into();
            let origin = if def.inferred { "inferred" } else { "declared" };
            (
                change,
                format!("attribute {} ({origin})", schema::def_text(def)),
            )
        }
        SchemaEdit::NodeType { id, name, inferred } => {
            let origin = if *inferred { "inferred" } else { "declared" };
            (
                json!({"kind":"node_type","id":id.0,"name":name,"inferred":inferred}),
                format!("node type {} ({origin})", escape::value(name)),
            )
        }
        SchemaEdit::Setting { key, value } => (
            json!({"kind":"world_setting","key":key,"value":value,"inferred":false}),
            format!(
                "world setting :{} :{}",
                escape::value(key),
                escape::value(value)
            ),
        ),
        SchemaEdit::Migration {
            source,
            def,
            skip_unconvertible,
        } => {
            let source_name = attr_name(n, *source);
            let from = n.attribute(&source_name).map(|d| d.value_type.name());
            let mut change = schema::def_json(def);
            change["kind"] = "migrate".into();
            change["source"] = source_name.clone().into();
            change["from"] = from.into();
            change["to"] = def.value_type.name().into();
            change["skip_unconvertible"] = (*skip_unconvertible).into();
            let skip = if *skip_unconvertible {
                ", skipping unconvertible values"
            } else {
                ""
            };
            (
                change,
                format!(
                    "migrate :{} from :{}{} to {}{skip}",
                    escape::value(&def.name),
                    escape::value(&source_name),
                    from.map_or(String::new(), |t| format!(" ({t})")),
                    schema::def_text(def)
                ),
            )
        }
    };
    JBody::Schema { change, text }
}

fn op_str(op: Op) -> &'static str {
    match op {
        Op::Assert => "assert",
        Op::Retract => "retract",
    }
}

fn body(n: &World, b: &EntryBody) -> JBody {
    match b {
        EntryBody::Schema(edit) => schema_body(n, edit),
        EntryBody::DefineNodeType { id, name } => JBody::DefineNodeType {
            id: id.0,
            name: name.clone(),
        },
        EntryBody::DefineAttr { id, def } => JBody::DefineAttr {
            id: id.0,
            name: def.name.clone(),
        },
        EntryBody::DefineEntity { id, handle } => JBody::DefineEntity {
            id: n.format_id(*id),
            handle: handle.clone(),
        },
        EntryBody::DefineEdge {
            id,
            source,
            attr,
            target,
        } => JBody::DefineEdge {
            edge: n.format_id(EntityId::from(*id)),
            source: ent(n, *source),
            attr: attr_name(n, *attr),
            target: ent(n, *target),
        },
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        } => JBody::Fact {
            fact: fact(n, *entity, *attr, value, *tick),
            precision: precision_str(*precision),
            op: op_str(*op),
        },
        EntryBody::Deleted {
            of,
            rev,
            entity,
            attr,
            value,
            tick,
        } => JBody::Deleted {
            of: of.0,
            rev: *rev,
            fact: fact(n, *entity, *attr, value, *tick),
        },
        EntryBody::Revised {
            of,
            rev,
            entity,
            attr,
            value,
            tick,
            op,
            precision,
        } => JBody::Revised {
            of: of.0,
            rev: *rev,
            fact: fact(n, *entity, *attr, value, *tick),
            precision: precision_str(*precision),
            op: op_str(*op),
        },
        EntryBody::Commit { tx, checksum } => JBody::Commit {
            tx: tx.0,
            checksum: *checksum,
        },
    }
}
