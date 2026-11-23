//! Write syntax shares the query lexer and bounded EDN reader.
use crate::ast::{Attr, Item, Replacement, WriteOp, WriteTxn, WriteValue};
use crate::error::{DatalogError, Span, show};
use crate::lex::{Bracket, Lexer, Tok};
use crate::parse::{Form, item, read_form};
use crate::{MAX_QUERY_BYTES, MAX_WRITE_FORMS, MAX_WRITE_OPS};
use kala_core::{
    AttrDef, Cardinality, EntryId, Kind, MigrationOptions, Op, Precision, RefDirection, Tick,
    ValueType,
};

/// The first element of a form selects its language operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormKind {
    Query,
    Write,
}

fn size(text: &str) -> Result<(), DatalogError> {
    if text.len() > MAX_QUERY_BYTES {
        Err(DatalogError::limit(
            None,
            format!("input exceeds {MAX_QUERY_BYTES} bytes"),
        ))
    } else {
        Ok(())
    }
}

/// Classify a form by its first element, using the shared lexer.
pub fn form_kind(text: &str) -> Result<FormKind, DatalogError> {
    let run = || {
        size(text)?;
        let mut lex = Lexer::new(text);
        let shape = "expected a query `[:find ...]` or a write `[:assert ...]` / `[[...]]`";
        if !matches!(
            lex.next_token()?.map(|t| t.tok),
            Some(Tok::Open(Bracket::Square))
        ) {
            return Err(DatalogError::syntax(Span::new(0, 0), shape));
        }
        let t = lex
            .next_token()?
            .ok_or_else(|| DatalogError::syntax(Span::new(0, text.len()), shape))?;
        match t.tok {
            Tok::Keyword(k) if k == "find" => Ok(FormKind::Query),
            Tok::Keyword(k)
                if matches!(
                    k.as_str(),
                    "create"
                        | "assert"
                        | "retract"
                        | "correct"
                        | "at"
                        | "attribute"
                        | "node-type"
                        | "rename"
                        | "deprecate"
                        | "cardinality"
                        | "migrate"
                        | "world-setting"
                ) =>
            {
                Ok(FormKind::Write)
            }
            Tok::Open(Bracket::Square) => Ok(FormKind::Write),
            _ => Err(DatalogError::syntax(t.span, shape)),
        }
    };
    run().map_err(|e| e.locate(text))
}

/// Parse exactly one transaction, either a single op or a vector of ops.
pub fn parse_write(text: &str) -> Result<WriteTxn, DatalogError> {
    let mut forms = write_forms(text)?;
    let first = forms.next().ok_or_else(|| {
        DatalogError::syntax(Span::new(0, text.len()), "expected a write transaction").locate(text)
    })??;
    if let Some(extra) = forms.next() {
        let extra = extra?;
        return Err(
            DatalogError::syntax(extra.span, "expected one write transaction").locate(text),
        );
    }
    Ok(first)
}

/// Parse all transactions, sharing the input, depth, op and form limits.
pub fn parse_writes(text: &str) -> Result<Vec<WriteTxn>, DatalogError> {
    write_forms(text)?.collect()
}

/// Iterate transaction forms in order, so an error in a later form does not
/// prevent a caller from committing earlier forms. Stops after the first error.
pub fn write_forms(
    text: &str,
) -> Result<impl Iterator<Item = Result<WriteTxn, DatalogError>> + '_, DatalogError> {
    size(text)?;
    let mut lex = Lexer::new(text);
    let mut count = 0;
    let mut done = false;
    Ok(std::iter::from_fn(move || {
        if done {
            return None;
        }
        // Peek with a cheap lexer copy to distinguish EOF from an empty form.
        let mut peek = lex.clone();
        match peek.next_token() {
            Ok(None) => {
                done = true;
                return None;
            }
            Err(e) => {
                done = true;
                return Some(Err(e.locate(text)));
            }
            Ok(Some(_)) => {}
        }
        count += 1;
        let result = if count > MAX_WRITE_FORMS {
            Err(DatalogError::limit(
                None,
                format!("more than {MAX_WRITE_FORMS} transaction forms"),
            ))
        } else {
            read_form(&mut lex, text.len()).and_then(transaction)
        };
        done = result.is_err();
        Some(result.map_err(|e| e.locate(text)))
    }))
}

