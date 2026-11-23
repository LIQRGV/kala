//! Source-preserving defaults for independently parsed queries and rules.

use std::collections::HashSet;
use std::sync::Arc;

use kala_core::{Tick, TxId, World};

use crate::{
    DatalogError, MAX_CLAUSES, MAX_PATTERNS, MAX_QUERY_BYTES, MAX_RULES, Prepared, Span, ast,
};

/// Fallbacks applied to a query's AST before compilation.
#[derive(Clone, Debug, Default)]
pub struct SessionDefaults {
    /// Default story tick, used only when the query has no `:at`.
    pub at: Option<Tick>,
    /// Default author canon, used only when the query has no `:canon`.
    pub canon: Option<TxId>,
    /// Extra rule definitions; a query's own definitions replace names here.
    pub rules: Vec<SessionRule>,
}

/// A parsed rule together with the original source used to locate errors.
#[derive(Clone, Debug)]
pub struct SessionRule {
    rule: ast::Rule,
    source: Arc<str>,
}

impl SessionRule {
    /// The rule name, without a sigil.
    pub fn name(&self) -> &str {
        &self.rule.name
    }

    /// The individual definition's original text, including its brackets.
    pub fn source(&self) -> &str {
        &self.source[self.rule.span.start..self.rule.span.end]
    }
}

/// Parse a `:rules` vector and retain source locations for later compilation.
pub fn parse_rules(text: &str) -> Result<Vec<SessionRule>, DatalogError> {
    let rules = crate::parse::parse_rules(text)?;
    let source: Arc<str> = Arc::from(text);
    Ok(rules
        .into_iter()
        .map(|rule| SessionRule {
            rule,
            source: Arc::clone(&source),
        })
        .collect())
}

/// Prepare query text after applying AST defaults and merging extra rules.
///
/// Query definitions replace all extra definitions of the same name for this
/// preparation only. Errors in extra definitions name the rule and point into
/// the original text passed to [`parse_rules`]. Merged input remains bounded by
/// the normal query size, rule, clause and pattern limits.
pub fn prepare_with(
    world: &World,
    text: &str,
    defaults: &SessionDefaults,
) -> Result<Prepared, DatalogError> {
    let mut query = crate::parse(text)?;
    if query.at.is_none() {
        query.at = defaults.at.map(|t| ast::TickExpr::Int(t.0, query.span));
    }
    if query.canon.is_none() {
        query.canon = defaults.canon.map(|c| ast::CanonExpr::Int(c.0, query.span));
    }
    let names: HashSet<&str> = query.rules.iter().map(|r| r.name.as_str()).collect();
    // Only session rules the query can reach (through `:where`, its own rules
    // and other session rules, negations included) are merged, so a broken
    // session rule only fails the queries that use it.
    let mut reach: HashSet<String> = HashSet::new();
    let mut todo: Vec<String> = Vec::new();
    calls_of(&query.calls, &query.nots, &query.ors, &mut todo);
    for r in &query.rules {
        calls_of(&r.calls, &r.nots, &r.ors, &mut todo);
    }
    while let Some(name) = todo.pop() {
        if names.contains(name.as_str()) || !reach.insert(name.clone()) {
            continue;
        }
        for r in defaults.rules.iter().filter(|r| r.name() == name) {
            calls_of(&r.rule.calls, &r.rule.nots, &r.rule.ors, &mut todo);
        }
    }
    let extra: Vec<&SessionRule> = defaults
        .rules
        .iter()
        .filter(|r| !names.contains(r.name()) && reach.contains(r.name()))
        .collect();
    let mut bytes = text.len();
    for rule in &extra {
        bytes = bytes.saturating_add(rule.source().len());
        if bytes > MAX_QUERY_BYTES {
            return Err(DatalogError::limit(
                None,
                format!("query and session rules exceed {MAX_QUERY_BYTES} bytes"),
            ));
        }
    }
    if query.rules.len().saturating_add(extra.len()) > MAX_RULES {
        return Err(DatalogError::limit(
            None,
            format!("more than {MAX_RULES} rule definitions in one query"),
        ));
    }
    // Each definition gets a disjoint virtual source range. Only spans change;
    // text is never spliced and the AST's semantics remain untouched.
    let mut sources = Vec::new();
    let mut offset = text.len() + 1;
    for rule in extra {
        let mut ast = rule.rule.clone();
        shift_rule(&mut ast, offset);
        query.rules.push(ast);
        sources.push((offset, rule));
        offset += rule.source.len() + 1;
    }
    let compile = || {
        check_limits(&query)?;
        crate::compile::compile(world, &query)
    };
    compile().map_err(|mut error| {
        if let Some(span) = error.span {
            for &(offset, rule) in &sources {
                if span.start >= offset && span.end <= offset + rule.source.len() {
                    error.span = Some(Span::new(span.start - offset, span.end - offset));
                    error.message = format!(
                        "rule `{}`: {}",
                        crate::error::show(rule.name()),
                        error.message
                    );
                    return error.locate(&rule.source);
                }
            }
        }
        error.locate(text)
    })
}

fn shift_span(span: &mut Span, offset: usize) {
    span.start += offset;
    span.end += offset;
}

fn shift_item(item: &mut ast::Item, offset: usize) {
    match item {
        ast::Item::Var(s) => shift_span(&mut s.span, offset),
        ast::Item::Any(s)
        | ast::Item::Keyword(_, s)
        | ast::Item::Id(_, s)
        | ast::Item::Str(_, s)
        | ast::Item::Int(_, s)
        | ast::Item::Float(_, s)
        | ast::Item::Bool(_, s) => shift_span(s, offset),
    }
}

