//! Shared parameter binding for prepared queries and writes.
use kala_core::{Value, ValueType, World};
use kala_query::{Datum, ParamKind, ParameterRow, QueryParams};

pub use kala_query::MAX_STAGED_BYTES;

/// Conservative cost of a row map entry, including the temporary type index.
pub const MAP_FIELD_BYTES: usize = 192;

/// An ordinary expression value or a table restricted to UNWIND.
pub enum BoundParameter {
    Value(Datum),
    Rows(Vec<ParameterRow>),
}

/// Parameter shapes accepted by a front-end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamPolicy {
    pub nulls: bool,
    pub lists: bool,
    pub map_rows: bool,
    /// How the language writes a parameter (`?name`, `$name`), for messages.
    pub sigil: char,
}

impl ParamPolicy {
    pub const SCALARS: Self = Self {
        nulls: false,
        lists: false,
        map_rows: false,
        sigil: '?',
    };
    pub const COLLECTIONS: Self = Self {
        nulls: true,
        lists: true,
        map_rows: true,
        sigil: '$',
    };

    /// Convert the parameters a statement uses. A transport supplies the
    /// name lookup and decodes its representation through `convert`.
    pub fn bind(
        self,
        kinds: &[(String, ParamKind)],
        mut lookup: impl FnMut(
            &str,
            &ParamKind,
            &mut usize,
        ) -> Result<Option<BoundParameter>, BindError>,
    ) -> Result<QueryParams, BindError> {
        let mut out = QueryParams::new();
        let mut remaining = MAX_STAGED_BYTES;
        for (name, kind) in kinds {
            let named = || format!("{}{name}", self.sigil);
            let value = lookup(name, kind, &mut remaining)
                .map_err(|e| e.named(named()))?
                .ok_or_else(|| BindError::Missing(named()))?;
            match value {
                BoundParameter::Value(v) => {
                    out.insert(name.clone(), v);
                }
                BoundParameter::Rows(rows) => out
                    .insert_rows(name.clone(), rows)
                    .map_err(|e| BindError::Row(e.to_string()))?,
            }
        }
        Ok(out)
    }

    /// Validate a borrowed input before converting it to an executor value.
    pub fn convert<T: ParameterTree>(
        self,
        world: &World,
        kind: &ParamKind,
        value: Input<'_, T>,
        remaining: &mut usize,
    ) -> Result<BoundParameter, BindError> {
        if let Input::List(xs) = &value
            && xs
                .first()
                .is_some_and(|x| matches!(x.input(), Input::Map(_)))
        {
            let ParamKind::Unwind(kinds) = kind else {
                return Err(BindError::Invalid(MAP_RULE));
            };
            if !self.map_rows {
                return Err(BindError::Invalid(MAP_RULE));
            }
            if xs.len() > kala_query::MAX_MAP_ROWS {
                return Err(BindError::Invalid("map rows exceed the 10000 row limit"));
            }
            charge_map(
                remaining,
                xs.len().saturating_mul(std::mem::size_of::<ParameterRow>()),
            )?;
            let mut rows = Vec::with_capacity(xs.len());
            for x in *xs {
                let Input::Map(fields) = x.input() else {
                    return Err(BindError::Invalid(
                        "an UNWIND table requires a map at every row",
                    ));
                };
                let mut row = ParameterRow::new();
                for (key, field) in fields {
                    let input = field.input();
                    let size = field_bytes(&input)?;
                    // B-tree cell and validation index, plus owned key and value buffers.
                    charge_map(
                        remaining,
                        MAP_FIELD_BYTES
                            .saturating_add(key.len())
                            .saturating_add(size),
                    )?;
                    // A field takes the kind its position gives it, as a
                    // scalar parameter there would; any other is a scalar.
                    let kind = kinds
                        .iter()
                        .find(|(field, _)| field == key)
                        .map_or(&ParamKind::Scalar, |(_, kind)| kind);
                    // `field_bytes` has charged this field's list already.
                    let mut charged = usize::MAX;
                    row.insert(
                        key.clone(),
                        self.datum(world, kind, input, 0, &mut charged)?,
                    );
                }
                rows.push(row);
            }
            return Ok(BoundParameter::Rows(rows));
        }
        let kind = if matches!(kind, ParamKind::Unwind(_)) {
            &ParamKind::Scalar
        } else {
            kind
        };
        self.datum(world, kind, value, 0, remaining)
            .map(BoundParameter::Value)
    }

