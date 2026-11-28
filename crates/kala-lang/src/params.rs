//! Shared parameter binding for prepared queries and writes.
use kala_core::{Value, ValueType, World};
use kala_query::{Datum, ParamKind, QueryParams};

/// Parameter shapes accepted by a front-end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamPolicy {
    pub nulls: bool,
    pub lists: bool,
}

impl ParamPolicy {
    pub const SCALARS: Self = Self {
        nulls: false,
        lists: false,
    };
    pub const COLLECTIONS: Self = Self {
        nulls: true,
        lists: true,
    };

    /// Convert the parameters a statement uses. A transport supplies the
    /// name lookup and decodes its representation through `convert`.
    pub fn bind(
        self,
        kinds: &[(String, ParamKind)],
        mut lookup: impl FnMut(&str, &ParamKind) -> Result<Option<Datum>, BindError>,
    ) -> Result<QueryParams, BindError> {
        let mut out = QueryParams::new();
        for (name, kind) in kinds {
            let value = lookup(name, kind)?.ok_or_else(|| BindError::Missing(name.clone()))?;
            out.insert(name.clone(), value);
        }
        Ok(out)
    }

    /// Validate a borrowed input before converting it to an executor value.
    pub fn convert<T: ParameterTree>(
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
                    .map(|x| self.convert(world, kind, x.input(), depth + 1))
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
            _ => return Err(BindError::Type),
        };
        Ok(Datum::Value(v))
    }
}

/// Borrowed view of a decoded client value. A list is walked without a copy.
pub enum Input<'a, T> {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(&'a str),
    List(&'a [T]),
    Unsupported,
}

/// Transport-specific decoding ends at this interface.
pub trait ParameterTree: Sized {
    fn input(&self) -> Input<'_, Self>;
}

#[derive(Debug)]
pub enum BindError {
    Missing(String),
    Type,
    Invalid(&'static str),
}
impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "missing parameter `?{name}`"),
            Self::Type => f.write_str("wrong type for query parameter"),
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
            Self::Object(_) => Input::Unsupported,
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
        if self.lists
            && given.iter().fold(0usize, |n, (k, v)| {
                n.saturating_add(k.len()).saturating_add(v.len())
            }) > 1 << 20
        {
            return Err(BindError::Invalid("query parameters exceed 1 MiB"));
        }
        let mut texts = std::collections::BTreeMap::new();
        for (name, text) in given {
            let name = name
                .strip_prefix('?')
                .or_else(|| self.lists.then(|| name.strip_prefix('$')).flatten())
                .unwrap_or(name);
            if !kinds.iter().any(|(n, _)| n == name) {
                return Err(BindError::Invalid("unknown parameter"));
            }
            if texts.insert(name, text).is_some() {
                return Err(BindError::Invalid("parameter given twice"));
            }
        }
        self.bind(kinds, |name, kind| {
            texts
                .get(name)
                .map(|s| self.convert_text(world, kind, s))
                .transpose()
        })
    }

    fn convert_text(self, world: &World, kind: &ParamKind, s: &str) -> Result<Datum, BindError> {
        if self.lists
            && !matches!(kind, ParamKind::Tick | ParamKind::Canon)
            && (s == "null" || s.starts_with(['[', '"']))
        {
            let json: serde_json::Value = serde_json::from_str(s)
                .map_err(|_| BindError::Invalid("invalid JSON query parameter"))?;
            return self.convert(world, kind, json.input(), 0);
        }
        let raw: Input<'_, serde_json::Value> = match kind {
            ParamKind::Scalar => s
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
        self.convert(world, kind, raw, 0)
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
