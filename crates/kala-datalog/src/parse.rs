//! Parser: tokens → a small EDN tree (built with an explicit stack, depth
//! limited) → the [`ast::Query`].
//!
//! The tree is at most [`MAX_DEPTH`] deep by construction, so everything
//! after the tree builder may walk it without worrying about the stack.

use crate::ast::{
    Attr, CanonExpr, Clause, CmpOp, Input, Item, Pattern, Pred, Query, Sym, TickExpr,
};
use crate::error::{DatalogError, Span, show};
use crate::lex::{Bracket, Lexer, Tok};
use crate::{MAX_DEPTH, MAX_PATTERNS, MAX_QUERY_BYTES};

/// Message tail for grammar that is planned but not built yet.
const SLICE7: &str = "planned for slice 7";

/// A read EDN form.
#[derive(Debug)]
enum Form {
    Seq {
        bracket: Bracket,
        items: Vec<Form>,
        span: Span,
    },
    Atom {
        tok: Tok,
        span: Span,
    },
}

impl Form {
    fn span(&self) -> Span {
        match self {
            Form::Seq { span, .. } | Form::Atom { span, .. } => *span,
        }
    }
}

/// Parse a Datalog query (see the crate docs for the grammar).
pub(crate) fn parse(text: &str) -> Result<Query, DatalogError> {
    parse_unlocated(text).map_err(|e| e.locate(text))
}

fn parse_unlocated(text: &str) -> Result<Query, DatalogError> {
    // Size first: nothing else looks at an oversized input.
    if text.len() > MAX_QUERY_BYTES {
        return Err(DatalogError::limit(
            None,
            format!(
                "query is {} bytes; the limit is {MAX_QUERY_BYTES}",
                text.len()
            ),
        ));
    }
    let mut lex = Lexer::new(text);
    let form = read_form(&mut lex, text.len())?;
    if let Some(t) = lex.next_token()? {
        return Err(DatalogError::syntax(
            t.span,
            "unexpected input after the query",
        ));
    }
    query(form)
}

/// Read one complete top-level form. Iterative, so input nesting never
/// touches the call stack; refuses nesting deeper than [`MAX_DEPTH`].
fn read_form(lex: &mut Lexer<'_>, len: usize) -> Result<Form, DatalogError> {
    // (bracket, span of the opening bracket, items so far)
    let mut stack: Vec<(Bracket, Span, Vec<Form>)> = Vec::new();
    loop {
        let Some(t) = lex.next_token()? else {
            return Err(match stack.last() {
                Some((b, open, _)) => {
                    DatalogError::syntax(*open, format!("unclosed `{}`", b.open()))
                }
                None => DatalogError::syntax(Span::new(len, len), "empty query"),
            });
        };
        let form = match t.tok {
            Tok::Open(b) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(DatalogError::limit(
                        Some(t.span),
                        format!("nesting is deeper than {MAX_DEPTH} levels"),
                    ));
                }
                stack.push((b, t.span, Vec::new()));
                continue;
            }
            Tok::Close(b) => {
                let Some((ob, open, items)) = stack.pop() else {
                    return Err(DatalogError::syntax(
                        t.span,
                        format!("unmatched `{}`", b.close()),
                    ));
                };
                if ob != b {
                    return Err(DatalogError::syntax(
                        t.span,
                        format!(
                            "expected `{}` to close `{}`, found `{}`",
                            ob.close(),
                            ob.open(),
                            b.close()
                        ),
                    ));
                }
                Form::Seq {
                    bracket: b,
                    items,
                    span: Span::new(open.start, t.span.end),
                }
            }
            tok => Form::Atom { tok, span: t.span },
        };
        match stack.last_mut() {
            Some((_, _, items)) => items.push(form),
            None => return Ok(form),
        }
    }
}

/// Query sections in their required order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Section {
    Find,
    In,
    At,
    Canon,
    Where,
}

impl Section {
    fn name(self) -> &'static str {
        match self {
            Section::Find => ":find",
            Section::In => ":in",
            Section::At => ":at",
            Section::Canon => ":canon",
            Section::Where => ":where",
        }
    }
}

