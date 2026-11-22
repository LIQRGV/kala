//! The story-check file of `kala doctor --checks FILE`: one top-level
//! vector of `[:name [:find ...]]` checks, read with the query lexer.
//!
//! The queries are not parsed or compiled here (compiling needs a world);
//! each is only cut out of the text as its exact source slice, with its
//! bracket nesting bounded by [`MAX_DEPTH`] and its size by
//! [`MAX_QUERY_BYTES`]. A flat token loop with an explicit bracket stack:
//! nothing recurses, so deep input can't overflow the call stack.

use std::collections::BTreeSet;

use crate::error::{DatalogError, Span, show};
use crate::lex::{Bracket, Lexer, Tok, Token};
use crate::{MAX_CHECKS, MAX_CHECKS_BYTES, MAX_DEPTH, MAX_QUERY_BYTES};

/// One named story check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The check's name, without the leading `:`.
    pub name: String,
    /// The query vector's exact source text, comments and spacing kept.
    pub query: String,
    /// The whole check `[:name [...]]` in the checks text.
    pub span: Span,
    /// Where [`Check::query`] sits in the checks text. An error from
    /// compiling the query is located in the file with
    /// `err.offset_by(check.query_span.start).locate(file_text)`.
    pub query_span: Span,
}

pub(crate) fn parse_checks(text: &str) -> Result<Vec<Check>, DatalogError> {
    parse_unlocated(text).map_err(|e| e.locate(text))
}

const SHAPE: &str = "a check is `[:name [:find ...]]`";

/// What a token is, for messages.
fn what(t: &Tok) -> String {
    match t {
        Tok::Open(b) => format!("`{}`", b.open()),
        Tok::Close(b) => format!("`{}`", b.close()),
        Tok::Keyword(k) => format!("keyword `:{}`", show(k)),
        Tok::Var(v) => format!("variable `?{}`", show(v)),
        Tok::Str(_) => "a string".to_string(),
        Tok::Int(_) | Tok::Float(_) => "a number".to_string(),
        Tok::Bool(_) => "a boolean".to_string(),
        Tok::Id(_) => "an ID".to_string(),
        Tok::Any | Tok::Db | Tok::Sym(_) => "a symbol".to_string(),
    }
}

fn parse_unlocated(text: &str) -> Result<Vec<Check>, DatalogError> {
    // Size first: nothing else looks at an oversized input.
    if text.len() > MAX_CHECKS_BYTES {
        return Err(DatalogError::limit(
            None,
            format!(
                "the checks file is {} bytes; the limit is {MAX_CHECKS_BYTES}",
                text.len()
            ),
        ));
    }
    let end = Span::new(text.len(), text.len());
    let mut lex = Lexer::new(text);
    let open = match lex.next_token()? {
        None => {
            return Err(DatalogError::syntax(
                end,
                "empty checks file: expected a vector of checks `[[:name [:find ...]] ...]`",
            ));
        }
        Some(Token {
            tok: Tok::Open(Bracket::Square),
            span,
        }) => span,
        Some(t) => {
            return Err(DatalogError::syntax(
                t.span,
                format!(
                    "expected a vector of checks `[[:name [:find ...]] ...]`, found {}",
                    what(&t.tok)
                ),
            ));
        }
    };
    let unclosed =
        |s: Span, b: Bracket| DatalogError::syntax(s, format!("unclosed `{}`", b.open()));
    let mut checks = Vec::new();
    let mut names = BTreeSet::new();
    loop {
        let Some(t) = lex.next_token()? else {
            return Err(unclosed(open, Bracket::Square));
        };
        let check_open = match t.tok {
            Tok::Close(Bracket::Square) => break,
            Tok::Open(Bracket::Square) => t.span,
            other => {
                return Err(DatalogError::syntax(
                    t.span,
                    format!("expected a check, found {}: {SHAPE}", what(&other)),
                ));
            }
        };
        let c = check(&mut lex, text, check_open)?;
        // Checks come in file order, so this is the second use.
        if !names.insert(c.name.clone()) {
            return Err(DatalogError::syntax(
                c.span,
                format!("duplicate check name `:{}`", show(&c.name)),
            ));
        }
        if checks.len() >= MAX_CHECKS {
            return Err(DatalogError::limit(
                Some(c.span),
                format!("more than {MAX_CHECKS} checks"),
            ));
        }
        checks.push(c);
    }
    if let Some(t) = lex.next_token()? {
        return Err(DatalogError::syntax(
            t.span,
            "unexpected input after the vector of checks",
        ));
    }
    Ok(checks)
}

