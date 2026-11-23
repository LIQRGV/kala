//! Backwards-compatible scalar parameters and optional nullable/list bindings.
use crate::{Datum, Params, QueryError};
use kala_core::Value;
use std::collections::BTreeMap;
/// A borrowed parameter, inspected before any copying or allocation.
#[derive(Clone, Copy)]
pub enum ParameterValue<'a> {
    /// A stored scalar.
    Scalar(&'a Value),
    /// NULL, list, or scalar.
    Datum(&'a Datum),
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
pub struct QueryParams(BTreeMap<String, Datum>);
impl QueryParams {
    /// Empty parameters.
    pub fn new() -> Self {
        Self::default()
    }
    /// Insert a supplied value.
    pub fn insert(&mut self, name: String, value: Datum) -> Option<Datum> {
        self.0.insert(name, value)
    }
}
impl ParameterInput for QueryParams {
    fn contains_key(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }
    fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name).and_then(Datum::as_value)
    }
    fn binding(&self, name: &str) -> Option<ParameterValue<'_>> {
        self.0.get(name).map(ParameterValue::Datum)
    }
}
impl From<Params> for QueryParams {
    fn from(values: Params) -> Self {
        Self(
            values
                .into_iter()
                .map(|(k, v)| (k, Datum::Value(v)))
                .collect(),
        )
    }
}
