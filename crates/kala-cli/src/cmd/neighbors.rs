//! `kala neighbors <world> <entity> <attr> [--in|--out|--both] [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::{Direction, EntityId};
use serde::Serialize;

use super::read::{Ctx, entity_err, tick_arg};
use crate::cli::ReadAt;
use crate::error::CliError;
use crate::escape;
use crate::out::{JEntity, Text, entity_text, render};

#[derive(Serialize)]
struct Neighbor {
    entity: JEntity,
    edge: String,
}

#[derive(Serialize)]
struct Neighbors {
    command: &'static str,
    canon: u64,
    tick: i64,
    entity: JEntity,
    attr: String,
    direction: &'static str,
    neighbors: Vec<Neighbor>,
}

impl Text for Neighbors {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} {} ({}) at tick {}, canon tx#{}",
            entity_text(&self.entity),
            escape::value(&self.attr),
            self.direction,
            self.tick,
            self.canon
        );
        if self.neighbors.is_empty() {
            s.push_str("  (none)\n");
        }
        for n in &self.neighbors {
            let _ = writeln!(s, "  {}  ({})", entity_text(&n.entity), n.edge);
        }
        s
    }
}

/// The direction flags, already checked by clap to be at most one.
pub struct Flags {
    pub incoming: bool,
    pub both: bool,
}

pub fn run(
    world: &Path,
    entity: &str,
    attr: &str,
    tick: Option<&str>,
    flags: Flags,
    read: &ReadAt,
) -> Result<String, CliError> {
    let given = tick_arg(tick, read.at)?;
    let (dir, dir_name) = match (flags.incoming, flags.both) {
        (true, _) => (Direction::In, "in"),
        (_, true) => (Direction::Both, "both"),
        _ => (Direction::Out, "out"),
    };
    let ctx = Ctx::open(world, read.canon)?;
    let id = ctx.entity(entity)?;
    let tick = ctx.tick(given);
    let mut found = ctx
        .view()
        .neighbors(id, attr, dir, tick)
        .map_err(entity_err(entity))?;
    found.sort_by_key(|(n, _)| *n);
    let l = ctx.labeler(tick);
    let out = Neighbors {
        command: "neighbors",
        canon: ctx.canon.0,
        tick: tick.0,
        entity: l.entity(id),
        attr: attr.to_string(),
        direction: dir_name,
        neighbors: found
            .iter()
            .map(|(n, e)| Neighbor {
                entity: l.entity(*n),
                edge: ctx.world.format_id(EntityId::from(*e)),
            })
            .collect(),
    };
    Ok(render(&out, read.json))
}