fn query(form: Form) -> Result<Query, DatalogError> {
    let (items, qspan) = match form {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span,
        } => (items, span),
        other => {
            return Err(DatalogError::syntax(
                other.span(),
                "a query is a vector: `[:find ?v .. :where ..]`",
            ));
        }
    };

    // Split into sections: (section, header span, body forms).
    let mut sections: Vec<(Section, Span, Vec<Form>)> = Vec::new();
    for f in items {
        if let Form::Atom {
            tok: Tok::Keyword(k),
            span,
        } = &f
        {
            let sec = match k.as_str() {
                "find" => Section::Find,
                "in" => Section::In,
                "at" => Section::At,
                "canon" => Section::Canon,
                "where" => Section::Where,
                "rules" => {
                    return Err(DatalogError::unsupported(
                        *span,
                        format!("`:rules` is not supported yet ({SLICE7})"),
                    ));
                }
                other => {
                    return Err(DatalogError::syntax(
                        *span,
                        format!(
                            "unknown section `:{}` (expected :find, :in, :at, :canon or :where)",
                            show(other)
                        ),
                    ));
                }
            };
            if let Some((prev, _, _)) = sections.last() {
                if *prev == sec {
                    return Err(DatalogError::syntax(
                        *span,
                        format!("section `{}` appears twice", sec.name()),
                    ));
                }
                if *prev > sec {
                    return Err(DatalogError::syntax(
                        *span,
                        format!(
                            "section `{}` must come before `{}` (order: :find :in :at :canon :where)",
                            sec.name(),
                            prev.name()
                        ),
                    ));
                }
            } else if sec != Section::Find {
                return Err(DatalogError::syntax(*span, "a query starts with `:find`"));
            }
            sections.push((sec, *span, Vec::new()));
            continue;
        }
        match sections.last_mut() {
            Some((_, _, body)) => body.push(f),
            None => {
                return Err(DatalogError::syntax(
                    f.span(),
                    "a query starts with `:find`",
                ));
            }
        }
    }

    let mut q = Query {
        find: Vec::new(),
        inputs: Vec::new(),
        at: None,
        canon: None,
        clauses: Vec::new(),
        span: qspan,
    };
    let mut saw_where = false;
    for (sec, head, body) in sections {
        if body.is_empty() {
            return Err(DatalogError::syntax(
                head,
                format!("section `{}` is empty", sec.name()),
            ));
        }
        match sec {
            Section::Find => {
                for f in body {
                    q.find.push(find_item(f)?);
                }
            }
            Section::In => {
                for (i, f) in body.into_iter().enumerate() {
                    q.inputs.push(input(f, i == 0)?);
                }
            }
            Section::At => q.at = Some(tick_expr(single(sec, head, body)?)?),
            Section::Canon => q.canon = Some(canon_expr(single(sec, head, body)?)?),
            Section::Where => {
                saw_where = true;
                let mut patterns = 0usize;
                for f in body {
                    q.clauses.push(clause(f, &mut patterns)?);
                }
            }
        }
    }
    if q.find.is_empty() {
        return Err(DatalogError::syntax(qspan, "missing `:find` section"));
    }
    if !saw_where {
        return Err(DatalogError::syntax(qspan, "missing `:where` section"));
    }
    Ok(q)
}

/// The one form of a single-valued section (`:at`, `:canon`).
fn single(sec: Section, head: Span, body: Vec<Form>) -> Result<Form, DatalogError> {
    let mut it = body.into_iter();
    let first = it.next().ok_or_else(|| {
        DatalogError::syntax(head, format!("section `{}` needs a value", sec.name()))
    })?;
    if let Some(extra) = it.next() {
        return Err(DatalogError::syntax(
            extra.span(),
            format!("section `{}` takes exactly one value", sec.name()),
        ));
    }
    Ok(first)
}

fn find_item(f: Form) -> Result<Sym, DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Ok(Sym { name, span }),
        Form::Seq {
            bracket: Bracket::Round,
            span,
            ..
        } => Err(DatalogError::unsupported(
            span,
            format!("aggregates in `:find` are not supported yet ({SLICE7})"),
        )),
        other => Err(DatalogError::syntax(
            other.span(),
            "`:find` takes variables (`?name`)",
        )),
    }
}

