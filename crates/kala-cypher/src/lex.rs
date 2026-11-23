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
pub(crate) fn lex(text: &str) -> Result<Vec<Token>, CypherError> {
    let error = |at, message: &str| CypherError::new(text, at, ErrorKind::Syntax, message);
    if text.len() > MAX_QUERY_BYTES {
        return Err(CypherError::new(
            text,
            0,
            ErrorKind::Limit,
            "query byte limit exceeded",
        ));
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        let start = i;
        let c = text[i..]
            .chars()
            .next()
            .ok_or_else(|| error(i, "invalid token"))?;
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        if text[i..].starts_with("//") {
            i = text[i..].find('\n').map_or(text.len(), |n| i + n);
            continue;
        }
        if text[i..].starts_with("/*") {
            i += 2;
            let mut depth = 1;
            while i < text.len() && depth > 0 {
                if text[i..].starts_with("/*") {
                    depth += 1;
                    if depth > 32 {
                        return Err(error(i, "comment nesting limit exceeded"));
                    }
                    i += 2;
                } else if text[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += text[i..]
                        .chars()
                        .next()
                        .ok_or_else(|| error(i, "comment"))?
                        .len_utf8();
                }
            }
            if depth > 0 {
                return Err(error(start, "unterminated comment"));
            }
            continue;
        }
        let kind = if matches!(c, '\'' | '"' | '`') {
            i += 1;
            let mut value = String::new();
            let mut closed = false;
            while i < text.len() {
                let ch = text[i..]
                    .chars()
                    .next()
                    .ok_or_else(|| error(i, "unterminated string"))?;
                i += ch.len_utf8();
                if ch == c {
                    if c == '`' && text[i..].starts_with('`') {
                        i += 1;
                        value.push('`');
                        continue;
                    }
                    closed = true;
                    break;
                }
                if ch == '\\' {
                    let esc = text[i..]
                        .chars()
                        .next()
                        .ok_or_else(|| error(i, "unterminated escape"))?;
                    i += esc.len_utf8();
                    let v = match esc {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'b' => '\u{8}',
                        'f' => '\u{c}',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        '`' => '`',
                        'u' | 'U' => {
                            let n = if esc == 'u' { 4 } else { 8 };
                            let hex = text
                                .get(i..i + n)
                                .ok_or_else(|| error(i, "invalid Unicode escape"))?;
                            let mut code = u32::from_str_radix(hex, 16)
                                .map_err(|_| error(i, "invalid Unicode escape"))?;
                            i += n;
                            if (0xd800..=0xdbff).contains(&code) && text[i..].starts_with("\\u") {
                                i += 2;
                                let low = u32::from_str_radix(
                                    text.get(i..i + 4)
                                        .ok_or_else(|| error(i, "invalid Unicode escape"))?,
                                    16,
                                )
                                .map_err(|_| error(i, "invalid Unicode escape"))?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(error(i, "invalid Unicode surrogate"));
                                }
                                i += 4;
                                code = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                            }
                            char::from_u32(code)
                                .ok_or_else(|| error(i, "invalid Unicode code point"))?
                        }
                        _ => return Err(error(i, "unsupported string escape")),
                    };
                    value.push(v);
                } else {
                    value.push(ch);
                }
            }
            if !closed {
                return Err(error(start, "unterminated quoted string or identifier"));
            }
            if c == '`' {
                if value.is_empty() {
                    return Err(error(start, "empty identifier"));
                }
                Kind::Ident(value, true)
            } else {
                Kind::String(value)
            }
        } else if c == '$' || c == '_' || c.is_alphabetic() {
            let param = c == '$';
            if param {
                i += 1;
            }
            let name_start = i;
            let Some(first) = text.get(i..).and_then(|s| s.chars().next()) else {
                return Err(error(i, "parameter name expected"));
            };
            if first != '_' && !first.is_alphabetic() {
                return Err(error(i, "parameter name expected"));
            }
            i += first.len_utf8();
            while let Some(ch) = text.get(i..).and_then(|s| s.chars().next()) {
                if ch == '_' || ch.is_alphanumeric() {
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            let name = text[name_start..i].to_owned();
            if param {
                Kind::Param(name)
            } else {
                Kind::Ident(name, false)
            }
        } else if c.is_ascii_digit() {
            i += 1;
            while text.as_bytes().get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if text.as_bytes().get(i) == Some(&b'.')
                && text.as_bytes().get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                i += 1;
                while text.as_bytes().get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
            }
            if text
                .as_bytes()
                .get(i)
                .is_some_and(|c| matches!(c, b'e' | b'E'))
            {
                i += 1;
                if text
                    .as_bytes()
                    .get(i)
                    .is_some_and(|c| matches!(c, b'+' | b'-'))
                {
                    i += 1;
                }
                let digits = i;
                while text.as_bytes().get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                if i == digits {
                    return Err(error(start, "invalid numeric literal"));
                }
            }
            Kind::Number(text[start..i].into())
        } else {
            let mut symbol = None;
            for s in [
                "<>", "<=", ">=", "->", "<-", "(", ")", "[", "]", "{", "}", ":", ",", ".", ";",
                "=", "<", ">", "-", "+", "*", "|",
            ] {
                if text[i..].starts_with(s) {
                    symbol = Some(s);
                    break;
                }
            }
            let s = symbol.ok_or_else(|| error(i, "token is not supported yet"))?;
            i += s.len();
            Kind::Symbol(s)
        };
        out.push(Token {
            kind,
            start,
            end: i,
        });
        if out.len() > 8192 {
            return Err(CypherError::new(
                text,
                i,
                ErrorKind::Limit,
                "token limit exceeded",
            ));
        }
    }
    out.push(Token {
        kind: Kind::End,
        start: i,
        end: i,
    });
    Ok(out)
}
