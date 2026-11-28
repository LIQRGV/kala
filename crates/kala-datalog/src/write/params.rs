//! Bind write parameters at the grammar positions that accept data.
use super::{Form, Tok, WriteTxn, transaction};
use crate::{DatalogError, ErrorKind, ParamKind, Span};
use kala_core::{EntryId, Value, ValueType, World};
use kala_query::{ParameterInput, QueryParams};
use std::collections::{BTreeMap, BTreeSet};

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
    CorrectionParameter(String),
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
            let prototype = transaction(
                form.clone(),
                false,
                &mut Bindings::Discover {
                    uses: &mut uses,
                    declarations: BTreeMap::new(),
                },
            )?;
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

    /// Decode only correction-entry parameters first: a replacement value
    /// takes its type from the fact that its supplied entry number names.
    /// Both passes use the caller's usual transport conversion.
    pub fn bind_parameters<E>(
        &self,
        world: &World,
        mut convert: impl FnMut(&[(String, ParamKind)]) -> Result<QueryParams, E>,
        error: impl Fn(DatalogError) -> E,
    ) -> Result<QueryParams, E> {
        let entries: BTreeSet<_> = self
            .uses
            .iter()
            .filter_map(|u| match &u.kind {
                Requirement::CorrectionParameter(name) => Some(name),
                _ => None,
            })
            .collect();
        let targets = if entries.is_empty() {
            QueryParams::new()
        } else {
            let kinds: Vec<_> = entries
                .into_iter()
                .map(|name| (name.clone(), ParamKind::Value(ValueType::I64)))
                .collect();
            convert(&kinds)?
        };
        let kinds = self.parameter_kinds(world, &targets).map_err(error)?;
        convert(&kinds.into_iter().collect::<Vec<_>>())
    }

    fn parameter_kinds(
        &self,
        world: &World,
        targets: &dyn ParameterInput,
    ) -> Result<BTreeMap<String, ParamKind>, DatalogError> {
        let mut kinds = BTreeMap::new();
        for usage in &self.uses {
            let kind = match &usage.kind {
                Requirement::Correction(entry) => correction_kind(world, *entry),
                Requirement::CorrectionParameter(name) => match targets.get(name) {
                    Some(Value::I64(n)) if *n >= 0 => {
                        correction_kind(world, EntryId(n.unsigned_abs()))
                    }
                    None if !targets.contains_key(name) => return Err(missing(name, usage.span)),
                    _ => return Err(wrong_type(usage.span)),
                },
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
        Ok(kinds)
    }

    /// Bind typed values into a parsed write. A value never becomes source text.
    pub fn bind(
        &self,
        world: &World,
        params: &dyn ParameterInput,
    ) -> Result<WriteTxn, DatalogError> {
        let mut remaining = kala_query::MAX_STAGED_BYTES;
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
                    kinds: &self.parameter_kinds(world, params)?,
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

fn correction_kind(world: &World, entry: EntryId) -> ParamKind {
    world
        .fact_value_type(entry)
        .map_or(ParamKind::Scalar, value_kind)
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
    Correction(EntryId, Option<&'a str>),
}

impl Position<'_> {
    fn kind(self) -> ParamKind {
        match self {
            Self::Reference | Self::Handle => ParamKind::Value(ValueType::Str),
            Self::Tick => ParamKind::Tick,
            Self::Entry => ParamKind::Value(ValueType::I64),
            Self::Attribute(_) | Self::Correction(..) => ParamKind::Scalar,
        }
    }
}

pub(super) enum Bindings<'a> {
    Literal,
    Discover {
        uses: &'a mut Vec<ParameterUse>,
        declarations: BTreeMap<String, ValueType>,
    },
    Values {
        kinds: &'a BTreeMap<String, ParamKind>,
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

fn missing(name: &str, span: Span) -> DatalogError {
    DatalogError::new(
        ErrorKind::UnboundVariable,
        Some(span),
        format!("missing write parameter ?{name}"),
    )
}

impl Bindings<'_> {
    /// Later operations bind against an explicit declaration in this form.
    pub(super) fn note_declaration(&mut self, op: &crate::ast::WriteOp) {
        if let Self::Discover { declarations, .. } = self
            && let crate::ast::WriteOp::Attribute {
                name, value_type, ..
            } = op
        {
            declarations.insert(name.clone(), *value_type);
        }
    }

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
            Self::Discover { uses, declarations } => {
                let kind = position.kind();
                let sample = match kind {
                    ParamKind::Tick | ParamKind::Value(ValueType::I64) => Value::I64(0),
                    _ => Value::Str(String::new()),
                };
                uses.push(ParameterUse {
                    name: name.clone(),
                    span: *span,
                    kind: match position {
                        Position::Correction(_, Some(name)) => {
                            Requirement::CorrectionParameter(name.to_owned())
                        }
                        Position::Correction(entry, None) => Requirement::Correction(entry),
                        Position::Attribute(name) => declarations.get(name).map_or_else(
                            || Requirement::Attribute(name.to_owned()),
                            |ty| Requirement::Known(value_kind(*ty)),
                        ),
                        _ => Requirement::Known(kind),
                    },
                });
                sample
            }
            Self::Values {
                kinds,
                params,
                remaining,
            } => {
                let v = params.get(name).ok_or_else(|| {
                    if params.contains_key(name) {
                        wrong_type(*span)
                    } else {
                        missing(name, *span)
                    }
                })?;
                let correct = match kinds.get(name).ok_or_else(|| missing(name, *span))? {
                    ParamKind::Tick => matches!(v, Value::I64(_)),
                    ParamKind::Value(ty) => v.value_type() == *ty,
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