    fn datum<T: ParameterTree>(
        self,
        world: &World,
        kind: &ParamKind,
        value: Input<'_, T>,
        depth: usize,
        remaining: &mut usize,
    ) -> Result<Datum, BindError> {
        if depth > 32 {
            return Err(wrong(kind));
        }
        let collections = !matches!(kind, ParamKind::Tick | ParamKind::Canon);
        let v = match value {
            Input::Null if collections && self.nulls => return Ok(Datum::Null),
            Input::List(xs) if collections && self.lists => {
                // Charged before the list is built: each element becomes a
                // whole `Datum`, however small it was on the wire.
                *remaining = remaining
                    .checked_sub(xs.len().saturating_mul(std::mem::size_of::<Datum>()))
                    .ok_or(BindError::Invalid(
                        "list parameters exceed the 16 MiB parameter budget",
                    ))?;
                return xs
                    .iter()
                    .map(|x| self.datum(world, kind, x.input(), depth + 1, remaining))
                    .collect::<Result<_, _>>()
                    .map(Datum::List);
            }
            Input::Bool(b)
                if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::Bool)) =>
            {
                Value::Bool(b)
            }
            Input::Integer(n)
                if matches!(
                    kind,
                    ParamKind::Scalar | ParamKind::Tick | ParamKind::Value(ValueType::I64)
                ) || (*kind == ParamKind::Canon && n >= 0) =>
            {
                Value::I64(n)
            }
            Input::Float(f)
                if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::F64)) =>
            {
                Value::f64(f).map_err(|_| wrong(kind))?
            }
            Input::Integer(n)
                if *kind == ParamKind::Value(ValueType::F64) && n.unsigned_abs() <= 1 << 53 =>
            {
                #[allow(clippy::cast_precision_loss)] // exact within 2^53
                Value::f64(n as f64).map_err(|_| wrong(kind))?
            }
            Input::String(s)
                if matches!(kind, ParamKind::Scalar | ParamKind::Value(ValueType::Str)) =>
            {
                Value::Str(s.into())
            }
            Input::String(s)
                if matches!(kind, ParamKind::Entity | ParamKind::Value(ValueType::Ref)) =>
            {
                Value::Ref(
                    world
                        .entity_by_handle(s)
                        .or_else(|| world.parse_id(s))
                        .filter(|id| world.is_defined(*id))
                        .ok_or_else(|| wrong(kind))?,
                )
            }
            Input::String(s) if *kind == ParamKind::Canon => Value::I64(
                s.strip_prefix("tx#")
                    .unwrap_or(s)
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| wrong(kind))?,
            ),
            Input::Map(_) => return Err(BindError::Invalid(MAP_RULE)),
            _ => return Err(wrong(kind)),
        };
        Ok(Datum::Value(v))
    }
}

/// The value is not what the parameter's position takes. [`ParamPolicy::bind`]
/// adds the parameter's name.
fn wrong(kind: &ParamKind) -> BindError {
    BindError::Type {
        name: String::new(),
        kind: kind.clone(),
    }
}

fn charge_map(remaining: &mut usize, size: usize) -> Result<(), BindError> {
    *remaining = remaining.checked_sub(size).ok_or(BindError::Invalid(
        "map rows exceed the 16 MiB parameter budget",
    ))?;
    Ok(())
}

fn field_bytes<T: ParameterTree>(input: &Input<'_, T>) -> Result<usize, BindError> {
    Ok(match input {
        Input::String(s) => s.len(),
        Input::List(xs) => {
            let mut n = xs.len().saturating_mul(std::mem::size_of::<Datum>());
            for x in *xs {
                match x.input() {
                    Input::String(s) => n = n.saturating_add(s.len()),
                    Input::Integer(_) | Input::Float(_) | Input::Bool(_) => {}
                    _ => {
                        return Err(BindError::Invalid(
                            "map row fields accept lists of scalars only",
                        ));
                    }
                }
            }
            n
        }
        Input::Map(_) => {
            return Err(BindError::Invalid(
                "nested maps are not allowed in map rows",
            ));
        }
        _ => 0,
    })
}

