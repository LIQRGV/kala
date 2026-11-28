//! Bounded PackStream values.
use std::collections::BTreeMap;
use std::fmt;
/// A PackStream value. Maps have string keys.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    /// A byte array (markers `0xCC`-`0xCE`). Kala never sends one; a client
    /// may, and a query parameter of this type is a type error.
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Structure(u8, Vec<Value>),
}
/// Invalid or oversized PackStream data (never includes input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub &'static str);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
impl Error {
    const ROW_COUNT: Self = Self("map rows exceed the 10000 row limit");
    const ROW_BYTES: Self = Self("map rows exceed the parameter memory budget");
    pub(crate) fn parameter_limit(&self) -> bool {
        matches!(*self, Self::ROW_COUNT | Self::ROW_BYTES)
    }
}

/// Encode one value with the standard limits.
pub fn encode(value: &Value) -> Result<Vec<u8>, Error> {
    encode_with_limits(value, 1 << 20, 32)
}
/// Decode exactly one value with the standard limits.
pub fn decode(bytes: &[u8]) -> Result<Value, Error> {
    decode_with_limits(bytes, 1 << 20, 32)
}
/// Encode one value within explicit byte and nesting limits.
pub fn encode_with_limits(
    value: &Value,
    max_bytes: usize,
    max_depth: usize,
) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let mut encoder = Encoder {
        bytes: Some(&mut bytes),
        size: 0,
        max: max_bytes,
        depth: max_depth.min(32),
    };
    encoder.value(value, 0)?;
    Ok(bytes)
}
/// Decode exactly one value within explicit byte and nesting limits.
/// Depth is capped at 32 even if the caller asks for more (native stack bound).
pub fn decode_with_limits(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
) -> Result<Value, Error> {
    decode_frame(bytes, max_bytes, max_depth, None)
}

/// Decode a request, bounding the expanded representation of its RUN parameters
/// before allocating the vectors, maps or strings for a row table.
pub(crate) fn decode_request(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
    map_budget: usize,
) -> Result<Value, Error> {
    decode_frame(bytes, max_bytes, max_depth, Some(map_budget))
}

