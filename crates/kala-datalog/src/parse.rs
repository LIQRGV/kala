//! Parser: tokens → a small EDN tree (built with an explicit stack, depth
//! limited) → the [`ast::Query`].
//!
//! The tree is at most [`MAX_DEPTH`] deep by construction, so everything
//! after the tree builder may walk it without worrying about the stack.

use crate::ast::{
    AggFn, Attr, Call, CanonExpr, Clause, CmpOp, During, Find, FindAgg, FnClause, Func, HistoryPos,
    Input, Item, Not, OpItem, Or, Pattern, Pred, Query, Rule, Sym, TickExpr,
};
use crate::error::{DatalogError, Span, show};
use crate::lex::{Bracket, Lexer, Tok};
use crate::{MAX_CLAUSES, MAX_DEPTH, MAX_PATTERNS, MAX_QUERY_BYTES, MAX_RULES};

/// Names a rule can't have: clause forms, built-in functions, aggregates
/// and comparison operators (present or planned).
const RESERVED: [&str; 21] = [
    "at",
    "not",
    "or",
    "and",
    "during",
    "intersect",
    "union",
    "difference",
    "not-empty",
    "contains",
    "count",
    "count-distinct",
    "min",
    "max",
    "sum",
    "=",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
];

/// Clause forms `(name ..)` planned for slice 7 (`not-join`, disjunction):
/// still refused as unsupported, not as rule calls.
const PLANNED_FORMS: [&str; 2] = ["not-join", "or-join"];

/// Message tail for grammar that is planned but not built yet.
const SLICE7: &str = "planned for slice 7";

/// A read EDN form.
#[derive(Clone, Debug)]
pub(crate) enum Form {
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
    pub(crate) fn span(&self) -> Span {
        match self {
            Form::Seq { span, .. } | Form::Atom { span, .. } => *span,
        }
    }
}

/// Parse a Datalog query (see the crate docs for the grammar).
pub(crate) fn parse(text: &str) -> Result<Query, DatalogError> {
    parse_unlocated(text).map_err(|e| e.locate(text))
}

