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
#[cfg(test)]
fn batch_completeness(text: &str) -> crate::Completeness {
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

/// Resumable structural completeness. Chunks must be valid UTF-8 (`&str`),
/// but may split tokens, comments, escapes and Unicode escape digits.
#[derive(Default)]
pub struct CompletenessScanner {
    stack: Vec<Bracket>,
    mode: ScanMode,
    bytes: usize,
    seen: bool,
    finished: bool,
    invalid: bool,
}

#[derive(Default)]
enum ScanMode {
    #[default]
    Trivia,
    Comment,
    String,
    Escape,
    Unicode {
        code: u32,
        left: u8,
    },
    Bare(Bare),
}

impl CompletenessScanner {
    /// Start with no pending form.
    pub fn new() -> Self {
        Self::default()
    }

    /// Discard all lexical, bracket and size state.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Consume only the new chunk and return the completeness of all input
    /// so far. An unfinished bare token can have an invalid prefix (`1e`,
    /// `:`), then become valid when another chunk extends it.
    pub fn push(&mut self, chunk: &str) -> crate::Completeness {
        self.bytes = self.bytes.saturating_add(chunk.len());
        self.invalid |= self.bytes > crate::MAX_QUERY_BYTES;
        for c in chunk.chars() {
            if self.invalid {
                break;
            }
            self.character(c);
        }
        self.status()
    }

    fn status(&self) -> crate::Completeness {
        use crate::Completeness::{Complete, Incomplete, Invalid};
        if self.invalid {
            return Invalid;
        }
        match &self.mode {
            ScanMode::Bare(bare) if !bare.valid() => Invalid,
            ScanMode::Bare(_) if self.stack.is_empty() => Complete,
            ScanMode::String | ScanMode::Escape | ScanMode::Unicode { .. } => Incomplete,
            _ if self.seen && self.stack.is_empty() => Complete,
            _ => Incomplete,
        }
    }

    fn end_token(&mut self) {
        self.seen = true;
        self.finished = self.stack.is_empty();
        self.mode = ScanMode::Trivia;
    }

    fn character(&mut self, c: char) {
        match &mut self.mode {
            ScanMode::Comment => {
                if c == '\n' {
                    self.mode = ScanMode::Trivia;
                }
                return;
            }
            ScanMode::String => {
                match c {
                    '"' => self.end_token(),
                    '\\' => self.mode = ScanMode::Escape,
                    _ => {}
                }
                return;
            }
            ScanMode::Escape => {
                self.mode = match c {
                    '"' | '\\' | 'n' | 't' | 'r' => ScanMode::String,
                    'u' => ScanMode::Unicode { code: 0, left: 4 },
                    _ => {
                        self.invalid = true;
                        ScanMode::Trivia
                    }
                };
                return;
            }
            ScanMode::Unicode { code, left } => {
                let Some(digit) = c.to_digit(16) else {
                    self.invalid = true;
                    return;
                };
                *code = *code * 16 + digit;
                *left -= 1;
                if *left == 0 {
                    self.invalid = char::from_u32(*code).is_none();
                    self.mode = ScanMode::String;
                }
                return;
            }
            ScanMode::Bare(bare) => {
                if !is_delimiter(c) {
                    bare.push(c);
                    return;
                }
                if !bare.valid() {
                    self.invalid = true;
                    return;
                }
                self.end_token();
                // The delimiter belongs to the next token/trivia.
            }
            ScanMode::Trivia => {}
        }
        if c.is_whitespace() || c == ',' {
            return;
        }
        if c == ';' {
            self.mode = ScanMode::Comment;
            return;
        }
        if self.finished {
            self.invalid = true;
            return;
        }
        match c {
            '[' | '(' => {
                if self.stack.len() == crate::MAX_DEPTH {
                    self.invalid = true;
                    return;
                }
                self.stack.push(if c == '[' {
                    Bracket::Square
                } else {
                    Bracket::Round
                });
                self.seen = true;
            }
            ']' | ')' => {
                let bracket = if c == ']' {
                    Bracket::Square
                } else {
                    Bracket::Round
                };
                if self.stack.pop() != Some(bracket) {
                    self.invalid = true;
                    return;
                }
                self.end_token();
            }
            '{' | '}' => self.invalid = true,
            '"' => self.mode = ScanMode::String,
            _ => self.mode = ScanMode::Bare(Bare::new(c)),
        }
    }
}

/// Incremental validation of a bare token. Do not re-tokenize its growing
/// prefix: a long token supplied one character at a time must stay linear.
enum Bare {
    Name { nonempty: bool },
    Symbol { sign: bool, id_type: bool },
    Id { nonempty: bool },
    Dollar,
    Number(NumberScan),
    Bad,
}

impl Bare {
    fn new(c: char) -> Self {
        match c {
            ':' | '?' => Self::Name { nonempty: false },
            '$' => Self::Dollar,
            '0'..='9' => Self::Number(NumberScan::new(c)),
            _ if is_name_char(c) => Self::Symbol {
                sign: matches!(c, '+' | '-'),
                id_type: c.is_ascii_alphabetic() || c == '_',
            },
            _ => Self::Bad,
        }
    }

    fn push(&mut self, c: char) {
        match self {
            Self::Name { nonempty } => {
                if is_name_char(c) {
                    *nonempty = true;
                } else {
                    *self = Self::Bad;
                }
            }
            Self::Symbol { sign, id_type } => {
                if *sign && c.is_ascii_digit() {
                    *self = Self::Number(NumberScan::new(c));
                } else if c == '#' && *id_type {
                    *self = Self::Id { nonempty: false };
                } else if is_name_char(c) {
                    *sign = false;
                    *id_type &= c.is_ascii_alphanumeric() || c == '_';
                } else {
                    *self = Self::Bad;
                }
            }
            Self::Id { nonempty } => {
                if c.is_ascii_digit() {
                    *nonempty = true;
                } else {
                    *self = Self::Bad;
                }
            }
            Self::Number(number) => number.push(c),
            Self::Dollar => *self = Self::Bad,
            Self::Bad => {}
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Name { nonempty } | Self::Id { nonempty } => *nonempty,
            Self::Symbol { .. } | Self::Dollar => true,
            Self::Number(number) => number.valid(),
            Self::Bad => false,
        }
    }
}

#[derive(Clone, Copy)]
enum NumberPart {
    Integer,
    Dot,
    Fraction,
    Exponent,
    ExponentSign,
    ExponentDigits,
    Bad,
}

struct NumberScan {
    part: NumberPart,
    integer_digits: i64,
    leading_zeroes: i64,
    // Only overflow matters for completeness. The f64 infinity rounding
    // boundary (2^1024 - 2^970) is an integer with 309 decimal digits.
    // Keeping that many significant digits preserves which side of the
    // boundary a decimal falls on, including a tie; further digits cannot
    // make a prefix already at the boundary finite.
    significant: String,
    exponent: i64,
    negative_exponent: bool,
}

impl NumberScan {
    fn new(c: char) -> Self {
        let mut scan = Self {
            part: NumberPart::Integer,
            integer_digits: 0,
            leading_zeroes: 0,
            significant: String::new(),
            exponent: 0,
            negative_exponent: false,
        };
        scan.digit(c, true);
        scan
    }

    fn digit(&mut self, c: char, integer: bool) {
        self.integer_digits += i64::from(integer);
        if self.significant.is_empty() && c == '0' {
            self.leading_zeroes += 1;
        } else if self.significant.len() < 309 {
            self.significant.push(c);
        }
    }

    fn push(&mut self, c: char) {
        use NumberPart::*;
        self.part = match (self.part, c) {
            (Integer, '0'..='9') => {
                self.digit(c, true);
                Integer
            }
            (Integer, '.') => Dot,
            (Integer | Fraction, 'e' | 'E') => Exponent,
            (Dot | Fraction, '0'..='9') => {
                self.digit(c, false);
                Fraction
            }
            (Exponent, '+' | '-') => {
                self.negative_exponent = c == '-';
                ExponentSign
            }
            (Exponent | ExponentSign | ExponentDigits, '0'..='9') => {
                // More than the entire input's digits cannot be cancelled
                // by the mantissa's decimal position.
                self.exponent = (self.exponent * 10 + i64::from(c as u8 - b'0'))
                    .min(crate::MAX_QUERY_BYTES as i64 * 2);
                ExponentDigits
            }
            _ => Bad,
        };
    }

    fn valid(&self) -> bool {
        use NumberPart::*;
        match self.part {
            Integer => true, // out-of-range integers remain lexical tokens
            Fraction | ExponentDigits => {
                if self.significant.is_empty() {
                    return true;
                }
                let exponent = if self.negative_exponent {
                    -self.exponent
                } else {
                    self.exponent
                };
                let order = self.integer_digits - self.leading_zeroes + exponent;
                if order > 309 {
                    return false;
                }
                if order < 309 {
                    return true;
                }
                // Constant-size parse, even for a 64 KiB mantissa supplied
                // in tiny chunks. The sign cannot change finiteness.
                format!("0.{}e{order}", self.significant)
                    .parse::<f64>()
                    .is_ok_and(f64::is_finite)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod completeness_oracle_tests {
    use super::{CompletenessScanner, batch_completeness};
    use crate::{Completeness, MAX_QUERY_BYTES};
    use proptest::prelude::*;

    fn compare_chunks(text: &str, chars_per_chunk: usize) {
        let mut scanner = CompletenessScanner::new();
        let boundaries: Vec<_> = text
            .char_indices()
            .map(|(at, _)| at)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut start = 0;
        for end in (chars_per_chunk..boundaries.len())
            .step_by(chars_per_chunk)
            .chain(std::iter::once(boundaries.len() - 1))
        {
            let end = boundaries[end];
            assert_eq!(
                scanner.push(&text[start..end]),
                batch_completeness(&text[..end]),
                "batch oracle disagrees at {end} in {text:?}"
            );
            start = end;
        }
    }

    fn soup() -> impl Strategy<Value = String> {
        prop::collection::vec(
            prop::sample::select(vec![
                "[", "]", "(", ")", "{", "}", "\"", "\\", "\\u", "0041", "D800", "?", ":", "a",
                "_", "$", "#", "TYPE#", "123", "+", "-", ".", "1e", "1e999", " ", "\n", "\r", ",",
                ";", "é", "😀", "中", "\u{2003}", "true", "false", "=", "'", "@", "?a", ":keyword",
                "\\q",
            ]),
            0..60,
        )
        .prop_map(|tokens| tokens.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn incremental_chunks_match_the_original_tokenizer(text in soup(), chunk in 1usize..16) {
            prop_assert_eq!(CompletenessScanner::new().push(&text), batch_completeness(&text));
            compare_chunks(&text, chunk);
            compare_chunks(&text, 1);
        }

        #[test]
        fn arbitrary_unicode_matches_the_original_tokenizer(text in ".{0,180}", chunk in 1usize..20) {
            compare_chunks(&text, chunk);
        }

        #[test]
        fn valid_nested_forms_match_the_original_tokenizer(
            tokens in prop::collection::vec(prop::sample::select(vec![
                "?é", ":名", "true", "false", "_", "$", "TYPE#42", "edge#1", "+-1",
                "(at -1 [?e :name \"é😀\\u0041\\\\\\\"\"])",
                "[0 1.25 1e-300]", "\"; [ ] ( )\"", "; ignored ] )\n", "(not [?a :x ?b])",
            ]), 0..30),
            chunk in 1usize..16,
        ) {
            let text = format!("[{}]", tokens.join(" "));
            prop_assert_eq!(batch_completeness(&text), Completeness::Complete);
            compare_chunks(&text, chunk);
            compare_chunks(&text, 1);
        }

        #[test]
        fn large_numeric_mantissas_and_exponents_match_the_original_tokenizer(
            sign in prop::sample::select(vec!["", "+", "-"]),
            integer in "[0-9]{1,700}",
            fraction in "[0-9]{1,700}",
            exponent in -2000i32..2000,
            chunk in 1usize..24,
        ) {
            let text = format!("[{sign}{integer}.{fraction}e{exponent}]");
            prop_assert_eq!(CompletenessScanner::new().push(&text), batch_completeness(&text));
            compare_chunks(&text, chunk);
        }
    }

    #[test]
    fn float_overflow_rounding_boundary_and_extreme_exponents_match_batch() {
        // Exact halfway point between f64::MAX and infinity: 2^1024-2^970.
        const BOUNDARY: &str = "179769313486231580793728971405303415079934132710037826936173778980444968292764750946649017977587207096330286416692887910946555547851940402630657488671505820681908902000708383676273854845817711531764475730270069855571366959622842914819860834936475292719074168444365510704342711559699508093042880177904174497792";
        let mut below = BOUNDARY.as_bytes().to_vec();
        *below.last_mut().unwrap() -= 1;
        let below = String::from_utf8(below).unwrap();
        for (digits, expected) in [
            (BOUNDARY, Completeness::Invalid),
            (below.as_str(), Completeness::Complete),
        ] {
            let literal = format!("{digits}.0");
            assert_eq!(batch_completeness(&literal), expected);
            compare_chunks(&literal, 1);
            for sign in ["", "+", "-"] {
                for text in [
                    format!("{sign}{digits}.{}", "9".repeat(1_000)),
                    format!("{sign}0.{digits}e309"),
                    format!("{sign}000{digits}.0"),
                    format!("{sign}{digits}{}e-1000", "0".repeat(1_000)),
                    format!("{sign}0.{}{digits}e1309", "0".repeat(1_000)),
                ] {
                    compare_chunks(&text, 17);
                }
            }
        }
        for text in [
            format!("0e{}", "9".repeat(1_000)),
            format!("1e-{}", "9".repeat(1_000)),
            format!("1e{}", "9".repeat(1_000)),
            format!("0.{}1e10000", "0".repeat(10_000)),
            format!("1{}e-10000", "0".repeat(10_000)),
        ] {
            compare_chunks(&text, 113);
        }
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn one_character_bare_tokens_are_linear_under_two_hundred_ms() {
        for prefix in ["?", "17976931348623157"] {
            let mut text = prefix.to_string();
            if prefix != "?" {
                text.push_str(&"0".repeat(309 - prefix.len()));
                text.push('.');
            }
            text.push_str(&"0".repeat(MAX_QUERY_BYTES - text.len()));
            let expected = batch_completeness(&text);
            let start = std::time::Instant::now();
            let mut scanner = CompletenessScanner::new();
            let mut actual = Completeness::Incomplete;
            for at in 0..text.len() {
                actual = scanner.push(&text[at..at + 1]);
            }
            let elapsed = start.elapsed();
            assert_eq!(actual, expected);
            assert!(
                elapsed < std::time::Duration::from_millis(200),
                "bare-token input took {elapsed:?}"
            );
        }
    }
}
