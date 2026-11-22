//! `kala edge <world> <edge#N | SOURCE ATTR TARGET> [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::{EdgeId, EntityId, Value};
use serde::Serialize;

use super::read::{Ctx, core_err, tick_arg};
use crate::cli::ReadAt;
use crate::error::CliError;
use crate::escape;
use crate::out::{JAttr, JEntity, Text, attrs_text, entity_text, render};

#[derive(Serialize)]
struct Edge {
    command: &'static str,
    canon: u64,
    tick: i64,
    edge: String,
    source: JEntity,
    attr: String,
    target: JEntity,
    holds: bool,
    properties: Vec<JAttr>,
}

impl Text for Edge {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{}: {} {} {} at tick {}, canon tx#{}: {}",
            self.edge,
            entity_text(&self.source),
            escape::value(&self.attr),
            entity_text(&self.target),
            self.tick,
            self.canon,
            if self.holds { "holds" } else { "does not hold" }
        );
        if self.properties.is_empty() {
            s.push_str("  (no properties)\n");
        }
        attrs_text(&mut s, &self.properties);
        s
    }
}

/// What the edge words name.
enum Spec<'a> {
    /// `edge#N`.
    Id(&'a str),
    /// `SOURCE ATTR TARGET`.
    Triple(&'a str, &'a str, &'a str),
}

pub fn run(world: &Path, args: &[String], read: &ReadAt) -> Result<String, CliError> {
    // clap allows 1..=4 words: an `@T` may end them, the rest is an edge ID
    // or a triple.
    let (spec, tick) = match args.split_last() {
        Some((last, rest)) if !rest.is_empty() && last.starts_with('@') => {
            (rest, Some(last.as_str()))
        }
        _ => (args, None),
    };
    let spec = match spec {
        [id] => Spec::Id(id),
        [s, a, t] => Spec::Triple(s, a, t),
        _ => {
            return Err(CliError::Usage(
                "expected `edge#N` or `SOURCE ATTR TARGET`, optionally followed by `@T`"
                    .to_string(),
            ));
        }
    };
    let given = tick_arg(tick, read.at)?;
    let ctx = Ctx::open(world, read.canon)?;
    let (edge, source, attr, target) = match spec {
        Spec::Id(id) => {
            let edge = edge_arg(&ctx, id)?;
            // An edge defined after the canon doesn't exist there.
            let (s, a, t) = ctx.view().edge(edge).ok_or_else(|| {
                CliError::User(format!(
                    "no edge `{}` in this world at canon tx#{}",
                    escape::value(id),
                    ctx.canon.0
                ))
            })?;
            (edge, s, a, t)
        }
        Spec::Triple(s, a, t) => {
            let (src, tgt) = (ctx.entity(s)?, ctx.entity(t)?);
            if ctx.world.attribute(a).is_none() {
                return Err(core_err(kala_core::Error::UnknownAttribute(a.to_string())));
            }
            let edge = ctx.view().edge_id(src, a, tgt).ok_or_else(|| {
                CliError::User(format!(
                    "no edge `{} {} {}`: nothing was asserted between them up to canon tx#{}",
                    escape::value(s),
                    escape::value(a),
                    escape::value(t),
                    ctx.canon.0
                ))
            })?;
            (edge, src, a.to_string(), tgt)
        }
    };
    let tick = ctx.tick(given);
    let holds = ctx
        .view()
        .as_of_value(source, &attr, &Value::Ref(target), tick)
        .map_err(core_err)?;
    let l = ctx.labeler(tick);
    let out = Edge {
        command: "edge",
        canon: ctx.canon.0,
        tick: tick.0,
        edge: ctx.world.format_id(EntityId::from(edge)),
        source: l.entity(source),
        attr,
        target: l.entity(target),
        holds,
        properties: l.attributes(EntityId::from(edge)).map_err(core_err)?,
    };
    Ok(render(&out, read.json))
}

/// An `edge#N` argument.
fn edge_arg(ctx: &Ctx, arg: &str) -> Result<EdgeId, CliError> {
    ctx.world
        .parse_id(arg)
        .and_then(EntityId::as_edge)
        .ok_or_else(|| {
            CliError::User(format!(
                "`{}` is not an edge ID: give `edge#N` or `SOURCE ATTR TARGET`",
                escape::value(arg)
            ))
        })
}