/// Borrowed view of a decoded client value. A list is walked without a copy.
pub enum Input<'a, T> {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(&'a str),
    List(&'a [T]),
    Map(Box<dyn Iterator<Item = (&'a String, &'a T)> + 'a>),
    Unsupported,
}

/// Transport-specific decoding ends at this interface.
pub trait ParameterTree: Sized {
    fn input(&self) -> Input<'_, Self>;
}

const MAP_RULE: &str = "a map is only allowed in a list of rows used directly by UNWIND";

/// Why parameters could not be bound. A message names the parameter and
/// what its position takes, never the supplied value: an adapter that may
/// show the value to its user (the CLI, whose author typed it) adds it.
#[derive(Debug)]
pub enum BindError {
    /// The statement uses this parameter and it was not supplied.
    Missing(String),
    /// A supplied name the statement does not use.
    Unknown(String),
    /// A name supplied twice (`who` and `?who` are one name).
    Duplicate(String),
    Row(String),
    /// `name` was given something that is not a `kind`.
    Type {
        name: String,
        kind: ParamKind,
    },
    Invalid(&'static str),
}
impl BindError {
    fn named(self, name: String) -> Self {
        match self {
            Self::Type { kind, .. } => Self::Type { name, kind },
            other => other,
        }
    }
}

/// What a parameter of this kind takes, as a message names it.
pub fn expected(kind: &ParamKind) -> &'static str {
    match kind {
        ParamKind::Scalar => "a scalar value (a string, an integer, a float or a bool)",
        ParamKind::Unwind(_) => "a list, or a scalar value",
        ParamKind::Entity | ParamKind::Value(ValueType::Ref) => {
            "an entity (a handle or an ID of this world)"
        }
        ParamKind::Value(ValueType::Str) => "a string",
        ParamKind::Value(ValueType::I64) => "an i64",
        ParamKind::Value(ValueType::F64) => "an f64",
        ParamKind::Value(ValueType::Bool) => "a bool (`true` or `false`)",
        ParamKind::Tick => "a tick (a signed 64-bit integer)",
        ParamKind::Canon => "a canon (`tx#N` or `N`)",
    }
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "missing parameter `{name}`"),
            Self::Unknown(name) => {
                write!(
                    f,
                    "unknown parameter `{name}`: the statement doesn't use it"
                )
            }
            Self::Duplicate(name) => write!(f, "parameter `{name}` is given twice"),
            Self::Type { name, kind } => write!(
                f,
                "wrong type for query parameter `{name}`: expected {}",
                expected(kind)
            ),
            Self::Row(message) => f.write_str(message),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for BindError {}

impl ParameterTree for serde_json::Value {
    fn input(&self) -> Input<'_, Self> {
        match self {
            Self::Null => Input::Null,
            Self::Bool(b) => Input::Bool(*b),
            Self::Number(n) => n
                .as_i64()
                .map(Input::Integer)
                .or_else(|| n.as_f64().map(Input::Float))
                .unwrap_or(Input::Unsupported),
            Self::String(s) => Input::String(s),
            Self::Array(xs) => Input::List(xs),
            Self::Object(m) => Input::Map(Box::new(m.iter())),
        }
    }
}

impl ParamPolicy {
    /// CLI and REPL text syntax. Collections and quoted strings use JSON.
    pub fn bind_text(
        self,
        world: &World,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, BindError> {
        self.bind_text_scope(world, kinds, given, true)
    }

    /// A script or REPL shares its supplied names across several statements.
    pub fn bind_write_text(
        self,
        world: &World,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
    ) -> Result<QueryParams, BindError> {
        self.bind_text_scope(world, kinds, given, false)
    }

    fn bind_text_scope(
        self,
        world: &World,
        kinds: &[(String, ParamKind)],
        given: &[(String, String)],
        reject_unused: bool,
    ) -> Result<QueryParams, BindError> {
        let size = given.iter().fold(0usize, |n, (k, v)| {
            n.saturating_add(k.len()).saturating_add(v.len())
        });
        if !reject_unused && size > MAX_STAGED_BYTES {
            return Err(BindError::Invalid(
                "parameters exceed the 16 MiB transaction limit",
            ));
        }
        if self.lists && size > 1 << 20 {
            return Err(BindError::Invalid("query parameters exceed 1 MiB"));
        }
        let mut texts = std::collections::BTreeMap::new();
        for (name, text) in given {
            let name = name
                .strip_prefix('?')
                .or_else(|| self.lists.then(|| name.strip_prefix('$')).flatten())
                .unwrap_or(name);
            if reject_unused && !kinds.iter().any(|(n, _)| n == name) {
                return Err(BindError::Unknown(format!("{}{name}", self.sigil)));
            }
            if texts.insert(name, text).is_some() {
                return Err(BindError::Duplicate(format!("{}{name}", self.sigil)));
            }
        }
        self.bind(kinds, |name, kind, remaining| {
            texts
                .get(name)
                .map(|s| self.convert_text(world, kind, s, remaining))
                .transpose()
        })
    }

    fn convert_text(
        self,
        world: &World,
        kind: &ParamKind,
        s: &str,
        remaining: &mut usize,
    ) -> Result<BoundParameter, BindError> {
        if self.text_is_json(kind, s) {
            let json: serde_json::Value = serde_json::from_str(s)
                .map_err(|_| BindError::Invalid("invalid JSON query parameter"))?;
            return self.convert(world, kind, json.input(), remaining);
        }
        let raw: Input<'_, serde_json::Value> = match kind {
            ParamKind::Scalar | ParamKind::Unwind(_) => s
                .parse()
                .map(Input::Integer)
                .ok()
                .or_else(|| s.parse().map(Input::Bool).ok())
                .or_else(|| numeric_float(s).map(Input::Float))
                .unwrap_or(Input::String(s)),
            ParamKind::Tick | ParamKind::Value(ValueType::I64) => {
                Input::Integer(s.parse().map_err(|_| wrong(kind))?)
            }
            ParamKind::Value(ValueType::F64) => Input::Float(s.parse().map_err(|_| wrong(kind))?),
            ParamKind::Value(ValueType::Bool) => Input::Bool(s.parse().map_err(|_| wrong(kind))?),
            _ => Input::String(s),
        };
        self.convert(world, kind, raw, remaining)
    }