/// Parse the same vector accepted after `:rules`, without a query wrapper.
pub(crate) fn parse_rules(text: &str) -> Result<Vec<Rule>, DatalogError> {
    let parse = || {
        if text.len() > MAX_QUERY_BYTES {
            return Err(DatalogError::limit(
                None,
                format!(
                    "rules are {} bytes; the limit is {MAX_QUERY_BYTES}",
                    text.len()
                ),
            ));
        }
        let mut lex = Lexer::new(text);
        let form = read_form(&mut lex, text.len())?;
        if let Some(t) = lex.next_token()? {
            return Err(DatalogError::syntax(
                t.span,
                "unexpected input after the rules",
            ));
        }
        rules(form, &mut Counts::default())
    };
    parse().map_err(|e| e.locate(text))
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
pub(crate) fn read_form(lex: &mut Lexer<'_>, len: usize) -> Result<Form, DatalogError> {
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
    Rules,
    Where,
}

impl Section {
    /// The section a `:keyword` header names.
    fn named(keyword: &str) -> Option<Section> {
        Some(match keyword {
            "find" => Section::Find,
            "in" => Section::In,
            "at" => Section::At,
            "canon" => Section::Canon,
            "where" => Section::Where,
            "rules" => Section::Rules,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Section::Find => ":find",
            Section::In => ":in",
            Section::At => ":at",
            Section::Canon => ":canon",
            Section::Rules => ":rules",
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
    let mut q = Query {
        find: Vec::new(),
        inputs: Vec::new(),
        at: None,
        canon: None,
        rules: Vec::new(),
        clauses: Vec::new(),
        calls: Vec::new(),
        nots: Vec::new(),
        ors: Vec::new(),
        durings: Vec::new(),
        fns: Vec::new(),
        span: qspan,
    };
    let mut saw_where = false;
    // Shared by rule bodies and `:where`: the limits are per query.
    let mut counts = Counts::default();
    for (sec, head, body) in sections(items)? {
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
            Section::Rules => q.rules = rules(single(sec, head, body)?, &mut counts)?,
            Section::Where => {
                saw_where = true;
                let mut b = Body::default();
                for f in body {
                    clause(f, &mut counts, &mut b)?;
                }
                q.clauses = b.clauses;
                q.calls = b.calls;
                q.nots = b.nots;
                q.ors = b.ors;
                q.durings = b.durings;
                q.fns = b.fns;
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

/// Splits a query's forms into its sections, each (section, header span,
/// body forms), checking the headers' names and order.
fn sections(items: Vec<Form>) -> Result<Vec<(Section, Span, Vec<Form>)>, DatalogError> {
    let mut sections: Vec<(Section, Span, Vec<Form>)> = Vec::new();
    for f in items {
        if let Form::Atom {
            tok: Tok::Keyword(k),
            span,
        } = &f
        {
            let Some(sec) = Section::named(k) else {
                return Err(DatalogError::syntax(
                    *span,
                    format!(
                        "unknown section `:{}` (expected :find, :in, :at, :canon, :rules or :where)",
                        show(k)
                    ),
                ));
            };
            check_section_order(sections.last().map(|(prev, _, _)| *prev), sec, *span)?;
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
    Ok(sections)
}

/// Checks that section `sec` (its header at `span`) may follow `prev`: the
/// first is `:find`, and each comes once, in the required order.
fn check_section_order(
    prev: Option<Section>,
    sec: Section,
    span: Span,
) -> Result<(), DatalogError> {
    let Some(prev) = prev else {
        if sec != Section::Find {
            return Err(DatalogError::syntax(span, "a query starts with `:find`"));
        }
        return Ok(());
    };
    if prev == sec {
        return Err(DatalogError::syntax(
            span,
            format!("section `{}` appears twice", sec.name()),
        ));
    }
    if prev > sec {
        return Err(DatalogError::syntax(
            span,
            format!(
                "section `{}` must come before `{}` (order: :find :in :at :canon :rules :where)",
                sec.name(),
                prev.name()
            ),
        ));
    }
    Ok(())
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

/// One `:find` element: `?name`, or an aggregate `(func ?name)` with
/// `func` one of `count`, `count-distinct`, `min`, `max`, `sum`.
fn find_item(f: Form) -> Result<Find, DatalogError> {
    match f {
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Ok(Find {
            name,
            span,
            agg: None,
        }),
        Form::Seq {
            bracket: Bracket::Round,
            items,
            span,
        } => find_aggregate(items, span),
        other => Err(DatalogError::syntax(
            other.span(),
            "`:find` takes variables (`?name`) and aggregates (`(count ?name)`)",
        )),
    }
}

/// An aggregate `:find` element `(func ?name)`.
fn find_aggregate(items: Vec<Form>, span: Span) -> Result<Find, DatalogError> {
    const SHAPE: &str = "an aggregate is `(count ?x)`, `(count-distinct ?x)`, `(min ?x)`, \
                         `(max ?x)` or `(sum ?x)`";
    let mut it = items.into_iter();
    let func = match it.next() {
        Some(Form::Atom {
            tok: Tok::Sym(name),
            span: name_span,
        }) => agg_fn(&name).ok_or_else(|| {
            DatalogError::syntax(
                name_span,
                format!("`{}` is not an aggregate; {SHAPE}", show(&name)),
            )
        })?,
        Some(other) => return Err(DatalogError::syntax(other.span(), SHAPE)),
        None => return Err(DatalogError::syntax(span, SHAPE)),
    };
    let var = match (it.next(), it.next()) {
        (
            Some(Form::Atom {
                tok: Tok::Var(name),
                span,
            }),
            None,
        ) => Sym { name, span },
        (_, Some(extra)) => {
            return Err(DatalogError::syntax(
                extra.span(),
                format!("`{}` takes exactly one variable", func.name()),
            ));
        }
        (Some(other), None) => {
            return Err(DatalogError::syntax(
                other.span(),
                format!("`{}` takes a variable (`?name`)", func.name()),
            ));
        }
        (None, None) => {
            return Err(DatalogError::syntax(
                span,
                format!("`{}` takes a variable (`?name`)", func.name()),
            ));
        }
    };
    Ok(Find {
        name: format!("({} ?{})", func.name(), var.name),
        span,
        agg: Some(FindAgg { func, var }),
    })
}

/// The aggregate function named `name`.
fn agg_fn(name: &str) -> Option<AggFn> {
    Some(match name {
        "count" => AggFn::Count,
        "count-distinct" => AggFn::CountDistinct,
        "min" => AggFn::Min,
        "max" => AggFn::Max,
        "sum" => AggFn::Sum,
        _ => return None,
    })
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

/// The clauses of one query counted so far, against [`MAX_PATTERNS`] and
/// [`MAX_CLAUSES`].
#[derive(Default)]
struct Counts {
    patterns: usize,
    clauses: usize,
}

impl Counts {
    /// Count one more clause of any kind (a pattern, a predicate, an
    /// `(at ..)` wrapper) against [`MAX_CLAUSES`].
    fn clause(&mut self, span: Span) -> Result<(), DatalogError> {
        self.clauses += 1;
        if self.clauses > MAX_CLAUSES {
            return Err(DatalogError::limit(
                Some(span),
                format!("more than {MAX_CLAUSES} clauses in one query"),
            ));
        }
        Ok(())
    }

    /// Count one more pattern, against [`MAX_PATTERNS`] and as a clause.
    fn pattern(&mut self, span: Span) -> Result<(), DatalogError> {
        self.patterns += 1;
        if self.patterns > MAX_PATTERNS {
            return Err(DatalogError::limit(
                Some(span),
                format!("more than {MAX_PATTERNS} patterns in one query"),
            ));
        }
        self.clause(span)
    }
}

/// The clauses of one body (`:where`, a rule's or a `not`'s), split by
/// kind.
#[derive(Default)]
struct Body {
    clauses: Vec<Clause>,
    calls: Vec<Call>,
    nots: Vec<Not>,
    ors: Vec<Or>,
    durings: Vec<During>,
    fns: Vec<FnClause>,
}

impl Body {
    fn is_empty(&self) -> bool {
        self.clauses.is_empty()
            && self.calls.is_empty()
            && self.nots.is_empty()
            && self.ors.is_empty()
            && self.durings.is_empty()
            && self.fns.is_empty()
    }

    /// These clauses as one conjunction: a `not`'s body or an `or` branch.
    fn into_conjunction(self, span: Span) -> Not {
        Not {
            clauses: self.clauses,
            calls: self.calls,
            nots: self.nots,
            ors: self.ors,
            durings: self.durings,
            fns: self.fns,
            span,
        }
    }
}

/// The `:rules` section's one form: a vector of definitions
/// `[(name ?v ..) clause+]`, at most [`MAX_RULES`] of them.
fn rules(f: Form, counts: &mut Counts) -> Result<Vec<Rule>, DatalogError> {
    let (items, span) = match f {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span,
        } => (items, span),
        other => {
            return Err(DatalogError::syntax(
                other.span(),
                "`:rules` takes one vector of definitions: `[[(name ?v ..) clause ..] ..]`",
            ));
        }
    };
    if items.is_empty() {
        return Err(DatalogError::syntax(
            span,
            "`:rules` needs at least one definition",
        ));
    }
    let mut out = Vec::with_capacity(items.len().min(MAX_RULES));
    for f in items {
        if out.len() == MAX_RULES {
            return Err(DatalogError::limit(
                Some(f.span()),
                format!("more than {MAX_RULES} rule definitions in one query"),
            ));
        }
        out.push(rule(f, counts)?);
    }
    Ok(out)
}

/// One definition `[(name ?v ..) clause+]`.
fn rule(f: Form, counts: &mut Counts) -> Result<Rule, DatalogError> {
    const SHAPE: &str = "a rule definition is a vector `[(name ?v ..) clause ..]`";
    let (items, span) = match f {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span,
        } => (items, span),
        other => return Err(DatalogError::syntax(other.span(), SHAPE)),
    };
    let mut it = items.into_iter();
    let (head_items, head_span) = match it.next() {
        Some(Form::Seq {
            bracket: Bracket::Round,
            items,
            span,
        }) => (items, span),
        Some(other) => return Err(DatalogError::syntax(other.span(), SHAPE)),
        None => return Err(DatalogError::syntax(span, SHAPE)),
    };
    let mut hi = head_items.into_iter();
    let (name, name_span) = rule_name(hi.next(), head_span)?;
    let head = rule_head_vars(hi, head_span)?;
    let mut b = Body::default();
    for f in it {
        clause(f, counts, &mut b)?;
    }
    if b.is_empty() {
        return Err(DatalogError::syntax(
            span,
            "a rule definition needs at least one body clause",
        ));
    }
    Ok(Rule {
        name,
        name_span,
        head,
        clauses: b.clauses,
        calls: b.calls,
        nots: b.nots,
        ors: b.ors,
        durings: b.durings,
        fns: b.fns,
        span,
    })
}

/// The name of a rule head `(name ?v ..)`: its first form, a bare symbol
/// that isn't reserved.
fn rule_name(first: Option<Form>, head_span: Span) -> Result<(String, Span), DatalogError> {
    match first {
        Some(Form::Atom {
            tok: Tok::Sym(s),
            span,
        }) => {
            if RESERVED.contains(&s.as_str()) {
                return Err(DatalogError::syntax(
                    span,
                    format!("`{}` is reserved and can't name a rule", show(&s)),
                ));
            }
            Ok((s, span))
        }
        Some(other) => Err(DatalogError::syntax(
            other.span(),
            "a rule name is a bare symbol (`ancestor`)",
        )),
        None => Err(DatalogError::syntax(
            head_span,
            "a rule head is `(name ?v ..)`",
        )),
    }
}

/// The variables of a rule head, after its name: at least one, each once.
fn rule_head_vars(
    forms: impl Iterator<Item = Form>,
    head_span: Span,
) -> Result<Vec<Sym>, DatalogError> {
    let mut head: Vec<Sym> = Vec::new();
    for f in forms {
        match f {
            Form::Atom {
                tok: Tok::Var(v),
                span,
            } => {
                if head.iter().any(|h| h.name == v) {
                    return Err(DatalogError::syntax(
                        span,
                        format!("?{} appears twice in the rule head", show(&v)),
                    ));
                }
                head.push(Sym { name: v, span });
            }
            other => {
                return Err(DatalogError::syntax(
                    other.span(),
                    "a rule head holds only variables (`?name`)",
                ));
            }
        }
    }
    if head.is_empty() {
        return Err(DatalogError::syntax(
            head_span,
            "a rule head needs at least one variable",
        ));
    }
    Ok(head)
}

/// Parses one clause into `body`. Recursion only follows `(not ..)`
/// nesting, which the tree builder bounded by [`MAX_DEPTH`].
fn clause(f: Form, counts: &mut Counts, body: &mut Body) -> Result<(), DatalogError> {
    match f {
        Form::Seq {
            bracket: Bracket::Round,
            items,
            span,
        } => {
            counts.clause(span)?;
            let head = match items.first() {
                Some(Form::Atom {
                    tok: Tok::Sym(s), ..
                }) => Some(s.as_str()),
                _ => None,
            };
            if head == Some("or") {
                body.ors.push(or(items, span, counts)?);
            } else if head == Some("and") {
                return Err(DatalogError::syntax(
                    span,
                    "and is only allowed as an or branch",
                ));
            } else if head == Some("not") {
                body.nots.push(not(items, span, counts)?);
            } else if head == Some("during") {
                body.durings.push(during(items, span, counts)?);
            } else if head.is_some_and(|s| s != "at") {
                body.calls.push(call(items, span)?);
            } else {
                body.clauses.push(at_clause(items, span, counts)?);
            }
            Ok(())
        }
        f => simple_clause(f, counts, body),
    }
}

/// A disjunction `(or branch+)`: each branch is one clause, or several
/// grouped as `(and clause+)`.
fn or(items: Vec<Form>, span: Span, counts: &mut Counts) -> Result<Or, DatalogError> {
    let mut branches = Vec::new();
    // The first item is the `or` symbol.
    for f in items.into_iter().skip(1) {
        let branch = match f {
            Form::Seq {
                bracket: Bracket::Round,
                items,
                span,
            } if matches!(items.first(), Some(Form::Atom { tok: Tok::Sym(s), .. }) if s == "and") =>
            {
                counts.clause(span)?;
                not(items, span, counts)?
            }
            f => {
                let span = f.span();
                let mut b = Body::default();
                clause(f, counts, &mut b)?;
                b.into_conjunction(span)
            }
        };
        branches.push(branch);
    }
    if branches.is_empty() {
        return Err(DatalogError::syntax(span, "or needs at least one branch"));
    }
    Ok(Or { branches, span })
}

/// A negation `(not clause+)`: at least one clause, of any kind (nested
/// `not`s included).
fn not(items: Vec<Form>, span: Span, counts: &mut Counts) -> Result<Not, DatalogError> {
    let mut b = Body::default();
    // The first item is the `not` symbol.
    for f in items.into_iter().skip(1) {
        clause(f, counts, &mut b)?;
    }
    if b.is_empty() {
        return Err(DatalogError::syntax(
            span,
            "a conjunction needs at least one clause",
        ));
    }
    Ok(b.into_conjunction(span))
}

/// `(during [e a v] ?s)`, exactly: a 3-element pattern (not `:kala_handle`) and
/// a variable.
fn during(items: Vec<Form>, span: Span, counts: &mut Counts) -> Result<During, DatalogError> {
    const SHAPE: &str = "`during` is written `(during [e a v] ?s)`: a 3-element pattern and a \
                         variable for the interval set";
    let mut it = items.into_iter().skip(1); // the `during` symbol
    let (pat, out) = match (it.next(), it.next(), it.next()) {
        (Some(p), Some(o), None) => (p, o),
        _ => return Err(DatalogError::syntax(span, SHAPE)),
    };
    let pattern = match pat {
        Form::Seq {
            bracket: Bracket::Square,
            items,
            span: ps,
        } if items.len() == 3
            && !matches!(
                items.first(),
                Some(Form::Seq {
                    bracket: Bracket::Round,
                    ..
                })
            ) =>
        {
            counts.pattern(ps)?;
            pattern(items, ps, false)?
        }
        other => return Err(DatalogError::syntax(other.span(), SHAPE)),
    };
    if pattern.a.name == "kala_handle" {
        return Err(DatalogError::syntax(
            pattern.a.span,
            "`:kala_handle` has no history; `during` needs a schema attribute",
        ));
    }
    let out = match out {
        Form::Atom {
            tok: Tok::Var(name),
            span,
        } => Sym { name, span },
        other => return Err(DatalogError::syntax(other.span(), SHAPE)),
    };
    Ok(During { pattern, out, span })
}

/// A rule call `(name arg ..)`; `name` is a symbol other than `at` and `not`.
fn call(items: Vec<Form>, span: Span) -> Result<Call, DatalogError> {
    let mut it = items.into_iter();
    let Some(Form::Atom {
        tok: Tok::Sym(name),
        span: name_span,
    }) = it.next()
    else {
        unreachable!("invariant: clause() routes only `(symbol ..)` lists here")
    };
    if PLANNED_FORMS.contains(&name.as_str()) {
        return Err(DatalogError::unsupported(
            name_span,
            format!(
                "`({} ..)` clauses are not supported yet ({SLICE7})",
                show(&name)
            ),
        ));
    }
    if RESERVED.contains(&name.as_str()) {
        return Err(DatalogError::syntax(
            name_span,
            format!(
                "`{}` is built in, not a rule; comparisons are written `[(op x y)]`",
                show(&name)
            ),
        ));
    }
    let mut args = Vec::new();
    for f in it {
        args.push(item(f)?);
    }
    Ok(Call {
        name,
        name_span,
        args,
        span,
    })
}

/// A pattern, a predicate or an atom (an error): every clause but a list.
fn simple_clause(f: Form, counts: &mut Counts, body: &mut Body) -> Result<(), DatalogError> {
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
                counts.clause(span)?;
                match predicate(items, span)? {
                    Bracketed::Pred(p) => body.clauses.push(Clause::Pred(p)),
                    Bracketed::Fn(f) => body.fns.push(f),
                }
                return Ok(());
            }
            counts.pattern(span)?;
            body.clauses
                .push(pattern(items, span, true).map(Clause::Pattern)?);
            Ok(())
        }
        Form::Seq { span, .. } | Form::Atom { span, .. } => Err(DatalogError::syntax(
            span,
            "expected a clause: a pattern `[e a v]`, `(at T ..)`, a rule call `(name ..)` \
             or a predicate `[(op x y)]`",
        )),
    }
}

fn at_clause(items: Vec<Form>, span: Span, counts: &mut Counts) -> Result<Clause, DatalogError> {
    let mut it = items.into_iter();
    match it.next() {
        Some(Form::Atom {
            tok: Tok::Sym(s), ..
        }) if s == "at" => {}
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
                counts.pattern(ps)?;
                pats.push(pattern(items, ps, false)?);
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

/// A pattern `[e a v]`, or (`history_ok`) a history pattern `[e a v t]` /
/// `[e a v t op]`. Inside `(at ..)` a history pattern is refused: it reads
/// every tick, so a tick of its own has no meaning.
fn pattern(items: Vec<Form>, span: Span, history_ok: bool) -> Result<Pattern, DatalogError> {
    const SHAPE: &str = "a pattern has 3 elements `[e a v]`, or 4 to 6 for change points \
                         `[e a v t]` / `[e a v t op]`";
    match items.len() {
        3 => {}
        4..=6 if !history_ok => {
            return Err(DatalogError::syntax(
                span,
                "a history pattern `[e a v t ..]` can't be inside `(at ..)`: history patterns \
                 read every tick",
            ));
        }
        4..=6 => {}
        _ => return Err(DatalogError::syntax(span, SHAPE)),
    }
    let mut it = items.into_iter();
    let (Some(e), Some(a), Some(v)) = (it.next(), it.next(), it.next()) else {
        return Err(DatalogError::syntax(span, SHAPE));
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
    let history = match it.next() {
        None => None,
        Some(t) => Some(history_pos(&a, t, it)?),
    };
    Ok(Pattern {
        e,
        a,
        v,
        history,
        span,
    })
}

/// The change-point positions of a history pattern over attribute `a`: the
/// tick `t`, then an optional op and an optional precision from `rest`.
fn history_pos(
    a: &Attr,
    t: Form,
    mut rest: impl Iterator<Item = Form>,
) -> Result<HistoryPos, DatalogError> {
    if a.name == "kala_handle" {
        return Err(DatalogError::syntax(
            a.span,
            "`:kala_handle` has no history; a history pattern needs a schema attribute",
        ));
    }
    let tick = match item(t)? {
        t @ (Item::Var(_) | Item::Int(..) | Item::Any(_)) => t,
        other => {
            return Err(DatalogError::syntax(
                other.span(),
                "the tick position of a history pattern is a variable, an integer or `_`",
            ));
        }
    };
    let op = rest.next().map(op_item).transpose()?;
    let precision = rest.next().map(item).transpose()?;
    if let Some(p) = &precision {
        let valid = match p {
            Item::Var(_) | Item::Any(_) => true,
            Item::Keyword(k, _) => crate::write::precision_name(k).is_some(),
            _ => false,
        };
        if !valid {
            return Err(DatalogError::syntax(
                p.span(),
                "precision position needs a variable, wildcard or precision keyword",
            ));
        }
    }
    Ok(HistoryPos {
        tick,
        op,
        precision,
    })
}

/// The op position of a history pattern: `?op`, `_`, `:assert` or
/// `:retract`.
fn op_item(f: Form) -> Result<OpItem, DatalogError> {
    const OP: &str = "the op position of a history pattern is a variable, `_`, `:assert` or \
                      `:retract`";
    match item(f)? {
        Item::Var(s) => Ok(OpItem::Var(s)),
        Item::Any(span) => Ok(OpItem::Any(span)),
        Item::Keyword(k, span) if k == "assert" => Ok(OpItem::Assert(span)),
        Item::Keyword(k, span) if k == "retract" => Ok(OpItem::Retract(span)),
        other => Err(DatalogError::syntax(other.span(), OP)),
    }
}

/// What a `[(..) ..]` clause is.
enum Bracketed {
    /// A comparison `[(op x y)]`.
    Pred(Pred),
    /// An interval-set function clause.
    Fn(FnClause),
}

/// A predicate `[(op x y)]` or an interval-set function clause
/// `[(f args..) ?out]` / `[(f args..)]`.
fn predicate(items: Vec<Form>, span: Span) -> Result<Bracketed, DatalogError> {
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
    let out = outer.next();
    if let Some(extra) = outer.next() {
        return Err(DatalogError::syntax(
            extra.span(),
            "a function clause holds the call and at most one output: `[(f x y) ?out]`",
        ));
    }
    let mut args = inner.into_iter();
    let (name, name_span) = match args.next() {
        Some(Form::Atom {
            tok: Tok::Sym(s),
            span: os,
        }) => (s, os),
        Some(other) => {
            return Err(DatalogError::syntax(
                other.span(),
                "expected a comparison operator (= != < <= > >=) or a function",
            ));
        }
        None => return Err(DatalogError::syntax(list_span, "empty predicate")),
    };
    let call = Applied {
        name_span,
        args,
        out,
        list_span,
        span,
    };
    if let Some(func) = func_named(&name) {
        return fn_clause(func, call);
    }
    let Some(op) = cmp_op_named(&name) else {
        return Err(DatalogError::syntax(
            name_span,
            format!(
                "unknown predicate or function `{}` (expected = != < <= > >=, intersect, \
                 union, difference, not-empty or contains)",
                show(&name)
            ),
        ));
    };
    comparison(op, call)
}

/// A `[(name args..) out]` clause after its name: what a comparison and a
/// function clause are both read from.
struct Applied {
    name_span: Span,
    /// The forms after the name inside the list.
    args: std::vec::IntoIter<Form>,
    /// The form after the list, if any.
    out: Option<Form>,
    list_span: Span,
    /// The whole clause.
    span: Span,
}

/// The interval-set function named `name`.
fn func_named(name: &str) -> Option<Func> {
    Some(match name {
        "intersect" => Func::Intersect,
        "union" => Func::Union,
        "difference" => Func::Difference,
        "not-empty" => Func::NotEmpty,
        "contains" => Func::Contains,
        _ => return None,
    })
}

/// The comparison operator written `name`.
fn cmp_op_named(name: &str) -> Option<CmpOp> {
    Some(match name {
        "=" => CmpOp::Eq,
        "!=" => CmpOp::Ne,
        "<" => CmpOp::Lt,
        "<=" => CmpOp::Le,
        ">" => CmpOp::Gt,
        ">=" => CmpOp::Ge,
        _ => return None,
    })
}

/// The rest of a comparison `[(op x y)]`: exactly two operands, no output.
fn comparison(op: CmpOp, call: Applied) -> Result<Bracketed, DatalogError> {
    let Applied {
        mut args,
        out,
        list_span,
        span,
        ..
    } = call;
    if let Some(extra) = out {
        return Err(DatalogError::syntax(
            extra.span(),
            "a predicate clause holds only the call: `[(op x y)]`",
        ));
    }
    let (Some(l), Some(r), None) = (args.next(), args.next(), args.next()) else {
        return Err(DatalogError::syntax(
            list_span,
            "a comparison takes exactly 2 operands",
        ));
    };
    Ok(Bracketed::Pred(Pred {
        op,
        left: item(l)?,
        right: item(r)?,
        span,
    }))
}

/// The rest of an interval-set function clause: its arguments (2 for
/// `intersect`, `union`, `difference` and `contains`, 1 for `not-empty`)
/// and its output (a variable, for the set functions only).
fn fn_clause(func: Func, call: Applied) -> Result<Bracketed, DatalogError> {
    let Applied {
        name_span,
        args,
        out,
        list_span,
        span,
    } = call;
    let (arity, has_out) = match func {
        Func::Intersect | Func::Union | Func::Difference => (2, true),
        Func::NotEmpty => (1, false),
        Func::Contains => (2, false),
    };
    let args = args.map(item).collect::<Result<Vec<_>, _>>()?;
    if args.len() != arity {
        return Err(DatalogError::syntax(
            list_span,
            format!(
                "`{}` takes exactly {arity} argument{}",
                func.name(),
                if arity == 1 { "" } else { "s" }
            ),
        ));
    }
    let out = match (out, has_out) {
        (
            Some(Form::Atom {
                tok: Tok::Var(name),
                span,
            }),
            true,
        ) => Some(Sym { name, span }),
        (Some(other), true) => {
            return Err(DatalogError::syntax(
                other.span(),
                format!(
                    "the output of `{}` is a variable: `[({} ?a ?b) ?out]`",
                    func.name(),
                    func.name()
                ),
            ));
        }
        (None, true) => {
            return Err(DatalogError::syntax(
                span,
                format!(
                    "`{}` binds its result to a variable: `[({} ?a ?b) ?out]`",
                    func.name(),
                    func.name()
                ),
            ));
        }
        (Some(other), false) => {
            return Err(DatalogError::syntax(
                other.span(),
                format!("`{}` is a filter and has no output", func.name()),
            ));
        }
        (None, false) => None,
    };
    Ok(Bracketed::Fn(FnClause {
        func,
        name_span,
        args,
        out,
        span,
    }))
}

pub(crate) fn item(f: Form) -> Result<Item, DatalogError> {
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