fn decode_frame(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
    map_budget: Option<usize>,
) -> Result<Value, Error> {
    if bytes.len() > max_bytes {
        return Err(Error("message size limit exceeded"));
    }
    let mut decoder = Decoder {
        bytes,
        map_budget,
        depth: max_depth.min(32),
    };
    let value = decoder.value(0, Area::Other)?;
    if !decoder.bytes.is_empty() {
        return Err(Error("trailing PackStream data"));
    }
    Ok(value)
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Area {
    Other,
    /// The second field of RUN contains parameter names and their values.
    Parameters,
    Parameter,
    /// A map in a parameter and all values nested inside that map.
    Row,
}

struct Decoder<'a> {
    map_budget: Option<usize>,
    bytes: &'a [u8],
    depth: usize,
}
impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.bytes.len() {
            return Err(Error("truncated PackStream value"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn number<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn len(&mut self, marker: u8) -> Result<usize, Error> {
        match marker {
            0x80..=0xbf => Ok(usize::from(marker & 15)),
            0xcc | 0xd0 | 0xd4 | 0xd8 => Ok(usize::from(self.byte()?)),
            0xcd | 0xd1 | 0xd5 | 0xd9 => Ok(usize::from(u16::from_be_bytes(self.number()?))),
            0xce | 0xd2 | 0xd6 | 0xda => usize::try_from(u32::from_be_bytes(self.number()?))
                .map_err(|_| Error("length overflow")),
            _ => Err(Error("invalid length marker")),
        }
    }
    fn charge_row(&mut self, bytes: usize) -> Result<(), Error> {
        if let Some(left) = &mut self.map_budget {
            *left = left.checked_sub(bytes).ok_or(Error::ROW_BYTES)?;
        }
        Ok(())
    }
    fn value(&mut self, depth: usize, area: Area) -> Result<Value, Error> {
        if depth > self.depth {
            return Err(Error("PackStream depth limit exceeded"));
        }
        let marker = self.byte()?;
        Ok(match marker {
            0x00..=0x7f | 0xf0..=0xff => Value::Integer(i64::from(marker as i8)),
            0xc0 => Value::Null,
            0xc2 => Value::Bool(false),
            0xc3 => Value::Bool(true),
            0xc1 => Value::Float(f64::from_be_bytes(self.number()?)),
            0xc8 => Value::Integer(i64::from(self.byte()? as i8)),
            0xc9 => Value::Integer(i64::from(i16::from_be_bytes(self.number()?))),
            0xca => Value::Integer(i64::from(i32::from_be_bytes(self.number()?))),
            0xcb => Value::Integer(i64::from_be_bytes(self.number()?)),
            0x80..=0x8f | 0xd0..=0xd2 => {
                let n = self.len(marker)?;
                if area == Area::Row {
                    self.charge_row(n)?;
                }
                Value::String(
                    std::str::from_utf8(self.take(n)?)
                        .map_err(|_| Error("invalid UTF-8"))?
                        .to_owned(),
                )
            }
            0xcc..=0xce => {
                // `take` checks `n` against the bytes actually remaining
                // before anything is copied.
                let n = self.len(marker)?;
                if area == Area::Row {
                    self.charge_row(n)?;
                }
                Value::Bytes(self.take(n)?.to_vec())
            }
            0x90..=0x9f | 0xd4..=0xd6 => {
                let n = self.len(marker)?;
                Value::List(self.sequence(n, depth, area)?)
            }
            0xa0..=0xaf | 0xd8..=0xda => {
                let n = self.len(marker)?;
                if n > self.bytes.len() / 2 {
                    return Err(Error("map length exceeds remaining bytes"));
                }
                let field_area = match area {
                    Area::Parameters => Area::Parameter,
                    Area::Parameter | Area::Row => {
                        self.charge_row(n.saturating_mul(kala_lang::params::MAP_FIELD_BYTES))?;
                        Area::Row
                    }
                    Area::Other => Area::Other,
                };
                let mut map = BTreeMap::new();
                for _ in 0..n {
                    if !matches!(self.bytes.first(), Some(0x80..=0x8f | 0xd0..=0xd2)) {
                        return Err(Error("map key must be a string"));
                    }
                    let Value::String(key) = self.value(
                        depth + 1,
                        if field_area == Area::Row {
                            Area::Row
                        } else {
                            Area::Other
                        },
                    )?
                    else {
                        return Err(Error("map key must be a string"));
                    };
                    let value = self.value(depth + 1, field_area)?;
                    if map.insert(key, value).is_some() {
                        return Err(Error("duplicate map key"));
                    }
                }
                Value::Map(map)
            }
            0xb0..=0xbf => {
                let n = self.len(marker)?;
                let tag = self.byte()?;
                if depth == 0 && tag == 0x10 && n == 3 && self.map_budget.is_some() {
                    Value::Structure(
                        tag,
                        vec![
                            self.value(1, Area::Other)?,
                            self.value(1, Area::Parameters)?,
                            self.value(1, Area::Other)?,
                        ],
                    )
                } else {
                    Value::Structure(tag, self.sequence(n, depth, area)?)
                }
            }
            _ => return Err(Error("unsupported PackStream marker")),
        })
    }
    fn sequence(&mut self, n: usize, depth: usize, area: Area) -> Result<Vec<Value>, Error> {
        if n > self.bytes.len() {
            return Err(Error("list length exceeds remaining bytes"));
        }
        if area == Area::Row {
            self.charge_row(n.saturating_mul(std::mem::size_of::<Value>()))?;
        }
        let mut table = false;
        let mut values = Vec::new();
        for _ in 0..n {
            if area == Area::Parameter
                && !table
                && matches!(self.bytes.first(), Some(0xa0..=0xaf | 0xd8..=0xda))
            {
                if n > kala_query::MAX_MAP_ROWS {
                    return Err(Error::ROW_COUNT);
                }
                self.charge_row(n.saturating_mul(std::mem::size_of::<Value>()))?;
                table = true;
            }
            values.push(self.value(depth + 1, area)?);
        }
        Ok(values)
    }
}
/// Count the encoded parameter map with the same markers as the encoder.
pub(crate) fn map_size(map: &BTreeMap<String, Value>, max: usize) -> Result<usize, Error> {
    let mut counter = Encoder {
        bytes: None,
        size: 0,
        max,
        depth: 32,
    };
    counter.map(map, 0)?;
    Ok(counter.size)
}
struct Encoder<'a> {
    bytes: Option<&'a mut Vec<u8>>,
    size: usize,
    max: usize,
    depth: usize,
}
impl Encoder<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() > self.max.saturating_sub(self.size) {
            return Err(Error("message size limit exceeded"));
        }
        self.size += bytes.len();
        if let Some(buf) = &mut self.bytes {
            buf.extend_from_slice(bytes);
        }
        Ok(())
    }
    fn len(&mut self, n: usize, tiny: u8, wide: u8) -> Result<(), Error> {
        if n < 16 {
            self.put(&[tiny | n as u8])
        } else if n <= u8::MAX as usize {
            self.put(&[wide, n as u8])
        } else if n <= u16::MAX as usize {
            self.put(&[wide + 1])?;
            self.put(&(n as u16).to_be_bytes())
        } else {
            let n = u32::try_from(n).map_err(|_| Error("length overflow"))?;
            self.put(&[wide + 2])?;
            self.put(&n.to_be_bytes())
        }
    }
    fn map(&mut self, map: &BTreeMap<String, Value>, depth: usize) -> Result<(), Error> {
        self.len(map.len(), 0xa0, 0xd8)?;
        for (key, value) in map {
            self.len(key.len(), 0x80, 0xd0)?;
            self.put(key.as_bytes())?;
            self.value(value, depth + 1)?;
        }
        Ok(())
    }
    fn value(&mut self, value: &Value, depth: usize) -> Result<(), Error> {
        if depth > self.depth {
            return Err(Error("PackStream depth limit exceeded"));
        }
        match value {
            Value::Null => self.put(&[0xc0]),
            Value::Bool(v) => self.put(&[if *v { 0xc3 } else { 0xc2 }]),
            Value::Integer(v) => match *v {
                -16..=127 => self.put(&[*v as u8]),
                -128..=-17 => self.put(&[0xc8, *v as u8]),
                -32768..=32767 => {
                    self.put(&[0xc9])?;
                    self.put(&(*v as i16).to_be_bytes())
                }
                -2147483648..=2147483647 => {
                    self.put(&[0xca])?;
                    self.put(&(*v as i32).to_be_bytes())
                }
                _ => {
                    self.put(&[0xcb])?;
                    self.put(&v.to_be_bytes())
                }
            },
            Value::Float(v) => {
                self.put(&[0xc1])?;
                self.put(&v.to_be_bytes())
            }
            Value::String(v) => {
                self.len(v.len(), 0x80, 0xd0)?;
                self.put(v.as_bytes())
            }
            Value::Bytes(v) => {
                if v.len() < 256 {
                    self.put(&[0xcc, v.len() as u8])?;
                } else if let Ok(n) = u16::try_from(v.len()) {
                    self.put(&[0xcd])?;
                    self.put(&n.to_be_bytes())?;
                } else {
                    let n = u32::try_from(v.len()).map_err(|_| Error("bytes too long"))?;
                    self.put(&[0xce])?;
                    self.put(&n.to_be_bytes())?;
                }
                self.put(v)
            }
            Value::List(v) => {
                self.len(v.len(), 0x90, 0xd4)?;
                for item in v {
                    self.value(item, depth + 1)?;
                }
                Ok(())
            }
            Value::Map(v) => self.map(v, depth),
            Value::Structure(tag, fields) => {
                if fields.len() > 15 {
                    return Err(Error("too many structure fields"));
                }
                self.put(&[0xb0 | fields.len() as u8, *tag])?;
                for item in fields {
                    self.value(item, depth + 1)?;
                }
                Ok(())
            }
        }
    }
}
