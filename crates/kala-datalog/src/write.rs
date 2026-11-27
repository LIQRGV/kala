//! Write syntax shares the query lexer and bounded EDN reader.
use crate::ast::{Attr, Item, Replacement, WriteOp, WriteTxn, WriteValue};
use crate::error::{DatalogError, Span, show};
use crate::lex::{Bracket, Lexer, Tok};
use crate::parse::{Form, item, read_form};
use crate::{MAX_QUERY_BYTES, MAX_WRITE_FORMS, MAX_WRITE_OPS};
use kala_core::{
    AttributeOptions, Cardinality, Concurrency, EntryId, Kind, MigrationOptions, Op, Precision,
    RefDirection, Tick, ValueType,
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
                    "genesis"
                        | "concurrency"
                        | "create"
                        | "set"
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
    Ok(forms(text, false))
}

/// Parse a local restore script without byte, operation or form limits.
/// Bracket depth remains bounded by `MAX_DEPTH`; execution must require a
/// leading genesis and validate every operation through the native store.
pub fn restore_forms(text: &str) -> impl Iterator<Item = Result<WriteTxn, DatalogError>> + '_ {
    forms(text, true)
}

fn forms(text: &str, restore: bool) -> impl Iterator<Item = Result<WriteTxn, DatalogError>> + '_ {
    let mut lex = Lexer::new(text);
    let mut count = 0;
    let mut done = false;
    std::iter::from_fn(move || {
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
        let result = if !restore && count > MAX_WRITE_FORMS {
            Err(DatalogError::limit(
                None,
                format!("more than {MAX_WRITE_FORMS} transaction forms"),
            ))
        } else {
            read_form(&mut lex, text.len()).and_then(|f| transaction(f, restore))
        };
        done = result.is_err();
        Some(result.map_err(|e| e.locate(text)))
    })
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
// Schema and handle names are opaque store strings. Quoted names make native
// names outside the bare keyword alphabet representable in a dump.
fn named(f: Form) -> Result<(String, Span), DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Str(s),
            span,
        } => Ok((s, span)),
        f => keyword(f),
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
/// A write value: an edge reference `[source :attribute target]`, a tagged
/// scalar (see [`tagged_scalar`]) or a literal.
fn value(f: Form) -> Result<WriteValue, DatalogError> {
    if matches!(&f, Form::Seq { .. }) {
        let (items, span) = vector(f)?;
        if let Some(scalar) = tagged_scalar(&items) {
            return Ok(scalar);
        }
        let mut it = items.into_iter();
        let (Some(s), Some(a), Some(t), None) = (it.next(), it.next(), it.next(), it.next()) else {
            return Err(DatalogError::syntax(
                span,
                "an edge reference is [source :attribute target]",
            ));
        };
        let (name, span) = named(a)?;
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
/// The two-element vectors that are scalars: an ID as a string (`[:id "…"]`)
/// and an infinite float (`[:f64 :inf]`, `[:f64 :-inf]`).
fn tagged_scalar(items: &[Form]) -> Option<WriteValue> {
    let [
        Form::Atom {
            tok: Tok::Keyword(k),
            ..
        },
        Form::Atom { tok, span },
    ] = items
    else {
        return None;
    };
    match tok {
        Tok::Str(id) if k == "id" => Some(WriteValue::Scalar(Item::Id(id.clone(), *span))),
        Tok::Keyword(v) if k == "f64" && matches!(v.as_str(), "inf" | "-inf") => {
            Some(WriteValue::Scalar(Item::Float(
                if v == "inf" {
                    f64::INFINITY
                } else {
                    f64::NEG_INFINITY
                },
                *span,
            )))
        }
        _ => None,
    }
}
/// One transaction form: a single op `[:assert …]`, or a vector of ops with
/// an optional leading `:at tick` (or, in a restore script, `:genesis`). The
/// first op of a vector may be `[:concurrency :weak]` or `[:concurrency
/// :optimistic]`: the transaction's own concurrency mode, not an op.
fn transaction(f: Form, restore: bool) -> Result<WriteTxn, DatalogError> {
    let (mut items, span) = vector(f)?;
    let genesis =
        matches!(items.first(), Some(Form::Atom { tok: Tok::Keyword(k), .. }) if k == "genesis");
    if genesis {
        if !restore {
            return Err(DatalogError::syntax(
                span,
                ":genesis requires kala transact --create",
            ));
        }
        items.remove(0);
    }
    let single = matches!(items.first(),Some(Form::Atom{tok:Tok::Keyword(k),..}) if k!="at");
    if single && genesis {
        return Err(DatalogError::syntax(
            span,
            "genesis holds operation vectors: [:genesis [:node-type ...] [:attribute ...]]",
        ));
    }
    if single {
        return Ok(WriteTxn {
            genesis: false,
            concurrency: None,
            at: None,
            ops: vec![operation(items, span, restore)?],
            span,
        });
    }
    let Parts {
        at,
        concurrency,
        ops,
    } = transaction_ops(items, span, restore)?;
    if ops.is_empty() && !genesis {
        return Err(DatalogError::syntax(
            span,
            "a transaction needs at least one op",
        ));
    }
    if genesis && (at.is_some() || concurrency.is_some() || !ops.iter().all(is_genesis_op)) {
        return Err(DatalogError::syntax(
            span,
            "genesis accepts only declared schema forms and world settings",
        ));
    }
    Ok(WriteTxn {
        genesis,
        concurrency,
        at,
        ops,
        span,
    })
}
/// What a multi-op transaction vector holds.
struct Parts {
    /// A leading `:at tick`: the transaction's default tick.
    at: Option<Tick>,
    /// A `[:concurrency …]` as the first op.
    concurrency: Option<Concurrency>,
    ops: Vec<WriteOp>,
}
/// The parts of a multi-op transaction.
fn transaction_ops(items: Vec<Form>, span: Span, restore: bool) -> Result<Parts, DatalogError> {
    let mut it = items.into_iter().peekable();
    let mut at = None;
    let mut concurrency = None;
    let mut ops = Vec::new();
    if matches!(it.peek(), Some(Form::Atom{tok:Tok::Keyword(k),..}) if k=="at") {
        it.next();
        at =
            Some(tick(it.next().ok_or_else(|| {
                DatalogError::syntax(span, ":at needs a tick")
            })?)?);
    }
    for f in it {
        if !restore && ops.len() >= MAX_WRITE_OPS {
            return Err(DatalogError::limit(
                Some(f.span()),
                format!("more than {MAX_WRITE_OPS} operations in a transaction"),
            ));
        }
        let (items, s) = vector(f)?;
        if is_concurrency(&items) {
            if !ops.is_empty() || concurrency.is_some() {
                return Err(DatalogError::syntax(
                    s,
                    "[:concurrency …] must be the first op of a transaction, once: \
                     [[:concurrency :weak] [:set …]]",
                ));
            }
            concurrency = Some(concurrency_mode(items, s)?);
            continue;
        }
        ops.push(operation(items, s, restore)?);
    }
    Ok(Parts {
        at,
        concurrency,
        ops,
    })
}
/// Whether an op vector is a `[:concurrency …]` (well-formed or not).
fn is_concurrency(items: &[Form]) -> bool {
    matches!(items.first(), Some(Form::Atom { tok: Tok::Keyword(k), .. }) if k == "concurrency")
}
/// The mode of a `[:concurrency :weak]` / `[:concurrency :optimistic]`.
/// Pessimistic locking is a planned mode, so it gets its own message.
fn concurrency_mode(items: Vec<Form>, span: Span) -> Result<Concurrency, DatalogError> {
    let shape = "expected :weak or :optimistic: [:concurrency :weak]";
    let mut it = items.into_iter().skip(1);
    let (Some(mode), None) = (it.next(), it.next()) else {
        return Err(DatalogError::syntax(span, shape));
    };
    let Form::Atom {
        tok: Tok::Keyword(mode),
        span,
    } = mode
    else {
        return Err(DatalogError::syntax(mode.span(), shape));
    };
    match mode.as_str() {
        "weak" => Ok(Concurrency::Weak),
        "optimistic" => Ok(Concurrency::Optimistic),
        "pessimistic" => Err(DatalogError::syntax(
            span,
            "concurrency mode :pessimistic is not supported yet; expected :weak or :optimistic",
        )),
        _ => Err(DatalogError::syntax(
            span,
            format!(
                "unknown concurrency mode :{}; expected :weak or :optimistic",
                show(&mode)
            ),
        )),
    }
}
/// Whether a genesis transaction may hold `op`: a declared (not inferred)
/// schema form or a world setting.
fn is_genesis_op(op: &WriteOp) -> bool {
    matches!(
        op,
        WriteOp::Attribute {
            inferred: false,
            ..
        } | WriteOp::NodeType {
            inferred: false,
            ..
        } | WriteOp::WorldSetting { .. }
    )
}
/// The arguments of one write operation, after its keyword.
struct Args {
    it: std::vec::IntoIter<Form>,
    /// The whole operation's span.
    span: Span,
}
impl Args {
    /// The next argument, which the operation can't do without.
    fn required(&mut self) -> Result<Form, DatalogError> {
        self.it
            .next()
            .ok_or_else(|| DatalogError::syntax(self.span, "missing parts of write operation"))
    }
}
/// One write operation `[:keyword …]`: the keyword selects the form.
fn operation(items: Vec<Form>, span: Span, restore: bool) -> Result<WriteOp, DatalogError> {
    let mut args = Args {
        it: items.into_iter(),
        span,
    };
    let (kind, ks) = keyword(args.required()?)?;
    match kind.as_str() {
        "attribute" => attribute_op(args, false),
        "migrate" => attribute_op(args, true),
        "node-type" | "deprecate" | "rename" | "cardinality" | "world-setting" => {
            schema_op(&kind, args)
        }
        "genesis" => Err(DatalogError::syntax(
            span,
            ":genesis must be the first top-level form of kala transact --create",
        )),
        "concurrency" => Err(DatalogError::syntax(
            span,
            "[:concurrency …] is not a transaction by itself; put it first in a vector of \
             ops: [[:concurrency :weak] [:set …]]",
        )),
        "create" => create_op(args, restore),
        "set" => set_op(args),
        "assert" => fact_op(Op::Assert, args),
        "retract" => fact_op(Op::Retract, args),
        "correct" => correct_op(args),
        _ => Err(DatalogError::syntax(
            ks,
            format!(
                "unknown write op :{}; expected :create, :assert, :retract, :set or :correct",
                show(&kind)
            ),
        )),
    }
}
/// `[:attribute name type option…]`, or `[:migrate name option…]`.
fn attribute_op(mut args: Args, migrate: bool) -> Result<WriteOp, DatalogError> {
    let span = args.span;
    let (name, _) = named(args.required()?)?;
    let (options, clear_reverse_cardinality, inferred) = schema_options(args.it, migrate, span)?;
    if migrate {
        return Ok(WriteOp::Migrate {
            name,
            options,
            span,
        });
    }
    let value_type = options
        .value_type
        .ok_or_else(|| DatalogError::syntax(span, ":attribute needs a value type"))?;
    Ok(WriteOp::Attribute {
        inferred,
        name,
        value_type,
        options: AttributeOptions {
            kind: options.kind,
            cardinality: options.cardinality,
            direction: options.direction,
            indexed: options.indexed,
            inverse: options.inverse,
            reverse_cardinality: options.reverse_cardinality,
            clear_reverse_cardinality,
        },
        span,
    })
}
/// The schema forms that take a name and at most one more argument:
/// `[:node-type T]`, `[:deprecate a]`, `[:rename old new]`,
/// `[:cardinality a :many]` and `[:world-setting key value]`.
fn schema_op(kind: &str, mut args: Args) -> Result<WriteOp, DatalogError> {
    let span = args.span;
    let (name, _) = named(args.required()?)?;
    let op = match kind {
        "node-type" => {
            let inferred = if let Some(f) = args.it.next() {
                let (k, s) = keyword(f)?;
                if k != "inferred" {
                    return Err(DatalogError::syntax(s, "expected :inferred"));
                }
                true
            } else {
                false
            };
            WriteOp::NodeType {
                name,
                inferred,
                span,
            }
        }
        "deprecate" => WriteOp::Deprecate { name, span },
        "rename" => WriteOp::Rename {
            old: name,
            new: named(args.required()?)?.0,
            span,
        },
        "cardinality" => WriteOp::Cardinality {
            name,
            cardinality: cardinality(args.required()?)?,
            span,
        },
        _ => WriteOp::WorldSetting {
            key: name,
            value: named(args.required()?)?.0,
            span,
        },
    };
    if let Some(f) = args.it.next() {
        return Err(DatalogError::syntax(
            f.span(),
            "too many arguments to schema operation",
        ));
    }
    Ok(op)
}
/// `[:create TYPE]` or `[:create TYPE handle]`.
fn create_op(mut args: Args, restore: bool) -> Result<WriteOp, DatalogError> {
    let (node_type, _) = named(args.required()?)?;
    let handle_form = args.it.next();
    // Restore preserves the historic plain-create spelling of a handle named
    // `if-missing`. In ordinary input that keyword alone is the missing option
    // argument; a quoted handle remains an unambiguous literal.
    if !restore
        && matches!(&handle_form, Some(Form::Atom { tok: Tok::Keyword(k), .. }) if k == "if-missing")
    {
        return Err(DatalogError::syntax(
            args.span,
            ":if-missing requires a handle",
        ));
    }
    let handle = handle_form.map(named).transpose()?.map(|(name, _)| name);
    let if_missing = if let Some(f) = args.it.next() {
        let (option, span) = keyword(f)?;
        if option != "if-missing" {
            return Err(DatalogError::syntax(
                span,
                ":create takes only a type and handle (optionally :if-missing)",
            ));
        }
        true
    } else {
        false
    };
    if let Some(f) = args.it.next() {
        return Err(DatalogError::syntax(
            f.span(),
            ":create takes only a type and handle (optionally :if-missing)",
        ));
    }
    Ok(WriteOp::Create {
        node_type,
        handle,
        if_missing,
        span: args.span,
    })
}
/// `[:set subject attribute value-or-members option…]`.
///
/// The value position takes every single value `:assert` takes, or a member
/// list:
/// - a literal, a handle, or a vector that is a tagged scalar (`[:id "…"]`,
///   `[:f64 :inf]`, exactly what [`tagged_scalar`] recognises) is ONE value:
///   the one-member set;
/// - any other vector is the complete set of members (`[]` = none), each
///   parsed like an `:assert` value, so tagged scalars and edge triples work
///   as members.
///
/// So an edge triple as the single value must be wrapped
/// (`[[:edwin :ally_of :mara]]`): a bare `[:edwin :ally_of :mara]` is three
/// members.
fn set_op(mut args: Args) -> Result<WriteOp, DatalogError> {
    let span = args.span;
    let subject = value(args.required()?)?;
    if let WriteValue::Scalar(i) = &subject
        && !matches!(i, Item::Keyword(..) | Item::Id(..))
    {
        return Err(DatalogError::syntax(
            i.span(),
            "subject must be a handle, ID or edge triple",
        ));
    }
    let (name, aspan) = named(args.required()?)?;
    let members = args.required()?;
    let values = match members {
        Form::Seq {
            items,
            bracket: Bracket::Square,
            ..
        } if tagged_scalar(&items).is_none() => {
            if items.len() > MAX_WRITE_OPS {
                return Err(DatalogError::limit(
                    Some(span),
                    format!(":set exceeds {MAX_WRITE_OPS} values"),
                ));
            }
            items
                .into_iter()
                .map(value)
                .collect::<Result<Vec<_>, _>>()?
        }
        scalar => vec![value(scalar)?],
    };
    let options = options(args.it, false, span)?;
    Ok(WriteOp::Set {
        subject,
        attr: Attr { name, span: aspan },
        values,
        at: options.at,
        precision: options.precision.unwrap_or(Precision::Exact),
        span,
    })
}
/// `[:assert subject attribute value option…]`, or the same with `:retract`.
fn fact_op(op: Op, mut args: Args) -> Result<WriteOp, DatalogError> {
    let span = args.span;
    let subject = value(args.required()?)?;
    if let WriteValue::Scalar(i) = &subject
        && !matches!(i, Item::Keyword(..) | Item::Id(..))
    {
        return Err(DatalogError::syntax(
            i.span(),
            "subject must be a handle, ID or edge triple",
        ));
    }
    let (name, aspan) = named(args.required()?)?;
    let value = value(args.required()?)?;
    let options = options(args.it, false, span)?;
    Ok(WriteOp::Fact {
        op,
        subject,
        attr: Attr { name, span: aspan },
        value,
        at: options.at,
        precision: options.precision.unwrap_or(Precision::Exact),
        span,
    })
}
/// `[:correct entry :delete]` or `[:correct entry :replace option…]`.
fn correct_op(mut args: Args) -> Result<WriteOp, DatalogError> {
    let span = args.span;
    let f = args.required()?;
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
    .ok_or_else(|| DatalogError::syntax(es, "entry ID must be a non-negative u64 or entry#N"))?;
    let (mode, ms) = keyword(args.required()?)?;
    let replacement = match mode.as_str() {
        "delete" => {
            if let Some(f) = args.it.next() {
                return Err(DatalogError::syntax(f.span(), ":delete takes no options"));
            }
            None
        }
        "replace" => Some(options(args.it, true, span)?),
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
) -> Result<(MigrationOptions, bool, bool), DatalogError> {
    let mut out = MigrationOptions::default();
    let mut clear_reverse = false;
    let mut inferred = false;
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
            "indexed" => out.indexed = Some(true),
            "unindexed" => out.indexed = Some(false),
            "inverse" => {
                out.inverse = Some(
                    named(
                        it.next()
                            .ok_or_else(|| DatalogError::syntax(span, ":inverse needs a name"))?,
                    )?
                    .0,
                )
            }
            "reverse" => {
                let form = it
                    .next()
                    .ok_or_else(|| DatalogError::syntax(span, ":reverse needs cardinality"))?;
                if !migrate
                    && matches!(&form, Form::Atom { tok: Tok::Keyword(k), .. } if k == "none")
                {
                    clear_reverse = true;
                } else {
                    out.reverse_cardinality = Some(cardinality(form)?);
                }
            }
            "skip-unconvertible" if migrate => out.skip_unconvertible = true,
            // A dump's inferred declaration; validated like live inference.
            "inferred" if !migrate => inferred = true,
            _ => {
                return Err(DatalogError::syntax(
                    s,
                    format!("unknown schema option :{}", show(&name)),
                ));
            }
        }
    }
    Ok((out, clear_reverse, inferred))
}
