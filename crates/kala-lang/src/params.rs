//! Shared parameter binding for prepared queries and writes.
use kala_core::{Value, ValueType, World};
use kala_query::{Datum, ParamKind, ParameterRow, QueryParams};

/// Maximum retained input for a write transaction.
pub const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;

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
}

impl ParamPolicy {
    pub const SCALARS: Self = Self {
        nulls: false,
        lists: false,
        map_rows: false,
    };
    pub const COLLECTIONS: Self = Self {
        nulls: true,
        lists: true,
        map_rows: true,
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
            let value = lookup(name, kind, &mut remaining)?
                .ok_or_else(|| BindError::Missing(name.clone()))?;
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
            if !self.map_rows || *kind != ParamKind::Unwind {
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
                        192usize.saturating_add(key.len()).saturating_add(size),
                    )?;
                    row.insert(
                        key.clone(),
                        self.datum(world, &ParamKind::Scalar, input, 0)?,
                    );
                }
                rows.push(row);
            }
            return Ok(BoundParameter::Rows(rows));
        }
        let kind = if *kind == ParamKind::Unwind {
            &ParamKind::Scalar
        } else {
            kind
        };
        self.datum(world, kind, value, 0).map(BoundParameter::Value)
    }

    fn datum<T: ParameterTree>(
        self,
        world: &World,
        kind: &ParamKind,
        value: Input<'_, T>,
        depth: usize,
    ) -> Result<Datum, BindError> {
        if depth > 32 {
            return Err(BindError::Type);
        }
        let collections = !matches!(kind, ParamKind::Tick | ParamKind::Canon);
        let v = match value {
            Input::Null if collections && self.nulls => return Ok(Datum::Null),
            Input::List(xs) if collections && self.lists => {
                return xs
                    .iter()
                    .map(|x| self.datum(world, kind, x.input(), depth + 1))
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
                Value::f64(f).map_err(|_| BindError::Type)?
            }
            Input::Integer(n)
                if *kind == ParamKind::Value(ValueType::F64) && n.unsigned_abs() <= 1 << 53 =>
            {
                #[allow(clippy::cast_precision_loss)] // exact within 2^53
                Value::f64(n as f64).map_err(|_| BindError::Type)?
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
                        .ok_or(BindError::Type)?,
                )
            }
            Input::String(s) if *kind == ParamKind::Canon => Value::I64(
                s.strip_prefix("tx#")
                    .unwrap_or(s)
                    .parse::<i64>()
                    .ok()
                    .filter(|n| *n >= 0)
                    .ok_or(BindError::Type)?,
            ),
            Input::Map(_) => return Err(BindError::Invalid(MAP_RULE)),
            _ => return Err(BindError::Type),
        };
        Ok(Datum::Value(v))
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

#[derive(Debug)]
pub enum BindError {
    Missing(String),
    Row(String),
    Type,
    Invalid(&'static str),
}
impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "missing parameter `?{name}`"),
            Self::Type => f.write_str("wrong type for query parameter"),
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
                return Err(BindError::Invalid("unknown parameter"));
            }
            if texts.insert(name, text).is_some() {
                return Err(BindError::Invalid("parameter given twice"));
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
        if self.lists
            && !matches!(kind, ParamKind::Tick | ParamKind::Canon)
            && (s == "null" || s.trim_start().starts_with(['[', '{', '"']))
        {
            let json: serde_json::Value = serde_json::from_str(s)
                .map_err(|_| BindError::Invalid("invalid JSON query parameter"))?;
            return self.convert(world, kind, json.input(), remaining);
        }
        let raw: Input<'_, serde_json::Value> = match kind {
            ParamKind::Scalar | ParamKind::Unwind => s
                .parse()
                .map(Input::Integer)
                .ok()
                .or_else(|| s.parse().map(Input::Bool).ok())
                .or_else(|| numeric_float(s).map(Input::Float))
                .unwrap_or(Input::String(s)),
            ParamKind::Tick | ParamKind::Value(ValueType::I64) => {
                Input::Integer(s.parse().map_err(|_| BindError::Type)?)
            }
            ParamKind::Value(ValueType::F64) => {
                Input::Float(s.parse().map_err(|_| BindError::Type)?)
            }
            ParamKind::Value(ValueType::Bool) => {
                Input::Bool(s.parse().map_err(|_| BindError::Type)?)
            }
            _ => Input::String(s),
        };
        self.convert(world, kind, raw, remaining)
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