fn vector(f: Form) -> Result<(Vec<Form>, Span), DatalogError> {
    match f {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span,
        } => Ok((items, span)),
        f => Err(DatalogError::syntax(f.span(), "a write is a vector")),
    }
}
fn keyword(f: Form) -> Result<(String, Span), DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Keyword(k),
            span,
        } => Ok((k, span)),
        f => Err(DatalogError::syntax(f.span(), "expected a keyword")),
    }
}
fn tick(f: Form) -> Result<Tick, DatalogError> {
    let span = f.span();
    match f {
        Form::Atom {
            tok: Tok::Int(Some(n)),
            ..
        } => i64::try_from(n)
            .map(Tick)
            .map_err(|_| DatalogError::syntax(span, "tick must fit a signed 64-bit integer")),
        _ => Err(DatalogError::syntax(
            span,
            "tick must be a signed 64-bit integer",
        )),
    }
}
pub(crate) fn precision_name(k: &str) -> Option<Precision> {
    Some(match k {
        "exact" => Precision::Exact,
        "day" => Precision::Day,
        "month" => Precision::Month,
        "year" => Precision::Year,
        "decade" => Precision::Decade,
        "century" => Precision::Century,
        _ => return None,
    })
}
fn precision(f: Form) -> Result<Precision, DatalogError> {
    let (k, s) = keyword(f)?;
    precision_name(&k).ok_or_else(|| {
        DatalogError::syntax(
            s,
            "precision must be :exact, :day, :month, :year, :decade or :century",
        )
    })
}
fn reference(f: Form) -> Result<Item, DatalogError> {
    match item(f)? {
        i @ (Item::Keyword(..) | Item::Id(..)) => Ok(i),
        i => Err(DatalogError::syntax(
            i.span(),
            "a reference is a handle keyword or an ID",
        )),
    }
}
fn value(f: Form) -> Result<WriteValue, DatalogError> {
    if matches!(&f, Form::Seq { .. }) {
        let (items, span) = vector(f)?;
        let mut it = items.into_iter();
        let (Some(s), Some(a), Some(t), None) = (it.next(), it.next(), it.next(), it.next()) else {
            return Err(DatalogError::syntax(
                span,
                "an edge reference is [source :attribute target]",
            ));
        };
        let (name, span) = keyword(a)?;
        Ok(WriteValue::Edge {
            source: reference(s)?,
            attr: Attr { name, span },
            target: reference(t)?,
        })
    } else {
        match item(f)? {
            i @ (Item::Keyword(..)
            | Item::Id(..)
            | Item::Str(..)
            | Item::Int(..)
            | Item::Float(..)
            | Item::Bool(..)) => Ok(WriteValue::Scalar(i)),
            i => Err(DatalogError::syntax(
                i.span(),
                "writes take literal values, not variables or wildcards",
            )),
        }
    }
}
fn transaction(f: Form) -> Result<WriteTxn, DatalogError> {
    let (items, span) = vector(f)?;
    let single = matches!(items.first(),Some(Form::Atom{tok:Tok::Keyword(k),..}) if k!="at");
    if single {
        return Ok(WriteTxn {
            at: None,
            ops: vec![operation(items, span)?],
            span,
        });
    }
    let mut it = items.into_iter();
    let mut at = None;
    let mut ops = Vec::new();
    if let Some(first) = it.next() {
        if matches!(&first,Form::Atom{tok:Tok::Keyword(k),..} if k=="at") {
            at =
                Some(tick(it.next().ok_or_else(|| {
                    DatalogError::syntax(span, ":at needs a tick")
                })?)?);
        } else {
            let (items, s) = vector(first)?;
            ops.push(operation(items, s)?);
        }
    }
    for f in it {
        if ops.len() >= MAX_WRITE_OPS {
            return Err(DatalogError::limit(
                Some(f.span()),
                format!("more than {MAX_WRITE_OPS} operations in a transaction"),
            ));
        }
        let (items, s) = vector(f)?;
        ops.push(operation(items, s)?);
    }
    if ops.is_empty() {
        return Err(DatalogError::syntax(
            span,
            "a transaction needs at least one op",
        ));
    }
    Ok(WriteTxn { at, ops, span })
}
fn operation(items: Vec<Form>, span: Span) -> Result<WriteOp, DatalogError> {
    let mut it = items.into_iter();
    let missing = || DatalogError::syntax(span, "missing parts of write operation");
    let (kind, ks) = keyword(it.next().ok_or_else(missing)?)?;
    match kind.as_str() {
        "attribute" | "migrate" => {
            let (name, _) = keyword(it.next().ok_or_else(missing)?)?;
            let options = schema_options(it, kind == "migrate", span)?;
            if kind == "migrate" {
                return Ok(WriteOp::Migrate {
                    name,
                    options,
                    span,
                });
            }
            let ty = options
                .value_type
                .ok_or_else(|| DatalogError::syntax(span, ":attribute needs a value type"))?;
            let mut def = AttrDef::new(
                &name,
                ty,
                options.kind.unwrap_or(Kind::State),
                options.cardinality.unwrap_or(Cardinality::One),
            );
            if let Some(v) = options.direction {
                def.direction = Some(v);
            }
            if let Some(v) = options.indexed {
                def.indexed = v;
            }
            def.inverse = options.inverse;
            def.reverse_cardinality = options.reverse_cardinality;
            Ok(WriteOp::Attribute { def, span })
        }
        "node-type" | "deprecate" | "rename" | "cardinality" | "world-setting" => {
            let (name, _) = keyword(it.next().ok_or_else(missing)?)?;
            let op = match kind.as_str() {
                "node-type" => WriteOp::NodeType { name, span },
                "deprecate" => WriteOp::Deprecate { name, span },
                "rename" => WriteOp::Rename {
                    old: name,
                    new: keyword(it.next().ok_or_else(missing)?)?.0,
                    span,
                },
                "cardinality" => WriteOp::Cardinality {
                    name,
                    cardinality: cardinality(it.next().ok_or_else(missing)?)?,
                    span,
                },
                _ => WriteOp::WorldSetting {
                    key: name,
                    value: keyword(it.next().ok_or_else(missing)?)?.0,
                    span,
                },
            };
            if let Some(f) = it.next() {
                return Err(DatalogError::syntax(
                    f.span(),
                    "too many arguments to schema operation",
                ));
            }
            Ok(op)
        }
        "create" => {
            let (node_type, _) = keyword(it.next().ok_or_else(missing)?)?;
            let (handle, _) = keyword(it.next().ok_or_else(missing)?)?;
            if let Some(f) = it.next() {
                return Err(DatalogError::syntax(
                    f.span(),
                    ":create takes only a type and handle",
                ));
            }
            Ok(WriteOp::Create {
                node_type,
                handle,
                span,
            })
        }
        "assert" | "retract" => {
            let subject = value(it.next().ok_or_else(missing)?)?;
            if let WriteValue::Scalar(i) = &subject
                && !matches!(i, Item::Keyword(..) | Item::Id(..))
            {
                return Err(DatalogError::syntax(
                    i.span(),
                    "subject must be a handle, ID or edge triple",
                ));
            }
            let (name, aspan) = keyword(it.next().ok_or_else(missing)?)?;
            let value = value(it.next().ok_or_else(missing)?)?;
            let options = options(it, false, span)?;
            Ok(WriteOp::Fact {
                op: if kind == "assert" {
                    Op::Assert
                } else {
                    Op::Retract
                },
                subject,
                attr: Attr { name, span: aspan },
                value,
                at: options.at,
                precision: options.precision.unwrap_or(Precision::Exact),
                span,
            })
        }
        "correct" => {
            let f = it.next().ok_or_else(missing)?;
            let es = f.span();
            let n = match f {
                Form::Atom {
                    tok: Tok::Int(n), ..
                } => n.and_then(|n| u64::try_from(n).ok()),
                Form::Atom {
                    tok: Tok::Id(s), ..
                } => s.strip_prefix("entry#").and_then(|s| s.parse().ok()),
                _ => None,
            }
            .ok_or_else(|| {
                DatalogError::syntax(es, "entry ID must be a non-negative u64 or entry#N")
            })?;
            let (mode, ms) = keyword(it.next().ok_or_else(missing)?)?;
            let replacement = match mode.as_str() {
                "delete" => {
                    if let Some(f) = it.next() {
                        return Err(DatalogError::syntax(f.span(), ":delete takes no options"));
                    }
                    None
                }
                "replace" => Some(options(it, true, span)?),
                _ => {
                    return Err(DatalogError::syntax(
                        ms,
                        "correction needs :delete or :replace",
                    ));
                }
            };
            Ok(WriteOp::Correct {
                entry: EntryId(n),
                replacement,
                span,
            })
        }
        _ => Err(DatalogError::syntax(
            ks,
            format!(
                "unknown write op :{}; expected :create, :assert, :retract or :correct",
                show(&kind)
            ),
        )),
    }
}
fn options(
    mut it: impl Iterator<Item = Form>,
    replace: bool,
    span: Span,
) -> Result<Replacement, DatalogError> {
    let mut out = Replacement::default();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(f) = it.next() {
        let (k, s) = keyword(f)?;
        if !seen.insert(k.clone()) {
            return Err(DatalogError::syntax(
                s,
                format!(":{} appears twice", show(&k)),
            ));
        }
        let v = it
            .next()
            .ok_or_else(|| DatalogError::syntax(span, "write option needs a value"))?;
        match k.as_str() {
            "at" => out.at = Some(tick(v)?),
            "precision" => out.precision = Some(precision(v)?),
            "value" if replace => out.value = Some(value(v)?),
            "op" if replace => {
                let (o, s) = keyword(v)?;
                out.op = Some(match o.as_str() {
                    "assert" => Op::Assert,
                    "retract" => Op::Retract,
                    _ => return Err(DatalogError::syntax(s, "op must be :assert or :retract")),
                });
            }
            _ => {
                return Err(DatalogError::syntax(
                    s,
                    format!("unknown write option :{}", show(&k)),
                ));
            }
        }
    }
    Ok(out)
}