fn input(f: Form, first: bool) -> Result<Input, DatalogError> {
    match f {
        Form::Atom { tok: Tok::Db, span } if first => Ok(Input::Db(span)),
        Form::Atom { tok: Tok::Db, span } => Err(DatalogError::syntax(
            span,
            "`$` may only be the first `:in` entry",
        )),
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Ok(Input::Var(Sym { name, span })),
        other => Err(DatalogError::syntax(
            other.span(),
            "`:in` takes `$` and parameters (`?name`)",
        )),
    }
}

fn int_i64(v: Option<i128>, span: Span) -> Result<i64, DatalogError> {
    v.and_then(|n| i64::try_from(n).ok()).ok_or_else(|| {
        DatalogError::syntax(
            span,
            "integer out of range (must fit a signed 64-bit integer)",
        )
    })
}

fn tick_expr(f: Form) -> Result<TickExpr, DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Int(v),
            span,
        } => Ok(TickExpr::Int(int_i64(v, span)?, span)),
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Ok(TickExpr::Var(Sym { name, span })),
        other => Err(DatalogError::syntax(
            other.span(),
            "a tick is an integer or a variable (`?t`)",
        )),
    }
}

fn canon_expr(f: Form) -> Result<CanonExpr, DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Int(v),
            span,
        } => match v {
            Some(n) if n < 0 => Err(DatalogError::syntax(
                span,
                "a canon (tx number) can't be negative",
            )),
            _ => v
                .and_then(|n| u64::try_from(n).ok())
                .map(|n| CanonExpr::Int(n, span))
                .ok_or_else(|| {
                    DatalogError::syntax(
                        span,
                        "canon out of range (must fit an unsigned 64-bit integer)",
                    )
                }),
        },
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Ok(CanonExpr::Var(Sym { name, span })),
        other => Err(DatalogError::syntax(
            other.span(),
            "a canon is a non-negative integer or a variable (`?c`)",
        )),
    }
}

/// Count one more pattern against [`MAX_PATTERNS`].
fn count_pattern(patterns: &mut usize, span: Span) -> Result<(), DatalogError> {
    *patterns += 1;
    if *patterns > MAX_PATTERNS {
        return Err(DatalogError::limit(
            Some(span),
            format!("more than {MAX_PATTERNS} patterns in one query"),
        ));
    }
    Ok(())
}

fn clause(f: Form, patterns: &mut usize) -> Result<Clause, DatalogError> {
    match f {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span,
        } => {
            if matches!(
                items.first(),
                Some(Form::Seq {
                    bracket: Bracket::Round,
                    ..
                })
            ) {
                return predicate(items, span).map(Clause::Pred);
            }
            count_pattern(patterns, span)?;
            pattern(items, span).map(Clause::Pattern)
        }
        Form::Seq {
            bracket: Bracket::Round,
            items,
            span,
        } => at_clause(items, span, patterns),
        Form::Atom { span, .. } => Err(DatalogError::syntax(
            span,
            "expected a clause: a pattern `[e a v]`, `(at T ..)` or a predicate `[(op x y)]`",
        )),
    }
}

fn at_clause(items: Vec<Form>, span: Span, patterns: &mut usize) -> Result<Clause, DatalogError> {
    let mut it = items.into_iter();
    match it.next() {
        Some(Form::Atom {
            tok: Tok::Sym(s), ..
        }) if s == "at" => {}
        Some(Form::Atom {
            tok: Tok::Sym(s),
            span: head,
        }) => {
            return Err(DatalogError::unsupported(
                head,
                format!(
                    "`({} ..)` clauses (rules, not, or) are not supported yet ({SLICE7})",
                    show(&s)
                ),
            ));
        }
        Some(other) => {
            return Err(DatalogError::syntax(
                other.span(),
                "expected `at` or a rule name here",
            ));
        }
        None => return Err(DatalogError::syntax(span, "empty list is not a clause")),
    }
    let tick = match it.next() {
        Some(f) => tick_expr(f)?,
        None => return Err(DatalogError::syntax(span, "`(at T ..)` needs a tick")),
    };
    let mut pats = Vec::new();
    for f in it {
        match f {
            Form::Seq {
                bracket: Bracket::Square,
                items,
                span: ps,
            } if !matches!(
                items.first(),
                Some(Form::Seq {
                    bracket: Bracket::Round,
                    ..
                })
            ) =>
            {
                count_pattern(patterns, ps)?;
                pats.push(pattern(items, ps)?);
            }
            other => {
                return Err(DatalogError::syntax(
                    other.span(),
                    "`(at T ..)` holds only patterns `[e a v]`",
                ));
            }
        }
    }
    if pats.is_empty() {
        return Err(DatalogError::syntax(
            span,
            "`(at T ..)` needs at least one pattern",
        ));
    }
    Ok(Clause::At {
        tick,
        patterns: pats,
        span,
    })
}

