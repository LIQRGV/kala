//! `kala history <world> <entity> <attr> [--canon C]`.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::Op;
use serde::Serialize;

use super::read::{Ctx, entity_err};
use crate::cli::Canon;
use crate::error::CliError;
use crate::escape;
use crate::out::{JEntity, JTick, JValue, Text, entity_text, render, tick_text, value_text};

#[derive(Serialize)]
struct ChangeOut {
    #[serde(flatten)]
    at: JTick,
    op: &'static str,
    value: JValue,
}

#[derive(Serialize)]
struct History {
    command: &'static str,
    canon: u64,
    entity: JEntity,
    attr: String,
    changes: Vec<ChangeOut>,
}

impl Text for History {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} {}: history at canon tx#{}",
            entity_text(&self.entity),
            escape::value(&self.attr),
            self.canon
        );
        if self.changes.is_empty() {
            s.push_str("  (no changes)\n");
        }
        for c in &self.changes {
            let _ = writeln!(
                s,
                "  {}  {:<7}  {}",
                tick_text(&c.at),
                c.op,
                value_text(&c.value)
            );
        }
        s
    }
}

pub fn run(
    world: &Path,
    entity: &str,
    attr: &str,
    canon: Option<Canon>,
    json: bool,
) -> Result<String, CliError> {
    let ctx = Ctx::open(world, canon)?;
    let id = ctx.entity(entity)?;
    let changes = ctx.view().history(id, attr).map_err(entity_err(entity))?;
    // A history spans every tick, so names are shown as they are at the
    // canon's latest tick.
    let l = ctx.labeler(ctx.tick(None));
    let out = History {
        command: "history",
        canon: ctx.canon.0,
        entity: l.entity(id),
        attr: attr.to_string(),
        changes: changes
            .iter()
            .map(|c| ChangeOut {
                at: JTick::new(c.tick, c.precision),
                op: match c.op {
                    Op::Assert => "assert",
                    Op::Retract => "retract",
                },
                value: l.value(&c.value),
            })
            .collect(),
    };
    Ok(render(&out, json))
}
