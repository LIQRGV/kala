//! Bind write parameters at the grammar positions that accept data.
use super::{Form, Tok, WriteTxn, transaction};
use crate::{DatalogError, ErrorKind, ParamKind, Span};
use kala_core::{EntryId, Value, ValueType, World};
use kala_query::ParameterInput;
use std::collections::BTreeMap;

/// A syntax-checked write form whose parameters have yet to be supplied.
#[derive(Debug)]
pub struct WriteTemplate {
    prototype: WriteTxn,
    source: Option<Form>,
    uses: Vec<ParameterUse>,
}

#[derive(Debug)]
pub(super) struct ParameterUse {
    name: String,
    span: Span,
    kind: Requirement,
}

#[derive(Debug)]
enum Requirement {
    Correction(EntryId),
    Attribute(String),
    Known(ParamKind),
}

impl WriteTemplate {
    pub(super) fn prepare(form: Form) -> Result<Self, DatalogError> {
        fn has_variable(f: &Form) -> bool {
            match f {
                Form::Atom { tok, .. } => matches!(tok, Tok::Var(_)),
                Form::Seq { items, .. } => items.iter().any(has_variable),
            }
        }

        let mut uses = Vec::new();
        let (prototype, source) = if has_variable(&form) {
            let prototype = transaction(form.clone(), false, &mut Bindings::Discover(&mut uses))?;
            (prototype, Some(form))
        } else {
            (transaction(form, false, &mut Bindings::Literal)?, None)
        };
        Ok(Self {
            prototype,
            source,
            uses,
        })
    }

    /// The form's static shape for authorization and transaction-mode checks.
    /// Parameter positions contain inert placeholders. Execute the result of
    /// [Self::bind] after admission.
    pub fn prototype(&self) -> &WriteTxn {
        &self.prototype
    }

    /// Whether the form contains any parameter references.
    pub fn is_parameterized(&self) -> bool {
        self.source.is_some()
    }

    /// Names accepted in the RUN parameter map.
    pub fn parameter_names(&self) -> impl Iterator<Item = &str> {
        self.uses.iter().map(|u| u.name.as_str())
    }

    /// Kinds for the shared parameter binder. References travel as handle or ID
    /// strings so a write may refer to an entity the transaction will create.
    pub fn parameter_kinds(&self, world: &World) -> Result<Vec<(String, ParamKind)>, DatalogError> {
        let mut kinds = BTreeMap::new();
        for usage in &self.uses {
            let kind = match &usage.kind {
                Requirement::Correction(entry) => Position::Correction(*entry).kind(Some(world)),
                Requirement::Known(kind) => kind.clone(),
                Requirement::Attribute(attr) => attribute_kind(world, attr),
            };
            let previous = kinds.entry(usage.name.clone()).or_insert(ParamKind::Scalar);
            if *previous == ParamKind::Scalar {
                *previous = kind;
            } else if *previous != kind && kind != ParamKind::Scalar {
                if matches!(
                    (&*previous, &kind),
                    (ParamKind::Tick, ParamKind::Value(ValueType::I64))
                        | (ParamKind::Value(ValueType::I64), ParamKind::Tick)
                ) {
                    *previous = ParamKind::Tick;
                } else {
                    return Err(wrong_type(usage.span));
                }
            }
        }
        Ok(kinds.into_iter().collect())
    }

    /// Bind typed values into a parsed write. A value never becomes source text.
    pub fn bind(
        &self,
        world: &World,
        params: &dyn ParameterInput,
    ) -> Result<WriteTxn, DatalogError> {
        let mut remaining = kala_core::MAX_ENTRY_BYTES;
        self.bind_with_budget(world, params, &mut remaining)
    }

    /// Like [Self::bind], with the caller's remaining allowance for bound data
    /// across all forms. Charge each occurrence before cloning its payload.
    pub fn bind_with_budget(
        &self,
        world: &World,
        params: &dyn ParameterInput,
        remaining: &mut usize,
    ) -> Result<WriteTxn, DatalogError> {
        match &self.source {
            None => Ok(self.prototype.clone()),
            Some(f) => transaction(
                f.clone(),
                false,
                &mut Bindings::Values {
                    world,
                    params,
                    remaining,
                },
            ),
        }
    }
}

