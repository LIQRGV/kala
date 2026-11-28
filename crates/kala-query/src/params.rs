//! Backwards-compatible scalar parameters and optional nullable/list bindings.
use crate::{Datum, Params, QueryError};
use kala_core::{Value, ValueType};
use std::collections::BTreeMap;

/// Maximum number of map rows a parameter can supply.
pub const MAX_MAP_ROWS: usize = 10_000;
/// Byte budget shared by write parameter expansion and transaction staging.
pub const MAX_STAGED_BYTES: usize = 16 * 1024 * 1024;
/// One client-supplied row. Keys belong to the parameter, not the schema.
pub type ParameterRow = BTreeMap<String, Datum>;

/// A borrowed parameter, inspected before any copying or allocation.
#[derive(Clone, Copy)]
pub enum ParameterValue<'a> {
    /// A stored scalar.
    Scalar(&'a Value),
    /// NULL, list, or scalar.
    Datum(&'a Datum),
    /// An ordered table accessible only to a direct UNWIND.
    Rows(&'a [ParameterRow]),
}
/// Parameter lookup accepted by the executor. Existing `Params` stays a scalar map.
pub trait ParameterInput {
    /// Whether a parameter was supplied (NULL counts as supplied).
    fn contains_key(&self, name: &str) -> bool;
    /// A supplied scalar value, if any.
    fn get(&self, name: &str) -> Option<&Value>;
    /// A supplied value including NULL and lists.
    fn binding(&self, name: &str) -> Option<ParameterValue<'_>>;
    /// Require a stored scalar, distinguishing missing from NULL/list.
    fn scalar(&self, name: &str) -> Result<&Value, QueryError> {
        self.get(name).ok_or_else(|| {
            if self.contains_key(name) {
                QueryError::ParamType {
                    name: name.into(),
                    expected: "scalar value".into(),
                }
            } else {
                QueryError::MissingParam(name.into())
            }
        })
    }
}
impl ParameterInput for Params {
    fn contains_key(&self, name: &str) -> bool {
        BTreeMap::contains_key(self, name)
    }
    fn get(&self, name: &str) -> Option<&Value> {
        BTreeMap::get(self, name)
    }
    fn binding(&self, name: &str) -> Option<ParameterValue<'_>> {
        self.get(name).map(ParameterValue::Scalar)
    }
}
/// Query parameters that can also carry NULL or lists. Lists retain their input order.
#[derive(Clone, Debug, Default)]
pub struct QueryParams(BTreeMap<String, Binding>);

#[derive(Clone, Debug)]
enum Binding {
    Value(Datum),
    Rows(Vec<ParameterRow>),
}
impl QueryParams {
    /// Empty parameters.
    pub fn new() -> Self {
        Self::default()
    }
    /// Insert a supplied value.
    pub fn insert(&mut self, name: String, value: Datum) -> Option<Datum> {
        match self.0.insert(name, Binding::Value(value)) {
            Some(Binding::Value(previous)) => Some(previous),
            _ => None,
        }
    }
    /// Insert a table after validating all cells, including unused fields.
    pub fn insert_rows(&mut self, name: String, rows: Vec<ParameterRow>) -> Result<(), QueryError> {
        if rows.len() > MAX_MAP_ROWS {
            return Err(QueryError::RowLimit {
                limit: MAX_MAP_ROWS,
            });
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Field {
            Scalar(ValueType),
            List,
        }
        let mut fields = BTreeMap::new();
        for (index, row) in rows.iter().enumerate() {
            for (key, cell) in row {
                let kind = match cell {
                    Datum::Null => continue,
                    Datum::Value(v) => Field::Scalar(v.value_type()),
                    Datum::List(xs) if xs.iter().all(|x| matches!(x, Datum::Value(_))) => {
                        Field::List
                    }
                    _ => {
                        return Err(QueryError::TypeMismatch(format!(
                            "row {index} key {} must contain a scalar, NULL, or a list of scalars",
                            kala_core::Echo(key)
                        )));
                    }
                };
                if fields.entry(key).or_insert(kind) != &kind {
                    return Err(QueryError::TypeMismatch(format!(
                        "mixed types at row {index} key {}",
                        kala_core::Echo(key)
                    )));
                }
            }
        }
        self.0.insert(name, Binding::Rows(rows));
        Ok(())
    }
}
impl ParameterInput for QueryParams {
    fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
    fn get(&self, name: &str) -> Option<&Value> {
        match self.0.get(name)? {
            Binding::Value(v) => v.as_value(),
            Binding::Rows(_) => None,
        }
    }
    fn binding(&self, name: &str) -> Option<ParameterValue<'_>> {
        Some(match self.0.get(name)? {
            Binding::Value(v) => ParameterValue::Datum(v),
            Binding::Rows(r) => ParameterValue::Rows(r),
        })
    }
}
impl From<Params> for QueryParams {
    fn from(values: Params) -> Self {
        Self(
            values
                .into_iter()
                .map(|(k, v)| (k, Binding::Value(Datum::Value(v))))
                .collect(),
        )
    }
}
