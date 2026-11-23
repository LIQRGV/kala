//! Typed request shapes. Errors never contain input (including auth tokens).
use crate::error::Failure;
use crate::packstream::Value;
use std::collections::BTreeMap;
pub(crate) type Map = BTreeMap<String, Value>;
pub(crate) enum Message {
    Hello(Map),
    Logon(Map),
    Logoff,
    Run {
        text: String,
        params: Map,
        extra: Map,
    },
    Pull {
        n: i64,
        qid: i64,
        discard: bool,
    },
    Reset,
    Goodbye,
    Telemetry,
    Unsupported,
}
impl Message {
    pub fn parse(value: Value) -> Result<Self, Failure> {
        let Value::Structure(tag, fields) = value else {
            return Err(Failure::invalid());
        };
        let mut fields = fields.into_iter();
        let msg = match (tag, fields.len()) {
            (0x01, 1) => Self::Hello(map(fields.next())?),
            (0x6a, 1) => Self::Logon(map(fields.next())?),
            (0x6b, 0) => Self::Logoff,
            (0x10, 3) => {
                let Some(Value::String(text)) = fields.next() else {
                    return Err(Failure::invalid());
                };
                Self::Run {
                    text,
                    params: map(fields.next())?,
                    extra: map(fields.next())?,
                }
            }
            (0x3f | 0x2f, 1) => {
                let map = map(fields.next())?;
                let Some(Value::Integer(n)) = map.get("n") else {
                    return Err(Failure::invalid());
                };
                if *n != -1 && *n <= 0 {
                    return Err(Failure::invalid());
                }
                let qid = match map.get("qid") {
                    None => -1,
                    Some(Value::Integer(n)) if *n >= -1 => *n,
                    _ => return Err(Failure::invalid()),
                };
                Self::Pull {
                    n: *n,
                    qid,
                    discard: tag == 0x2f,
                }
            }
            (0x0f, 0) => Self::Reset,
            (0x02, 0) => Self::Goodbye,
            (0x54, 1) if matches!(fields.next(), Some(Value::Integer(0..=3))) => Self::Telemetry,
            (0x11, 1) => {
                map(fields.next())?;
                Self::Unsupported
            }
            (0x12 | 0x13, 0) | (0x66, 3) => Self::Unsupported,
            _ => return Err(Failure::invalid()),
        };
        Ok(msg)
    }
}
fn map(value: Option<Value>) -> Result<Map, Failure> {
    match value {
        Some(Value::Map(m)) => Ok(m),
        _ => Err(Failure::invalid()),
    }
}
pub(crate) fn success(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Structure(
        0x70,
        vec![Value::Map(
            entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
        )],
    )
}
