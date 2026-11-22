//! Lexer for the EDN subset. Pulled one token at a time, so a limit error
//! early in the input is reported before anything later is looked at.
//! Every step is linear in the input; nothing recurses.

use crate::error::{DatalogError, Span, show};

/// Which bracket pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bracket {
    /// `[ ]`, a vector.
    Square,
    /// `( )`, a list.
    Round,
}

impl Bracket {
    pub(crate) fn open(self) -> char {
        match self {
            Bracket::Square => '[',
            Bracket::Round => '(',
        }
    }

    pub(crate) fn close(self) -> char {
        match self {
            Bracket::Square => ']',
            Bracket::Round => ')',
        }
    }
}

/// A token's content.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    Open(Bracket),
    Close(Bracket),
    /// `:name` (name without the colon).
    Keyword(String),
    /// `?name` (name without the question mark).
    Var(String),
    /// `_`.
    Any,
    /// `$`.
    Db,
    Bool(bool),
    /// An integer literal. `None` if it doesn't even fit an `i128`; range
    /// checks against `i64`/`u64` depend on where it is used.
    Int(Option<i128>),
    /// A finite float literal.
    Float(f64),
    Str(String),
    /// `TYPE#n` / `edge#n`, as written.
    Id(String),
    /// Any other bare symbol (`at`, `not`, `=`, `foo`, ...).
    Sym(String),
}

/// A token and where it is.
#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub(crate) tok: Tok,
    pub(crate) span: Span,
}

/// Characters that end a bare token.
fn is_delimiter(c: char) -> bool {
    c.is_whitespace() || matches!(c, ',' | '[' | ']' | '(' | ')' | '{' | '}' | '"' | ';')
}

/// Characters allowed in keyword, variable and symbol names.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            '_' | '-' | '*' | '+' | '!' | '?' | '<' | '>' | '=' | '/' | '.'
        )
}

