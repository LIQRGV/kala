//! Bounded UTF-8 lexer. No recursive tokenization.
use crate::{CypherError, ErrorKind, MAX_QUERY_BYTES};
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    Ident(String, bool),
    Param(String),
    String(String),
    Number(String),
    Symbol(&'static str),
    End,
}
#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}
/// Symbols in match order: every two-character symbol comes before its prefix.
const SYMBOLS: [&str; 22] = [
    "<>", "<=", ">=", "->", "<-", "(", ")", "[", "]", "{", "}", ":", ",", ".", ";", "=", "<", ">",
    "-", "+", "*", "|",
];
/// Most tokens one query may hold.
const MAX_TOKENS: usize = 8192;
/// Deepest `/* … */` nesting accepted.
const MAX_COMMENT_DEPTH: usize = 32;
pub(crate) fn lex(text: &str) -> Result<Vec<Token>, CypherError> {
    if text.len() > MAX_QUERY_BYTES {
        return Err(CypherError::new(
            text,
            0,
            ErrorKind::Limit,
            "query byte limit exceeded",
        ));
    }
    let mut lexer = Lexer { text, i: 0 };
    let mut out = Vec::new();
    while lexer.i < text.len() {
        let start = lexer.i;
        let c = lexer
            .peek()
            .ok_or_else(|| lexer.error(start, "invalid token"))?;
        if c.is_whitespace() {
            lexer.i += c.len_utf8();
            continue;
        }
        if lexer.rest().starts_with("//") {
            lexer.skip_line_comment();
            continue;
        }
        if lexer.rest().starts_with("/*") {
            lexer.skip_block_comment()?;
            continue;
        }
        let kind = if matches!(c, '\'' | '"' | '`') {
            lexer.quoted(c)?
        } else if c == '$' || c == '_' || c.is_alphabetic() {
            lexer.word(c == '$')?
        } else if c.is_ascii_digit() {
            lexer.number()?
        } else {
            lexer.symbol()?
        };
        out.push(Token {
            kind,
            start,
            end: lexer.i,
        });
        if out.len() > MAX_TOKENS {
            return Err(CypherError::new(
                text,
                lexer.i,
                ErrorKind::Limit,
                "token limit exceeded",
            ));
        }
    }
    out.push(Token {
        kind: Kind::End,
        start: lexer.i,
        end: lexer.i,
    });
    Ok(out)
}
/// A cursor over the query text; `i` is always a character boundary.
struct Lexer<'a> {
    text: &'a str,
    i: usize,
}
impl Lexer<'_> {
    fn error(&self, at: usize, message: &str) -> CypherError {
        CypherError::new(self.text, at, ErrorKind::Syntax, message)
    }
    /// The text from the cursor on.
    fn rest(&self) -> &str {
        &self.text[self.i..]
    }
    /// The character at the cursor, if any.
    fn peek(&self) -> Option<char> {
        self.text.get(self.i..).and_then(|s| s.chars().next())
    }
    /// The byte at the cursor, if any.
    fn byte(&self) -> Option<u8> {
        self.text.as_bytes().get(self.i).copied()
    }
    /// Skip a run of ASCII digits (possibly empty).
    fn skip_digits(&mut self) {
        while self.byte().is_some_and(|b| b.is_ascii_digit()) {
            self.i += 1;
        }
    }
    /// Skip a `//` comment up to (not including) the line break.
    fn skip_line_comment(&mut self) {
        self.i = self
            .rest()
            .find('\n')
            .map_or(self.text.len(), |n| self.i + n);
    }
    /// Skip a `/* … */` comment; comments nest, up to a fixed depth.
    fn skip_block_comment(&mut self) -> Result<(), CypherError> {
        let start = self.i;
        self.i += 2;
        let mut depth = 1;
        while self.i < self.text.len() && depth > 0 {
            if self.rest().starts_with("/*") {
                depth += 1;
                if depth > MAX_COMMENT_DEPTH {
                    return Err(self.error(self.i, "comment nesting limit exceeded"));
                }
                self.i += 2;
            } else if self.rest().starts_with("*/") {
                depth -= 1;
                self.i += 2;
            } else {
                self.i += self
                    .peek()
                    .ok_or_else(|| self.error(self.i, "comment"))?
                    .len_utf8();
            }
        }
        if depth > 0 {
            return Err(self.error(start, "unterminated comment"));
        }
        Ok(())
    }
    /// A `'…'` or `"…"` string, or a `` `…` `` identifier (where a doubled
    /// backtick is a literal one).
    fn quoted(&mut self, quote: char) -> Result<Kind, CypherError> {
        let start = self.i;
        self.i += 1;
        let mut value = String::new();
        let mut closed = false;
        while self.i < self.text.len() {
            let ch = self
                .peek()
                .ok_or_else(|| self.error(self.i, "unterminated string"))?;
            self.i += ch.len_utf8();
            if ch == quote {
                if quote == '`' && self.rest().starts_with('`') {
                    self.i += 1;
                    value.push('`');
                    continue;
                }
                closed = true;
                break;
            }
            if ch == '\\' {
                value.push(self.escape()?);
            } else {
                value.push(ch);
            }
        }
        if !closed {
            return Err(self.error(start, "unterminated quoted string or identifier"));
        }
        if quote == '`' {
            if value.is_empty() {
                return Err(self.error(start, "empty identifier"));
            }
            Ok(Kind::Ident(value, true))
        } else {
            Ok(Kind::String(value))
        }
    }
    /// The character a backslash escape stands for; the cursor is just past
    /// the backslash.
    fn escape(&mut self) -> Result<char, CypherError> {
        let esc = self
            .peek()
            .ok_or_else(|| self.error(self.i, "unterminated escape"))?;
        self.i += esc.len_utf8();
        Ok(match esc {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'b' => '\u{8}',
            'f' => '\u{c}',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            '`' => '`',
            'u' | 'U' => self.unicode_escape(if esc == 'u' { 4 } else { 8 })?,
            _ => return Err(self.error(self.i, "unsupported string escape")),
        })
    }
    /// A `\uXXXX` or `\UXXXXXXXX` escape of `digits` hex digits; a high
    /// surrogate may be followed by a `\uXXXX` low surrogate.
    fn unicode_escape(&mut self, digits: usize) -> Result<char, CypherError> {
        let mut code = self.hex(digits)?;
        self.i += digits;
        if (0xd800..=0xdbff).contains(&code) && self.rest().starts_with("\\u") {
            self.i += 2;
            let low = self.hex(4)?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(self.error(self.i, "invalid Unicode surrogate"));
            }
            self.i += 4;
            code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
        }
        char::from_u32(code).ok_or_else(|| self.error(self.i, "invalid Unicode code point"))
    }
    /// Read `digits` hex digits at the cursor without moving it.
    fn hex(&self, digits: usize) -> Result<u32, CypherError> {
        let hex = self
            .text
            .get(self.i..self.i + digits)
            .ok_or_else(|| self.error(self.i, "invalid Unicode escape"))?;
        u32::from_str_radix(hex, 16).map_err(|_| self.error(self.i, "invalid Unicode escape"))
    }
    /// A bare identifier (keywords included), or a `$name` parameter.
    fn word(&mut self, param: bool) -> Result<Kind, CypherError> {
        if param {
            self.i += 1;
        }
        let name_start = self.i;
        let Some(first) = self.peek() else {
            return Err(self.error(self.i, "parameter name expected"));
        };
        if first != '_' && !first.is_alphabetic() {
            return Err(self.error(self.i, "parameter name expected"));
        }
        self.i += first.len_utf8();
        while let Some(ch) = self.peek() {
            if ch == '_' || ch.is_alphanumeric() {
                self.i += ch.len_utf8();
            } else {
                break;
            }
        }
        let name = self.text[name_start..self.i].to_owned();
        Ok(if param {
            Kind::Param(name)
        } else {
            Kind::Ident(name, false)
        })
    }
    /// An unsigned number: digits, an optional fraction, an optional exponent.
    fn number(&mut self) -> Result<Kind, CypherError> {
        let start = self.i;
        self.i += 1;
        self.skip_digits();
        if self.byte() == Some(b'.')
            && self
                .text
                .as_bytes()
                .get(self.i + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            self.i += 1;
            self.skip_digits();
        }
        if self.byte().is_some_and(|c| matches!(c, b'e' | b'E')) {
            self.i += 1;
            if self.byte().is_some_and(|c| matches!(c, b'+' | b'-')) {
                self.i += 1;
            }
            let digits = self.i;
            self.skip_digits();
            if self.i == digits {
                return Err(self.error(start, "invalid numeric literal"));
            }
        }
        Ok(Kind::Number(self.text[start..self.i].into()))
    }
    /// The longest symbol at the cursor.
    fn symbol(&mut self) -> Result<Kind, CypherError> {
        let symbol = SYMBOLS
            .into_iter()
            .find(|s| self.rest().starts_with(s))
            .ok_or_else(|| self.error(self.i, "token is not supported yet"))?;
        self.i += symbol.len();
        Ok(Kind::Symbol(symbol))
    }
}
