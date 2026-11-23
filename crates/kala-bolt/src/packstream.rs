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
    let mut encoder = Encoder {
        bytes: Vec::new(),
        max: max_bytes,
        depth: max_depth.min(32),
    };
    encoder.value(value, 0)?;
    Ok(encoder.bytes)
}
/// Decode exactly one value within explicit byte and nesting limits.
/// Depth is capped at 32 even if the caller asks for more (native stack bound).
pub fn decode_with_limits(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
) -> Result<Value, Error> {
    if bytes.len() > max_bytes {
        return Err(Error("message size limit exceeded"));
    }
    let mut decoder = Decoder {
        bytes,
        depth: max_depth.min(32),
    };
    let value = decoder.value(0)?;
    if !decoder.bytes.is_empty() {
        return Err(Error("trailing PackStream data"));
    }
    Ok(value)
}
struct Decoder<'a> {
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
    fn value(&mut self, depth: usize) -> Result<Value, Error> {
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
                Value::Bytes(self.take(n)?.to_vec())
            }
            0x90..=0x9f | 0xd4..=0xd6 => {
                let n = self.len(marker)?;
                Value::List(self.sequence(n, depth)?)
            }
            0xa0..=0xaf | 0xd8..=0xda => {
                let n = self.len(marker)?;
                if n > self.bytes.len() / 2 {
                    return Err(Error("map length exceeds remaining bytes"));
                }
                let mut map = BTreeMap::new();
                for _ in 0..n {
                    let Value::String(key) = self.value(depth + 1)? else {
                        return Err(Error("map key must be a string"));
                    };
                    let value = self.value(depth + 1)?;
                    if map.insert(key, value).is_some() {
                        return Err(Error("duplicate map key"));
                    }
                }
                Value::Map(map)
            }
            0xb0..=0xbf => {
                let n = self.len(marker)?;
                let tag = self.byte()?;
                Value::Structure(tag, self.sequence(n, depth)?)
            }
            _ => return Err(Error("unsupported PackStream marker")),
        })
    }
    fn sequence(&mut self, n: usize, depth: usize) -> Result<Vec<Value>, Error> {
        if n > self.bytes.len() {
            return Err(Error("list length exceeds remaining bytes"));
        }
        let mut values = Vec::new();
        for _ in 0..n {
            values.push(self.value(depth + 1)?);
        }
        Ok(values)
    }
}
struct Encoder {
    bytes: Vec<u8>,
    max: usize,
    depth: usize,
}
impl Encoder {
    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.len() > self.max.saturating_sub(self.bytes.len()) {
            return Err(Error("message size limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
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
            Value::Map(v) => {
                self.len(v.len(), 0xa0, 0xd8)?;
                for (key, value) in v {
                    self.len(key.len(), 0x80, 0xd0)?;
                    self.put(key.as_bytes())?;
                    self.value(value, depth + 1)?;
                }
                Ok(())
            }
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