fn pattern(items: Vec<Form>, span: Span) -> Result<Pattern, DatalogError> {
    match items.len() {
        3 => {}
        4 | 5 => {
            return Err(DatalogError::unsupported(
                span,
                format!("patterns with a tick or tx position are not supported yet ({SLICE7})"),
            ));
        }
        _ => {
            return Err(DatalogError::syntax(
                span,
                "a pattern has 3 elements: `[e a v]`",
            ));
        }
    }
    let mut it = items.into_iter();
    let (Some(e), Some(a), Some(v)) = (it.next(), it.next(), it.next()) else {
        return Err(DatalogError::syntax(
            span,
            "a pattern has 3 elements: `[e a v]`",
        ));
    };
    let e = item(e)?;
    let a = match a {
        Form::Atom {
            tok: Tok::Keyword(name),
            span,
        } => Attr { name, span },
        other => {
            return Err(DatalogError::syntax(
                other.span(),
                "the attribute must be a keyword (`:name`)",
            ));
        }
    };
    let v = item(v)?;
    Ok(Pattern { e, a, v, span })
}

fn predicate(items: Vec<Form>, span: Span) -> Result<Pred, DatalogError> {
    let mut outer = items.into_iter();
    let Some(Form::Seq {
        items: inner,
        span: list_span,
        ..
    }) = outer.next()
    else {
        return Err(DatalogError::syntax(
            span,
            "expected a predicate `[(op x y)]`",
        ));
    };
    if let Some(extra) = outer.next() {
        return Err(DatalogError::syntax(
            extra.span(),
            "a predicate clause holds only the call: `[(op x y)]`",
        ));
    }
    let mut it = inner.into_iter();
    let op = match it.next() {
        Some(Form::Atom {
            tok: Tok::Sym(s),
            span: os,
        }) => match s.as_str() {
            "=" => CmpOp::Eq,
            "!=" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            "<=" => CmpOp::Le,
            ">" => CmpOp::Gt,
            ">=" => CmpOp::Ge,
            other => {
                return Err(DatalogError::syntax(
                    os,
                    format!(
                        "unknown predicate `{}` (expected = != < <= > >=)",
                        show(other)
                    ),
                ));
            }
        },
        Some(other) => {
            return Err(DatalogError::syntax(
                other.span(),
                "expected a comparison operator (= != < <= > >=)",
            ));
        }
        None => return Err(DatalogError::syntax(list_span, "empty predicate")),
    };
    let (Some(l), Some(r), None) = (it.next(), it.next(), it.next()) else {
        return Err(DatalogError::syntax(
            list_span,
            "a comparison takes exactly 2 operands",
        ));
    };
    Ok(Pred {
        op,
        left: item(l)?,
        right: item(r)?,
        span,
    })
}

fn item(f: Form) -> Result<Item, DatalogError> {
    let (tok, span) = match f {
        Form::Atom { tok, span } => (tok, span),
        Form::Seq { span, .. } => {
            return Err(DatalogError::syntax(
                span,
                "expected a variable, `_` or a constant",
            ));
        }
    };
    Ok(match tok {
        Tok::Var(name) => Item::Var(Sym { name, span }),
        Tok::Any => Item::Any(span),
        Tok::Keyword(k) => Item::Keyword(k, span),
        Tok::Id(s) => Item::Id(s, span),
        Tok::Str(s) => Item::Str(s, span),
        Tok::Int(v) => Item::Int(int_i64(v, span)?, span),
        Tok::Float(f) => Item::Float(f, span),
        Tok::Bool(b) => Item::Bool(b, span),
        Tok::Sym(s) => {
            return Err(DatalogError::syntax(
                span,
                format!("unknown symbol `{}` (strings need quotes)", show(&s)),
            ));
        }
        Tok::Db | Tok::Open(_) | Tok::Close(_) => {
            return Err(DatalogError::syntax(
                span,
                "expected a variable, `_` or a constant",
            ));
        }
    })
}