    /// Whether a parameter's text is read as JSON rather than as one scalar
    /// of its kind. Only a language with NULL and lists has JSON text: for
    /// an ordinary parameter, exactly `null`, a text that starts with `[` (a
    /// list) or one that starts with `"` (a quoted string). Any other text
    /// is the value itself, so `{draft}` and ` [x` stay strings. A direct
    /// UNWIND source may be a list of map rows, so its text is JSON whenever
    /// it starts, after whitespace, like a JSON list, map or string.
    pub fn text_is_json(self, kind: &ParamKind, s: &str) -> bool {
        if !self.lists || matches!(kind, ParamKind::Tick | ParamKind::Canon) {
            return false;
        }
        if matches!(kind, ParamKind::Unwind(_)) {
            return s == "null" || s.trim_start().starts_with(['[', '{', '"']);
        }
        s == "null" || s.starts_with(['[', '"'])
    }
}

/// `s` as a float, only when it looks numeric:
/// `^[+-]?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?$`. Rust's `f64` parser also
/// takes `nan`, `inf` and `infinity`; those words stay strings here, since a
/// scalar parameter is guessed from its text.
fn numeric_float(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = 0;
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_digits = digits(&mut i);
    let mut frac_digits = 0;
    if b.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = digits(&mut i);
    }
    if int_digits == 0 && frac_digits == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    s.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kala_core::Schema;

    #[test]
    fn a_list_parameter_is_charged_per_element_at_every_level() {
        let world = World::in_memory(Schema::new()).unwrap();
        let cell = std::mem::size_of::<Datum>();
        let convert = |json: serde_json::Value, mut remaining: usize| {
            ParamPolicy::COLLECTIONS
                .convert(&world, &ParamKind::Scalar, json.input(), &mut remaining)
                .map(|_| remaining)
        };
        let flat = serde_json::json!([1, 2, 3, 4]);
        assert!(matches!(convert(flat.clone(), 4 * cell), Ok(0)));
        assert!(matches!(
            convert(flat, 4 * cell - 1),
            Err(BindError::Invalid(message)) if message.contains("parameter budget")
        ));
        // Two inner lists of two: 2 + 2 * 2 cells.
        let nested = serde_json::json!([[1, 2], [3, 4]]);
        assert!(matches!(convert(nested.clone(), 6 * cell), Ok(0)));
        assert!(convert(nested, 6 * cell - 1).is_err());
        // A map row's list field is charged once, with its row.
        let rows = serde_json::json!([{"xs": [1, 2, 3]}]);
        let mut remaining = MAX_STAGED_BYTES;
        ParamPolicy::COLLECTIONS
            .convert(
                &world,
                &ParamKind::Unwind(Vec::new()),
                rows.input(),
                &mut remaining,
            )
            .ok()
            .unwrap();
        assert_eq!(
            MAX_STAGED_BYTES - remaining,
            std::mem::size_of::<ParameterRow>() + MAP_FIELD_BYTES + "xs".len() + 3 * cell
        );
    }
}
