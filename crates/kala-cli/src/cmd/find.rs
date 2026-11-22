//! `kala find <world> <attr> <value> [-i] [@T]`.

use std::fmt::Write as _;
use std::path::Path;

use kala_core::{Value, ValueType};
use serde::Serialize;

use super::read::{Ctx, core_err, tick_arg};
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
        let v = parse_value(&ctx, ty, attr, value)?;
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

/// The value argument read as the attribute's type; a ref by entity syntax.
fn parse_value(ctx: &Ctx, ty: ValueType, attr: &str, s: &str) -> Result<Value, CliError> {
    let bad = |what: &str| {
        CliError::User(format!(
            "`{}` is not {what} (the type of attribute `{}`)",
            escape::value(s),
            escape::value(attr)
        ))
    };
    Ok(match ty {
        ValueType::Bool => match s {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(bad("a bool (`true` or `false`)")),
        },
        ValueType::I64 => Value::I64(s.parse().map_err(|_| bad("an i64"))?),
        ValueType::F64 => {
            let x: f64 = s.parse().map_err(|_| bad("an f64"))?;
            Value::f64(x).map_err(|_| bad("an f64 (NaN is never stored)"))?
        }
        ValueType::Str => Value::Str(s.to_string()),
        ValueType::Ref => Value::Ref(ctx.entity(s)?),
    })
}
