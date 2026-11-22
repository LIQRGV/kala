//! `kala list <world> <NODE_TYPE> [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::read::{Ctx, core_err, tick_arg};
use crate::cli::ReadAt;
use crate::error::CliError;
use crate::escape;
use crate::out::{JEntity, Text, entity_text, render};

#[derive(Serialize)]
struct List {
    command: &'static str,
    canon: u64,
    tick: i64,
    node_type: String,
    entities: Vec<JEntity>,
}

impl Text for List {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} at tick {}, canon tx#{}: {} entities",
            escape::value(&self.node_type),
            self.tick,
            self.canon,
            self.entities.len()
        );
        for e in &self.entities {
            let _ = writeln!(s, "  {}", entity_text(e));
        }
        s
    }
}

pub fn run(
    world: &Path,
    node_type: &str,
    tick: Option<&str>,
    read: &ReadAt,
) -> Result<String, CliError> {
    let given = tick_arg(tick, read.at)?;
    let ctx = Ctx::open(world, read.canon)?;
    // Ascending by ID; the canon decides which entities exist, the tick
    // only their names.
    let ids: Vec<_> = ctx
        .view()
        .entities_of_type(node_type)
        .map_err(core_err)?
        .collect();
    let tick = ctx.tick(given);
    let l = ctx.labeler(tick);
    let out = List {
        command: "list",
        canon: ctx.canon.0,
        tick: tick.0,
        node_type: node_type.to_string(),
        entities: ids.iter().map(|i| l.entity(*i)).collect(),
    };
    Ok(render(&out, read.json))
}
