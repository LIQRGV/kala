//! `kala asof <world> <entity> [<attr>] [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::read::{Ctx, entity_err, tick_arg};
use crate::cli::ReadAt;
use crate::error::CliError;
use crate::out::{JAttr, JEntity, Text, attrs_text, entity_text, render};

#[derive(Serialize)]
struct Asof {
    command: &'static str,
    canon: u64,
    tick: i64,
    entity: JEntity,
    attributes: Vec<JAttr>,
}

impl Text for Asof {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} at tick {}, canon tx#{}",
            entity_text(&self.entity),
            self.tick,
            self.canon
        );
        if self.attributes.is_empty() {
            s.push_str("  (no attribute holds a value)\n");
        }
        attrs_text(&mut s, &self.attributes);
        s
    }
}

pub fn run(
    world: &Path,
    entity: &str,
    attr: Option<&str>,
    tick: Option<&str>,
    read: &ReadAt,
) -> Result<String, CliError> {
    // `asof edwin @-210`: clap puts the `@T` in the attribute slot. An
    // attribute whose name starts with `@` is still readable with a tick
    // after it (`asof edwin @odd @5`).
    let (attr, tick) = match (attr, tick) {
        (Some(a), None) if a.starts_with('@') => (None, Some(a)),
        other => other,
    };
    let given = tick_arg(tick, read.at)?;
    let ctx = Ctx::open(world, read.canon)?;
    let id = ctx.entity(entity)?;
    let tick = ctx.tick(given);
    let l = ctx.labeler(tick);
    let attributes = match attr {
        Some(a) => vec![l.attr(id, a).map_err(entity_err(entity))?],
        None => l.attributes(id).map_err(entity_err(entity))?,
    };
    let out = Asof {
        command: "asof",
        canon: ctx.canon.0,
        tick: tick.0,
        entity: l.entity(id),
        attributes,
    };
    Ok(render(&out, read.json))
}
