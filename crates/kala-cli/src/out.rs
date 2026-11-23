//! The read commands' output model: one set of `Serialize` structs per
//! command, printed either as one JSON document (`--json`) or as text.
//!
//! Field order is declaration order, so it is stable. JSON is written by
//! [`to_json`], which escapes every control and bidi char (not only the
//! ones JSON requires), so `--json` output is as terminal-safe as text.

use std::io;

use kala_core::{EntityId, Interval, IntervalSet, Op, Precision, Tick, TxId, Value, View, World};
use kala_query::Datum;
use serde::Serialize;

use crate::escape;
use crate::names::Names;

/// An entity (or edge) as every read shows it.
#[derive(Debug, Serialize)]
pub struct JEntity {
    /// `CHARACTER#1`, `edge#9`.
    pub id: String,
    pub handle: Option<String>,
    /// The `name` at the command's tick and canon, if exactly one holds.
    pub name: Option<String>,
    /// The handle, else the name, else the ID.
    pub label: String,
}

impl JEntity {
    /// The entity with `name` as its name (`None`: no name shown, as in
    /// `kala log`, which isn't read at a tick).
    pub fn new(names: &dyn Names, id: EntityId, name: Option<String>) -> JEntity {
        let handle = names.handle(id).map(str::to_string);
        let fid = names.format_id(id);
        let label = handle
            .clone()
            .or_else(|| name.clone())
            .unwrap_or_else(|| fid.clone());
        JEntity {
            id: fid,
            handle,
            name,
            label,
        }
    }
}

/// A typed value: `{"type", "value"}`.
#[derive(Debug, Serialize)]
pub struct JValue {
    #[serde(rename = "type")]
    pub ty: &'static str,
    pub value: JInner,
}

impl JValue {
    /// The value, with refs shown by `entity`.
    pub fn new(v: &Value, entity: impl Fn(EntityId) -> JEntity) -> JValue {
        let (ty, value) = match v {
            Value::Bool(b) => ("bool", JInner::Bool(*b)),
            Value::I64(i) => ("i64", JInner::I64(*i)),
            Value::F64(f) if f.is_infinite() => (
                "f64",
                JInner::F64Special(if *f > 0.0 { "inf" } else { "-inf" }),
            ),
            Value::F64(f) => ("f64", JInner::F64(*f)),
            Value::Str(s) => ("string", JInner::Str(s.clone())),
            Value::Ref(e) => ("ref", JInner::Ref(entity(*e))),
        };
        JValue { ty, value }
    }

    /// Secret placeholders retain the string shape without exposing a hash.
    pub fn redacted() -> JValue {
        JValue {
            ty: "string",
            value: JInner::Str("<redacted>".into()),
        }
    }

    /// A query answer's cell: a value as [`JValue::new`] shows it, an op as
    /// `{"type": "op", "value": "assert"}`, an interval set as
    /// `{"type": "intervals", "value": [[from, until or null], ...]}`.
    pub fn datum(d: &Datum, entity: impl Fn(EntityId) -> JEntity) -> JValue {
        Self::datum_with(d, &entity)
    }
    fn datum_with(d: &Datum, entity: &dyn Fn(EntityId) -> JEntity) -> JValue {
        match d {
            Datum::Null => JValue {
                ty: "null",
                value: JInner::Null,
            },
            Datum::List(values) => JValue {
                ty: "list",
                value: JInner::List(values.iter().map(|v| Self::datum_with(v, entity)).collect()),
            },
            Datum::Value(v) => JValue::new(v, entity),
            Datum::Precision(p) => JValue {
                ty: "precision",
                value: JInner::Op(precision_str(*p)),
            },
            Datum::Op(op) => JValue {
                ty: "op",
                value: JInner::Op(match op {
                    Op::Assert => "assert",
                    Op::Retract => "retract",
                }),
            },
            Datum::Intervals(set) => JValue {
                ty: "intervals",
                value: JInner::Intervals(
                    set.intervals()
                        .iter()
                        .map(|i| (i.from.0, i.until.map(|u| u.0)))
                        .collect(),
                ),
            },
        }
    }
}