fn attribute_kind(world: &World, name: &str) -> ParamKind {
    world
        .at_canon(world.latest_tx())
        .resolve_attribute(name)
        .map_or(ParamKind::Scalar, |(attr, _)| value_kind(attr.value_type))
}

fn value_kind(ty: ValueType) -> ParamKind {
    // Resolve reference data only when the transaction stages the write.
    ParamKind::Value(if ty == ValueType::Ref {
        ValueType::Str
    } else {
        ty
    })
}

#[derive(Clone, Copy)]
pub(super) enum Position<'a> {
    Reference,
    Handle,
    Tick,
    Entry,
    Attribute(&'a str),
    Correction(EntryId),
}

impl Position<'_> {
    fn kind(self, world: Option<&World>) -> ParamKind {
        match self {
            Self::Reference | Self::Handle => ParamKind::Value(ValueType::Str),
            Self::Tick => ParamKind::Tick,
            Self::Entry => ParamKind::Value(ValueType::I64),
            Self::Attribute(name) => world.map_or(ParamKind::Scalar, |w| attribute_kind(w, name)),
            Self::Correction(entry) => world
                .and_then(|w| w.fact_value_type(entry))
                .map_or(ParamKind::Scalar, value_kind),
        }
    }
}

pub(super) enum Bindings<'a> {
    Literal,
    Discover(&'a mut Vec<ParameterUse>),
    Values {
        world: &'a World,
        params: &'a dyn ParameterInput,
        remaining: &'a mut usize,
    },
}

fn wrong_type(span: Span) -> DatalogError {
    DatalogError::new(
        ErrorKind::TypeMismatch,
        Some(span),
        "wrong type for write parameter",
    )
}

impl Bindings<'_> {
    /// During discovery substitute inert typed data; running the ordinary grammar
    /// still checks every structural position before any binding or staging.
    pub(super) fn take(
        &mut self,
        f: &Form,
        position: Position<'_>,
    ) -> Result<Option<Value>, DatalogError> {
        let Form::Atom {
            tok: Tok::Var(name),
            span,
        } = f
        else {
            return Ok(None);
        };
        let value = match self {
            Self::Literal => return Ok(None),
            Self::Discover(uses) => {
                let kind = position.kind(None);
                let sample = match kind {
                    ParamKind::Tick | ParamKind::Value(ValueType::I64) => Value::I64(0),
                    _ => Value::Str(String::new()),
                };
                uses.push(ParameterUse {
                    name: name.clone(),
                    span: *span,
                    kind: match position {
                        Position::Correction(entry) => Requirement::Correction(entry),
                        Position::Attribute(name) => Requirement::Attribute(name.to_owned()),
                        _ => Requirement::Known(kind),
                    },
                });
                sample
            }
            Self::Values {
                world,
                params,
                remaining,
            } => {
                let v = params.get(name).ok_or_else(|| {
                    if params.contains_key(name) {
                        wrong_type(*span)
                    } else {
                        DatalogError::new(
                            ErrorKind::UnboundVariable,
                            Some(*span),
                            format!("missing write parameter ?{name}"),
                        )
                    }
                })?;
                let correct = match position.kind(Some(world)) {
                    ParamKind::Tick => matches!(v, Value::I64(_)),
                    ParamKind::Value(ty) => v.value_type() == ty,
                    ParamKind::Scalar => true,
                    _ => false,
                };
                if !correct
                    || (matches!(position, Position::Entry) && matches!(v, Value::I64(n) if *n < 0))
                {
                    return Err(wrong_type(*span));
                }
                let size = match v {
                    Value::Str(s) => s.len(),
                    _ => 8,
                };
                **remaining = remaining.checked_sub(size).ok_or_else(|| {
                    DatalogError::limit(
                        Some(*span),
                        "expanded write parameters exceed the transaction byte limit",
                    )
                })?;
                v.clone()
            }
        };
        Ok(Some(value))
    }
}