fn shift_pattern(p: &mut ast::Pattern, offset: usize) {
    shift_span(&mut p.span, offset);
    shift_span(&mut p.a.span, offset);
    shift_item(&mut p.e, offset);
    shift_item(&mut p.v, offset);
    if let Some(h) = &mut p.history {
        shift_item(&mut h.tick, offset);
        if let Some(p) = &mut h.precision {
            shift_item(p, offset);
        }
        if let Some(op) = &mut h.op {
            match op {
                ast::OpItem::Var(s) => shift_span(&mut s.span, offset),
                ast::OpItem::Any(s) | ast::OpItem::Assert(s) | ast::OpItem::Retract(s) => {
                    shift_span(s, offset)
                }
            }
        }
    }
}

fn shift_rule(r: &mut ast::Rule, offset: usize) {
    shift_span(&mut r.span, offset);
    shift_span(&mut r.name_span, offset);
    for s in &mut r.head {
        shift_span(&mut s.span, offset);
    }
    shift_body(
        &mut r.clauses,
        &mut r.calls,
        &mut r.nots,
        &mut r.ors,
        &mut r.durings,
        &mut r.fns,
        offset,
    );
}

fn shift_body(
    clauses: &mut [ast::Clause],
    calls: &mut [ast::Call],
    nots: &mut [ast::Not],
    ors: &mut [ast::Or],
    durings: &mut [ast::During],
    fns: &mut [ast::FnClause],
    offset: usize,
) {
    for clause in clauses {
        match clause {
            ast::Clause::Pattern(p) => shift_pattern(p, offset),
            ast::Clause::At {
                tick,
                patterns,
                span,
            } => {
                shift_span(span, offset);
                match tick {
                    ast::TickExpr::Int(_, s) => shift_span(s, offset),
                    ast::TickExpr::Var(s) => shift_span(&mut s.span, offset),
                }
                for p in patterns {
                    shift_pattern(p, offset);
                }
            }
            ast::Clause::Pred(p) => {
                shift_span(&mut p.span, offset);
                shift_item(&mut p.left, offset);
                shift_item(&mut p.right, offset);
            }
        }
    }
    for c in calls {
        shift_span(&mut c.span, offset);
        shift_span(&mut c.name_span, offset);
        for a in &mut c.args {
            shift_item(a, offset);
        }
    }
    for d in durings {
        shift_span(&mut d.span, offset);
        shift_span(&mut d.out.span, offset);
        shift_pattern(&mut d.pattern, offset);
    }
    for f in fns {
        shift_span(&mut f.span, offset);
        shift_span(&mut f.name_span, offset);
        for a in &mut f.args {
            shift_item(a, offset);
        }
        if let Some(s) = &mut f.out {
            shift_span(&mut s.span, offset);
        }
    }
    for o in ors {
        shift_span(&mut o.span, offset);
        for b in &mut o.branches {
            shift_span(&mut b.span, offset);
            shift_body(
                &mut b.clauses,
                &mut b.calls,
                &mut b.nots,
                &mut b.ors,
                &mut b.durings,
                &mut b.fns,
                offset,
            );
        }
    }
    for n in nots {
        shift_span(&mut n.span, offset);
        shift_body(
            &mut n.clauses,
            &mut n.calls,
            &mut n.nots,
            &mut n.ors,
            &mut n.durings,
            &mut n.fns,
            offset,
        );
    }
}

fn check_limits(q: &ast::Query) -> Result<(), DatalogError> {
    let mut bodies = vec![(&q.clauses, &q.calls, &q.nots, &q.ors, &q.durings, &q.fns)];
    for r in &q.rules {
        bodies.push((&r.clauses, &r.calls, &r.nots, &r.ors, &r.durings, &r.fns));
    }
    let mut clauses = 0;
    let mut patterns = 0;
    while let Some((cs, calls, nots, ors, durings, fns)) = bodies.pop() {
        clauses += cs.len() + calls.len() + nots.len() + ors.len() + 2 * durings.len() + fns.len();
        patterns += durings.len();
        for c in cs {
            match c {
                ast::Clause::Pattern(_) => patterns += 1,
                ast::Clause::At { patterns: ps, .. } => {
                    patterns += ps.len();
                    clauses += ps.len();
                }
                ast::Clause::Pred(_) => {}
            }
        }
        if clauses > MAX_CLAUSES {
            return Err(DatalogError::limit(
                None,
                format!("more than {MAX_CLAUSES} clauses in one query"),
            ));
        }
        if patterns > MAX_PATTERNS {
            return Err(DatalogError::limit(
                None,
                format!("more than {MAX_PATTERNS} patterns in one query"),
            ));
        }
        for o in ors {
            for b in &o.branches {
                clauses += 1;
                bodies.push((&b.clauses, &b.calls, &b.nots, &b.ors, &b.durings, &b.fns));
            }
        }
        for n in nots {
            bodies.push((&n.clauses, &n.calls, &n.nots, &n.ors, &n.durings, &n.fns));
        }
    }
    Ok(())
}

/// Pushes the names of the rules called by `calls` and, recursively, by the
/// negations `nots`.
fn calls_of(calls: &[ast::Call], nots: &[ast::Not], ors: &[ast::Or], out: &mut Vec<String>) {
    out.extend(calls.iter().map(|c| c.name.clone()));
    for o in ors {
        for b in &o.branches {
            calls_of(&b.calls, &b.nots, &b.ors, out);
        }
    }
    for n in nots {
        calls_of(&n.calls, &n.nots, &n.ors, out);
    }
}