pub(crate) struct Lexer<'a> {
    text: &'a str,
    pos: usize,
    unfinished_string: bool,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(text: &'a str) -> Lexer<'a> {
        Lexer {
            text,
            pos: 0,
            unfinished_string: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    /// Skip whitespace, commas and `;` comments.
    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            if c == ';' {
                match self.text[self.pos..].find('\n') {
                    Some(i) => self.pos += i + 1,
                    None => self.pos = self.text.len(),
                }
            } else if c == ',' || c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    /// The next token, `Ok(None)` at end of input.
    pub(crate) fn next_token(&mut self) -> Result<Option<Token>, DatalogError> {
        self.skip_trivia();
        let start = self.pos;
        let Some(c) = self.peek() else {
            return Ok(None);
        };
        let single = |tok| {
            Ok(Some(Token {
                tok,
                span: Span::new(start, start + 1),
            }))
        };
        match c {
            '[' => {
                self.pos += 1;
                single(Tok::Open(Bracket::Square))
            }
            ']' => {
                self.pos += 1;
                single(Tok::Close(Bracket::Square))
            }
            '(' => {
                self.pos += 1;
                single(Tok::Open(Bracket::Round))
            }
            ')' => {
                self.pos += 1;
                single(Tok::Close(Bracket::Round))
            }
            '{' | '}' => Err(DatalogError::syntax(
                Span::new(start, start + 1),
                format!("unexpected `{c}`: maps and sets are not supported"),
            )),
            '"' => self.string(start).map(Some),
            _ => self.bare(start).map(Some),
        }
    }

    /// A string literal starting at the opening quote `start`.
    fn string(&mut self, start: usize) -> Result<Token, DatalogError> {
        let text = self.text;
        let unterminated =
            || DatalogError::syntax(Span::new(start, text.len()), "unterminated string");
        let mut out = String::new();
        let mut chars = text[start + 1..]
            .char_indices()
            .map(|(i, c)| (start + 1 + i, c));
        loop {
            let Some((i, c)) = chars.next() else {
                self.unfinished_string = true;
                return Err(unterminated());
            };
            match c {
                '"' => {
                    self.pos = i + 1;
                    return Ok(Token {
                        tok: Tok::Str(out),
                        span: Span::new(start, i + 1),
                    });
                }
                '\\' => {
                    let Some((j, e)) = chars.next() else {
                        self.unfinished_string = true;
                        return Err(unterminated());
                    };
                    let after = j + e.len_utf8();
                    match e {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'u' => {
                            let mut code = 0u32;
                            let mut end = after;
                            for _ in 0..4 {
                                let Some((k, h)) = chars.next() else {
                                    self.unfinished_string = true;
                                    return Err(unterminated());
                                };
                                end = k + h.len_utf8();
                                let Some(d) = h.to_digit(16) else {
                                    return Err(DatalogError::syntax(
                                        Span::new(i, end),
                                        "`\\u` needs exactly 4 hex digits (`\\uXXXX`)",
                                    ));
                                };
                                code = code * 16 + d;
                            }
                            match char::from_u32(code) {
                                Some(ch) => out.push(ch),
                                None => {
                                    return Err(DatalogError::syntax(
                                        Span::new(i, end),
                                        format!("`\\u{code:04X}` is a surrogate, not a character"),
                                    ));
                                }
                            }
                        }
                        other => {
                            return Err(DatalogError::syntax(
                                Span::new(i, after),
                                format!(
                                    "unknown escape `\\{}` (use \\\" \\\\ \\n \\t \\r or \\uXXXX)",
                                    show(other.encode_utf8(&mut [0; 4]))
                                ),
                            ));
                        }
                    }
                }
                c => out.push(c),
            }
        }
    }

    /// A bare token (keyword, variable, number, symbol, ...) at `start`.
    fn bare(&mut self, start: usize) -> Result<Token, DatalogError> {
        let rest = &self.text[start..];
        let len = rest.find(is_delimiter).unwrap_or(rest.len());
        let run = &rest[..len];
        self.pos = start + len;
        let span = Span::new(start, start + len);
        let tok = classify(run, span)?;
        Ok(Token { tok, span })
    }
}

/// Validate a name (after its `:` / `?` sigil).
fn name(run: &str, sigil: &str, what: &str, span: Span) -> Result<String, DatalogError> {
    let body = &run[sigil.len()..];
    if body.is_empty() {
        return Err(DatalogError::syntax(
            span,
            format!("{what} needs a name after `{sigil}`"),
        ));
    }
    if let Some(bad) = body.chars().find(|&c| !is_name_char(c)) {
        return Err(DatalogError::syntax(
            span,
            format!(
                "unexpected character `{}` in {what} `{}`",
                show(&bad.to_string()),
                show(run)
            ),
        ));
    }
    Ok(body.to_string())
}

fn classify(run: &str, span: Span) -> Result<Tok, DatalogError> {
    let mut chars = run.chars();
    let first = chars.next().unwrap_or(' ');
    let second = chars.next();
    if first.is_ascii_digit()
        || (matches!(first, '+' | '-') && second.is_some_and(|c| c.is_ascii_digit()))
    {
        return number(run, span);
    }
    match run {
        "_" => return Ok(Tok::Any),
        "$" => return Ok(Tok::Db),
        "true" => return Ok(Tok::Bool(true)),
        "false" => return Ok(Tok::Bool(false)),
        _ => {}
    }
    match first {
        ':' => return name(run, ":", "keyword", span).map(Tok::Keyword),
        '?' => return name(run, "?", "variable", span).map(Tok::Var),
        _ => {}
    }
    if let Some((ty, seq)) = run.split_once('#') {
        let ty_ok = ty
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && ty.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let seq_ok = !seq.is_empty() && seq.bytes().all(|b| b.is_ascii_digit());
        if ty_ok && seq_ok {
            return Ok(Tok::Id(run.to_string()));
        }
        return Err(DatalogError::syntax(
            span,
            format!("invalid ID `{}` (expected TYPE#n or edge#n)", show(run)),
        ));
    }
    if let Some(bad) = run.chars().find(|&c| !is_name_char(c)) {
        return Err(DatalogError::syntax(
            span,
            format!("unexpected character `{}`", show(&bad.to_string())),
        ));
    }
    Ok(Tok::Sym(run.to_string()))
}

/// `[+-]? digits ('.' digits)? ([eE] [+-]? digits)?`; a float if it has a
/// fraction or an exponent.
fn number(run: &str, span: Span) -> Result<Tok, DatalogError> {
    let invalid = || DatalogError::syntax(span, format!("invalid number `{}`", show(run)));
    let b = run.as_bytes();
    let mut i = 0;
    let neg = b[0] == b'-';
    if matches!(b[0], b'+' | b'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let s = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - s
    };
    let int_start = i;
    if digits(&mut i) == 0 {
        return Err(invalid());
    }
    let int_end = i;
    let mut is_float = false;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if digits(&mut i) == 0 {
            return Err(invalid());
        }
        is_float = true;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if i < b.len() && matches!(b[i], b'+' | b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return Err(invalid());
        }
        is_float = true;
    }
    if i != b.len() {
        return Err(invalid());
    }
    if is_float {
        // The run is pure ASCII number syntax here, which Rust's parser accepts.
        let f: f64 = run.parse().map_err(|_| invalid())?;
        if !f.is_finite() {
            return Err(DatalogError::syntax(
                span,
                format!("float `{}` is out of range", show(run)),
            ));
        }
        return Ok(Tok::Float(f));
    }
    let mut mag: Option<i128> = Some(0);
    for &d in &b[int_start..int_end] {
        mag = mag
            .and_then(|m| m.checked_mul(10))
            .and_then(|m| m.checked_add(i128::from(d - b'0')));
    }
    Ok(Tok::Int(mag.map(|m| if neg { -m } else { m })))
}

/// Structural completeness uses the same tokenizer as the parser, including
/// its escape validation. No AST is built, and nesting has a fixed cap.
pub(crate) fn completeness(text: &str) -> crate::Completeness {
    use crate::Completeness::{Complete, Incomplete, Invalid};
    if text.len() > crate::MAX_QUERY_BYTES {
        return Invalid;
    }
    let mut lexer = Lexer::new(text);
    let mut stack = Vec::new();
    let mut seen = false;
    let mut finished = false;
    loop {
        let token = match lexer.next_token() {
            Ok(Some(token)) => token,
            Ok(None) => {
                return if seen && stack.is_empty() {
                    Complete
                } else {
                    Incomplete
                };
            }
            Err(_) => {
                return if lexer.unfinished_string && !finished {
                    Incomplete
                } else {
                    Invalid
                };
            }
        };
        if finished {
            return Invalid;
        }
        seen = true;
        match token.tok {
            Tok::Open(bracket) => {
                if stack.len() == crate::MAX_DEPTH {
                    return Invalid;
                }
                stack.push(bracket);
            }
            Tok::Close(bracket) => {
                if stack.pop() != Some(bracket) {
                    return Invalid;
                }
                finished = stack.is_empty();
            }
            _ => finished = stack.is_empty(),
        }
    }
}
