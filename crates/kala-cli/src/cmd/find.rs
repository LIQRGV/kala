//! `kala find <world> <attr> <value> [-i] [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::Value;
use serde::Serialize;

use super::read::{Ctx, core_err, parse_value, tick_arg};
use crate::cli::ReadAt;
use crate::error::CliError;
use crate::escape;
use crate::out::{JEntity, JValue, Text, entity_text, render, value_text};

#[derive(Serialize)]
struct Find {
    command: &'static str,
    canon: u64,
    tick: i64,
    attr: String,
    value: JValue,
    case_insensitive: bool,
    entities: Vec<JEntity>,
}

impl Text for Find {
    fn text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "{} {} {}{} at tick {}, canon tx#{}",
            escape::value(&self.attr),
            if self.case_insensitive { "~" } else { "=" },
            value_text(&self.value),
            if self.case_insensitive {
                " (case-insensitive)"
            } else {
                ""
            },
            self.tick,
            self.canon
        );
        if self.entities.is_empty() {
            s.push_str("  (none)\n");
        }
        for e in &self.entities {
            let _ = writeln!(s, "  {}", entity_text(e));
        }
        s
    }
}

pub fn run(
    world: &Path,
    attr: &str,
    value: &str,
    tick: Option<&str>,
    ignore_case: bool,
    read: &ReadAt,
) -> Result<String, CliError> {
    let given = tick_arg(tick, read.at)?;
    let ctx = Ctx::open(world, read.canon)?;
    let tick = ctx.tick(given);
    let view = ctx.view();
    let (v, mut ids) = if ignore_case {
        // kala-core refuses `-i` on anything but a string attribute.
        let ids = view.lookup_ci(attr, value, tick).map_err(core_err)?;
        (Value::Str(value.to_string()), ids)
    } else {
        let ty = ctx
            .world
            .attribute(attr)
            .map(|a| a.value_type)
            .ok_or_else(|| core_err(kala_core::Error::UnknownAttribute(attr.to_string())))?;
        let v = parse_value(
            &ctx,
            ty,
            value,
            &format!("attribute `{}`", escape::value(attr)),
        )?;
        let ids = view.lookup(attr, &v, tick).map_err(core_err)?;
        (v, ids)
    };
    ids.sort();
    let l = ctx.labeler(tick);
    let out = Find {
        command: "find",
        canon: ctx.canon.0,
        tick: tick.0,
        attr: attr.to_string(),
        value: l.value(&v),
        case_insensitive: ignore_case,
        entities: ids.iter().map(|i| l.entity(*i)).collect(),
    };
    Ok(render(&out, read.json))
}