fn cardinality(form: Form) -> Result<Cardinality, DatalogError> {
    let (name, span) = keyword(form)?;
    match name.as_str() {
        "one" => Ok(Cardinality::One),
        "many" => Ok(Cardinality::Many),
        _ => Err(DatalogError::syntax(
            span,
            "cardinality must be :one or :many",
        )),
    }
}
fn schema_options(
    mut it: impl Iterator<Item = Form>,
    migrate: bool,
    span: Span,
) -> Result<MigrationOptions, DatalogError> {
    let mut out = MigrationOptions::default();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(form) = it.next() {
        let (name, s) = keyword(form)?;
        let category = match name.as_str() {
            "string" | "i64" | "f64" | "bool" | "ref" => "type",
            "one" | "many" => "cardinality",
            "state" | "event" => "kind",
            "directed" | "bidirectional" => "direction",
            other => other,
        };
        if !seen.insert(category.to_owned()) {
            return Err(DatalogError::syntax(
                s,
                format!("duplicate {category} option"),
            ));
        }
        if !migrate && seen.len() == 1 && category != "type" {
            return Err(DatalogError::syntax(
                s,
                "attribute type must follow its name",
            ));
        }
        match name.as_str() {
            "string" => out.value_type = Some(ValueType::Str),
            "i64" => out.value_type = Some(ValueType::I64),
            "f64" => out.value_type = Some(ValueType::F64),
            "bool" => out.value_type = Some(ValueType::Bool),
            "ref" => out.value_type = Some(ValueType::Ref),
            "one" => out.cardinality = Some(Cardinality::One),
            "many" => out.cardinality = Some(Cardinality::Many),
            "state" => out.kind = Some(Kind::State),
            "event" => out.kind = Some(Kind::Event),
            "directed" => out.direction = Some(RefDirection::Directed),
            "bidirectional" => out.direction = Some(RefDirection::Bidirectional),
            "unindexed" => out.indexed = Some(false),
            "inverse" => {
                out.inverse = Some(
                    keyword(
                        it.next()
                            .ok_or_else(|| DatalogError::syntax(span, ":inverse needs a name"))?,
                    )?
                    .0,
                )
            }
            "reverse" => {
                out.reverse_cardinality =
                    Some(cardinality(it.next().ok_or_else(|| {
                        DatalogError::syntax(span, ":reverse needs cardinality")
                    })?)?)
            }
            "skip-unconvertible" if migrate => out.skip_unconvertible = true,
            _ => {
                return Err(DatalogError::syntax(
                    s,
                    format!("unknown schema option :{}", show(&name)),
                ));
            }
        }
    }
    Ok(out)
}