/// One check after its opening `[` at `open`.
fn check(lex: &mut Lexer<'_>, text: &str, open: Span) -> Result<Check, DatalogError> {
    let unclosed = || DatalogError::syntax(open, "unclosed `[`");
    let Some(t) = lex.next_token()? else {
        return Err(unclosed());
    };
    let name = match t.tok {
        Tok::Keyword(k) => k,
        Tok::Close(Bracket::Square) => {
            return Err(DatalogError::syntax(
                Span::new(open.start, t.span.end),
                format!("empty check: {SHAPE}"),
            ));
        }
        other => {
            return Err(DatalogError::syntax(
                t.span,
                format!(
                    "a check's name must be a keyword, found {}: {SHAPE}",
                    what(&other)
                ),
            ));
        }
    };
    let Some(t) = lex.next_token()? else {
        return Err(unclosed());
    };
    let q_start = match t.tok {
        Tok::Open(Bracket::Square) => t.span.start,
        other => {
            return Err(DatalogError::syntax(
                t.span,
                format!(
                    "check `:{}` needs a query vector, found {}: {SHAPE}",
                    show(&name),
                    what(&other)
                ),
            ));
        }
    };
    let q_end = query_end(lex, q_start)?;
    let Some(t) = lex.next_token()? else {
        return Err(unclosed());
    };
    match t.tok {
        Tok::Close(Bracket::Square) => {}
        other => {
            return Err(DatalogError::syntax(
                t.span,
                format!(
                    "check `:{}` has an extra element ({}) after its query: {SHAPE}",
                    show(&name),
                    what(&other)
                ),
            ));
        }
    }
    Ok(Check {
        name,
        query: text[q_start..q_end].to_string(),
        span: Span::new(open.start, t.span.end),
        query_span: Span::new(q_start, q_end),
    })
}

/// The byte offset just past the query vector whose `[` is at `start` (the
/// lexer is just past it). Checks bracket matching, [`MAX_DEPTH`] (the
/// query's outer vector counts as one level, as in [`crate::parse`]) and
/// [`MAX_QUERY_BYTES`] as it goes.
fn query_end(lex: &mut Lexer<'_>, start: usize) -> Result<usize, DatalogError> {
    let mut stack: Vec<(Bracket, Span)> = vec![(Bracket::Square, Span::new(start, start + 1))];
    loop {
        let Some(t) = lex.next_token()? else {
            let (b, s) = stack
                .last()
                .copied()
                .unwrap_or((Bracket::Square, Span::new(start, start + 1)));
            return Err(DatalogError::syntax(s, format!("unclosed `{}`", b.open())));
        };
        if t.span.end - start > MAX_QUERY_BYTES {
            return Err(DatalogError::limit(
                Some(Span::new(start, start + 1)),
                format!("a check's query is longer than {MAX_QUERY_BYTES} bytes"),
            ));
        }
        match t.tok {
            Tok::Open(b) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(DatalogError::limit(
                        Some(t.span),
                        format!("nesting is deeper than {MAX_DEPTH} levels"),
                    ));
                }
                stack.push((b, t.span));
            }
            Tok::Close(b) => {
                // Never empty here: the query's own closing bracket empties
                // it and returns below.
                if let Some((ob, _)) = stack.pop()
                    && ob != b
                {
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
                if stack.is_empty() {
                    return Ok(t.span.end);
                }
            }
            _ => {}
        }
    }
}