/// The payload of a [`JValue`].
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum JInner {
    Null,
    List(Vec<JValue>),
    Bool(bool),
    I64(i64),
    F64(f64),
    /// A non-finite f64 (`"inf"`, `"-inf"`): JSON has no such number.
    F64Special(&'static str),
    Str(String),
    Ref(JEntity),
    /// A change point's op (`assert`, `retract`).
    Op(&'static str),
    /// An interval set's half-open intervals, `None` (`null`) for no end.
    Intervals(Vec<(i64, Option<i64>)>),
}

/// A tick with its precision.
#[derive(Debug, Serialize)]
pub struct JTick {
    pub tick: i64,
    pub precision: &'static str,
}

impl JTick {
    pub fn new(t: Tick, p: Precision) -> JTick {
        JTick {
            tick: t.0,
            precision: precision_str(p),
        }
    }
}

/// One value of an attribute, with the tick it holds since.
#[derive(Debug, Serialize)]
pub struct JHeld {
    pub value: JValue,
    pub since: JTick,
}

/// An attribute and the values holding at a tick.
#[derive(Debug, Serialize)]
pub struct JAttr {
    pub attr: String,
    pub values: Vec<JHeld>,
}

pub fn precision_str(p: Precision) -> &'static str {
    match p {
        Precision::Exact => "exact",
        Precision::Day => "day",
        Precision::Month => "month",
        Precision::Year => "year",
        Precision::Decade => "decade",
        Precision::Century => "century",
    }
}

/// Shows entities and values at one `(canon, tick)`: names are read there.
pub struct Labeler<'w> {
    pub world: &'w World,
    pub view: View<'w>,
    pub tick: Tick,
    /// Whether the schema has a `name` attribute of type string.
    has_name: bool,
}

impl<'w> Labeler<'w> {
    pub fn new(world: &'w World, canon: TxId, tick: Tick) -> Labeler<'w> {
        let has_name = world
            .at_canon(canon)
            .attribute("name")
            .map(|a| a.value_type)
            == Some(kala_core::ValueType::Str);
        Labeler {
            world,
            view: world.at_canon(canon),
            tick,
            has_name,
        }
    }

    fn name_of(&self, id: EntityId) -> Option<String> {
        if !self.has_name {
            return None;
        }
        match self.view.as_of(id, "name", self.tick).ok()?.as_slice() {
            [Value::Str(s)] => Some(s.clone()),
            _ => None,
        }
    }

    pub fn entity(&self, id: EntityId) -> JEntity {
        JEntity::new(self.world, id, self.name_of(id))
    }

    pub fn value(&self, v: &Value) -> JValue {
        if kala_bolt::system::is_system(self.world)
            && matches!(v, Value::Str(s) if s.starts_with("$argon2"))
        {
            return JValue::redacted();
        }
        JValue::new(v, |e| self.entity(e))
    }

    /// Attribute-aware redaction also covers damaged hash values.
    pub fn attribute_value(&self, attr: &str, value: &Value) -> JValue {
        if attr == "password_hash" && kala_bolt::system::is_system(self.world) {
            JValue::redacted()
        } else {
            self.value(value)
        }
    }

    /// A query answer's cell ([`JValue::datum`]).
    pub fn datum(&self, d: &Datum) -> JValue {
        match d {
            Datum::Value(value) => self.value(value),
            _ => JValue::datum(d, |e| self.entity(e)),
        }
    }

    /// The values of `entity.attr` holding at the tick.
    pub fn attr(&self, entity: EntityId, attr: &str) -> Result<JAttr, kala_core::Error> {
        let values = self
            .view
            .as_of_detail(entity, attr, self.tick)?
            .iter()
            .map(|c| JHeld {
                value: self.attribute_value(attr, &c.value),
                since: JTick::new(c.tick, c.precision),
            })
            .collect();
        Ok(JAttr {
            attr: attr.to_string(),
            values,
        })
    }

    /// Every schema attribute (schema order) holding at least one value.
    pub fn attributes(&self, entity: EntityId) -> Result<Vec<JAttr>, kala_core::Error> {
        let mut out = Vec::new();
        for a in self.view.attributes() {
            let j = self.attr(entity, &a.name)?;
            if !j.values.is_empty() {
                out.push(j);
            }
        }
        Ok(out)
    }
}

// ---- JSON ---------------------------------------------------------------------

/// serde_json's compact output, plus `\u` escapes for every
/// [`kala_core::must_escape`] char inside strings (serde_json writes C1
/// controls, DEL, bidi controls, U+2028/2029 and U+FEFF raw).
struct SafeFormatter;

impl serde_json::ser::Formatter for SafeFormatter {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if escape::json_needs_escape(c) {
                writer.write_all(&fragment.as_bytes()[start..i])?;
                // Every such char is in the BMP, so one `\u` escape.
                write!(writer, "\\u{:04x}", u32::from(c))?;
                start = i + c.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }
}

/// A command's text form; its JSON form is its `Serialize` impl.
pub trait Text {
    fn text(&self) -> String;
}

/// The command's output: one JSON document (`--json`) or its text.
pub fn render<T: Serialize + Text>(out: &T, json: bool) -> String {
    if json { to_json(out) } else { out.text() }
}

/// Count the actual terminal-safe JSON bytes without allocating output.
/// Stop serialization as soon as the allowance is exceeded.
pub(crate) fn json_size_within<T: Serialize>(value: &T, limit: usize) -> Option<usize> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(io::Error::other("output allowance exceeded"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    value
        .serialize(&mut serde_json::Serializer::with_formatter(
            &mut counter,
            SafeFormatter,
        ))
        .ok()?;
    Some(counter.bytes)
}

/// One JSON document and one newline.
pub(crate) fn to_json<T: Serialize>(v: &T) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, SafeFormatter);
    // Invariant: these structs have string keys and no fallible Serialize
    // impls, so writing them to a Vec can't fail.
    v.serialize(&mut ser)
        .expect("invariant: output structs always serialize");
    // Invariant: serde_json writes UTF-8.
    let mut s = String::from_utf8(buf).expect("invariant: serde_json writes UTF-8");
    s.push('\n');
    s
}

