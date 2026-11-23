//! Showing entities by label (handle, else ID) when there may be no open
//! world: `kala load` and the read commands have one, `kala doctor` only
//! has the log's entries (it never opens the world). Also the one overlap
//! line `load` and `doctor` print.

use std::collections::HashMap;

use kala_core::log::{EntryBody, LogEntry};
use kala_core::{EntityId, Interval, Overlap, Side, Value, World};

use crate::escape;

/// Where entity IDs and labels come from.
pub trait Names {
    /// Whether this is the reserved account store.
    fn is_system(&self) -> bool;
    /// The entity's handle, if it has one.
    fn handle(&self, id: EntityId) -> Option<&str>;
    /// `CHARACTER#1`, `edge#9`, as [`World::format_id`] formats it.
    fn format_id(&self, id: EntityId) -> String;
    /// The entity's handle, else its ID, unescaped.
    fn label(&self, id: EntityId) -> String {
        self.handle(id)
            .map_or_else(|| self.format_id(id), str::to_string)
    }
}

impl Names for World {
    fn is_system(&self) -> bool {
        kala_bolt::system::is_system(self)
    }
    fn handle(&self, id: EntityId) -> Option<&str> {
        self.handle_of(id)
    }
    fn format_id(&self, id: EntityId) -> String {
        // The inherent method (not this one).
        World::format_id(self, id)
    }
}

/// Node type names and handles read from a log's `DEFINE` entries, for
/// `doctor`, which reads the log without opening the world.
#[derive(Default)]
pub struct LogNames {
    node_types: HashMap<u16, String>,
    handles: HashMap<EntityId, String>,
}

impl LogNames {
    pub fn from_entries(entries: &[LogEntry]) -> LogNames {
        let mut n = LogNames::default();
        for e in entries {
            match &e.body {
                EntryBody::DefineNodeType { id, name } => {
                    n.node_types.insert(id.0, name.clone());
                }
                EntryBody::DefineEntity {
                    id,
                    handle: Some(h),
                } => {
                    n.handles.insert(*id, h.clone());
                }
                _ => {}
            }
        }
        n
    }
}

impl Names for LogNames {
    fn is_system(&self) -> bool {
        self.node_types
            .values()
            .any(|name| name == kala_bolt::system::SYSTEM_MARKER)
    }
    fn handle(&self, id: EntityId) -> Option<&str> {
        self.handles.get(&id).map(String::as_str)
    }

    /// As [`World::format_id`]: `edge#seq`, `TYPE#seq`, or `#<type id>#seq`
    /// for a node type the log doesn't define.
    fn format_id(&self, id: EntityId) -> String {
        if id.is_edge() {
            // Bits 0..=62: the edge sequence.
            return format!("edge#{}", id.raw() & !(1 << 63));
        }
        let t = id.node_type().0;
        match self.node_types.get(&t) {
            Some(name) => format!("{name}#{}", id.seq()),
            None => format!("#{t}#{}", id.seq()),
        }
    }
}

/// An entity by label, escaped (and cut) for display, `\` doubled (see
/// [`escape::label`]).
pub fn entity(n: &dyn Names, id: EntityId) -> String {
    escape::label(&n.label(id))
}

/// A value for a one-line message, escaped (and cut).
pub fn value(n: &dyn Names, v: &Value) -> String {
    match v {
        Value::Ref(id) => entity(n, *id),
        Value::Str(s) if n.is_system() && s.starts_with("$argon2") => "\"<redacted>\"".into(),
        Value::Str(s) => format!("\"{}\"", escape::value(s)),
        Value::Bool(b) => b.to_string(),
        Value::I64(i) => i.to_string(),
        Value::F64(f) => f.to_string(),
    }
}

fn interval(i: &Interval) -> String {
    match i.until {
        Some(u) => format!("from tick {} until tick {}", i.from.0, u.0),
        None => format!("from tick {} on", i.from.0),
    }
}

/// `greg located_in: varrow and ashford both hold from tick 601 on`, every
/// piece escaped.
pub fn overlap_line(n: &dyn Names, o: &Overlap) -> String {
    let who = entity(n, o.entity);
    let attr = escape::value(&o.attr);
    let (a, b) = if n.is_system() && o.attr == "password_hash" {
        ("<redacted>".into(), "<redacted>".into())
    } else {
        (value(n, &o.a), value(n, &o.b))
    };
    let during = interval(&o.during);
    match o.side {
        Side::Forward => format!("{who} {attr}: {a} and {b} both hold {during}"),
        Side::Reverse => {
            format!("{who} {attr} (reverse): {a} and {b} both point at it {during}")
        }
    }
}
