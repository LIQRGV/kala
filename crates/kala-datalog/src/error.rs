//! Datalog errors: a kind, an optional span into the query text, and a
//! message. Parse errors are always located (line and column known).

use std::fmt::{self, Write as _};

use kala_core::must_escape;

/// A byte range `start..end` into the query text. Both ends are on char
/// boundaries and within the text the span was produced from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    /// Byte offset of the first byte.
    pub start: usize,
    /// Byte offset just past the last byte.
    pub end: usize,
}

impl Span {
    /// A span from `start` to `end` (byte offsets).
    pub fn new(start: usize, end: usize) -> Span {
        Span { start, end }
    }
}

/// What went wrong with a Datalog query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The text is not in the grammar.
    Syntax,
    /// A resource limit was exceeded (size, nesting depth, pattern count).
    Limit,
    /// An attribute name not in the schema.
    UnknownAttribute,
    /// A handle or ID constant that names no entity.
    UnknownEntity,
    /// A variable used where it must already be bound, but isn't.
    UnboundVariable,
    /// Values of different types meet (constant, variable or predicate).
    TypeMismatch,
    /// In the grammar's future but not implemented yet.
    Unsupported,
    /// A rule called with the wrong number of arguments, or definitions of
    /// one rule name with different head lengths.
    Arity,
    /// A call of a rule no `:rules` definition names.
    UnknownRule,
    /// A `(not ..)` reaches, through its calls, a rule of the recursive
    /// component of the rule containing it: negation through recursion has
    /// no stratified meaning.
    Stratification,
}

/// A Datalog parse or compile error.
///
/// `Display` prints `line L, column C: message` (1-based, columns counted in
/// chars) once the error has been located in its source text; errors from
/// [`crate::parse`] always are. An unlocated error with a span prints
/// `bytes S..E: message`; one without a span prints just the message.
#[derive(Clone, Debug)]
pub struct DatalogError {
    /// The error class.
    pub kind: ErrorKind,
    /// Where in the query text, when known.
    pub span: Option<Span>,
    /// Human-readable description. Any input it echoes is escaped.
    pub message: String,
    /// `(line, column)` of `span.start`, filled in by [`DatalogError::locate`].
    line_col: Option<(usize, usize)>,
}

impl DatalogError {
    /// A new error. It is not located yet; see [`DatalogError::locate`].
    pub fn new(kind: ErrorKind, span: Option<Span>, message: impl Into<String>) -> DatalogError {
        DatalogError {
            kind,
            span,
            message: message.into(),
            line_col: None,
        }
    }

    pub(crate) fn syntax(span: Span, message: impl Into<String>) -> DatalogError {
        DatalogError::new(ErrorKind::Syntax, Some(span), message)
    }

    pub(crate) fn limit(span: Option<Span>, message: impl Into<String>) -> DatalogError {
        DatalogError::new(ErrorKind::Limit, span, message)
    }

    pub(crate) fn unsupported(span: Span, message: impl Into<String>) -> DatalogError {
        DatalogError::new(ErrorKind::Unsupported, Some(span), message)
    }

    /// The same error with its span moved `offset` bytes later, for when the
    /// text it was produced from sits at `offset` inside a larger text (a
    /// check's query inside its checks file). Any location is dropped, since
    /// it described the smaller text; call [`DatalogError::locate`] with the
    /// larger text afterwards. A shift that would overflow leaves the span
    /// unset rather than panicking.
    pub fn offset_by(mut self, offset: usize) -> DatalogError {
        self.span = self.span.and_then(|s| {
            Some(Span::new(
                s.start.checked_add(offset)?,
                s.end.checked_add(offset)?,
            ))
        });
        self.line_col = None;
        self
    }

    /// Compute the line and column of the span in `text` (the text the span
    /// was produced from), so `Display` can show them. A span that doesn't
    /// fit `text` is left unlocated rather than panicking.
    pub fn locate(mut self, text: &str) -> DatalogError {
        if let Some(span) = self.span
            && let Some(before) = text.get(..span.start)
        {
            // One linear pass, done once per error.
            let line = 1 + before.bytes().filter(|&b| b == b'\n').count();
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            let column = 1 + before[line_start..].chars().count();
            self.line_col = Some((line, column));
        }
        self
    }
}

impl fmt::Display for DatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line_col, self.span) {
            (Some((line, column)), _) => {
                write!(f, "line {line}, column {column}: {}", self.message)
            }
            (None, Some(s)) => write!(f, "bytes {}..{}: {}", s.start, s.end, self.message),
            (None, None) => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DatalogError {}

/// Longest piece of input echoed in a message, in chars.
const ECHO_CHARS: usize = 40;

/// Input text made safe to echo in a message: every [`must_escape`] char is
/// escaped in the same form as kala-core's error echo (`Cc` as
/// `char::escape_debug` does: `\u{1b}`, `\n`; the rest as lowercase
/// `\u{202e}`), and long text is cut short with `…`.
pub(crate) fn show(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == ECHO_CHARS {
            out.push('…');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_debug());
        } else if must_escape(c) {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    out
}