// ---- text -----------------------------------------------------------------------

/// `-208 (year)`; an exact tick is bare (`-208`).
pub fn tick_text(t: &JTick) -> String {
    if t.precision == "exact" {
        t.tick.to_string()
    } else {
        format!("{} ({})", t.tick, t.precision)
    }
}

/// An entity by its label, escaped ([`escape::label`]: `\` doubled).
pub fn entity_text(e: &JEntity) -> String {
    escape::label(&e.label)
}

/// A value for text: strings quoted, refs by label.
pub fn value_text(v: &JValue) -> String {
    match &v.value {
        JInner::Null => "NULL".into(),
        JInner::List(values) => format!(
            "[{}]",
            values.iter().map(value_text).collect::<Vec<_>>().join(", ")
        ),
        JInner::Bool(b) => b.to_string(),
        JInner::I64(i) => i.to_string(),
        JInner::F64(f) => f.to_string(),
        JInner::F64Special(s) => (*s).to_string(),
        JInner::Str(s) => escape::quoted(s),
        JInner::Ref(e) => entity_text(e),
        JInner::Op(op) => format!(":{op}"),
        JInner::Intervals(ivs) => {
            IntervalSet::from_intervals(ivs.iter().map(|&(from, until)| Interval {
                from: Tick(from),
                until: until.map(Tick),
            }))
            .to_string()
        }
    }
}

/// `  attr = value  (since T)` lines, one per value; `  attr: (none)` for an
/// attribute with no value.
pub fn attrs_text(out: &mut String, attrs: &[JAttr]) {
    use std::fmt::Write as _;
    for a in attrs {
        let name = escape::value(&a.attr);
        if a.values.is_empty() {
            let _ = writeln!(out, "  {name}: (none)");
        }
        for h in &a.values {
            let _ = writeln!(
                out,
                "  {name} = {}  since {}",
                value_text(&h.value),
                tick_text(&h.since)
            );
        }
    }
}
