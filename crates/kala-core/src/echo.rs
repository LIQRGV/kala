//! Echoing user-supplied strings in error messages.
//!
//! Error messages end up on terminals, so a handle, attribute or label taken
//! from input must not carry raw control characters (an ANSI escape could
//! repaint the screen) or bidi overrides (which can reorder the text around
//! them), and must not be unbounded. Every `Display` arm that echoes input
//! goes through [`Echo`] or [`Quoted`]; the error payloads keep the raw
//! strings.

use std::fmt::{self, Write};

/// The most chars of an input shown; longer input is cut and gets `...`.
pub(crate) const MAX_ECHO_CHARS: usize = 64;

/// Shows `self.0` with control characters (Unicode `Cc`) and bidi controls
/// escaped like `char::escape_debug` (`\u{1b}`, `\n`), cut to its first
/// [`MAX_ECHO_CHARS`] chars (counted in the original string) plus `...`.
/// Printable text, non-ASCII included, is shown as is.
pub(crate) struct Echo<'a>(pub(crate) &'a str);

/// [`Echo`] inside double quotes, with `"` and `\` also escaped, so plain
/// input reads exactly as its `Debug` form would.
pub(crate) struct Quoted<'a>(pub(crate) &'a str);

impl fmt::Display for Echo<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_echo(f, self.0, false)
    }
}

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_char('"')?;
        write_echo(f, self.0, true)?;
        f.write_char('"')
    }
}

/// The bidi embedding/override (U+202A..U+202E) and isolate (U+2066..U+2069)
/// controls: format chars (`Cf`), not `Cc`, but they reorder displayed text.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn write_echo(f: &mut fmt::Formatter<'_>, s: &str, quoted: bool) -> fmt::Result {
    let mut chars = s.chars();
    for c in chars.by_ref().take(MAX_ECHO_CHARS) {
        if c.is_control() {
            write!(f, "{}", c.escape_debug())?;
        } else if is_bidi_control(c) {
            write!(f, "\\u{{{:x}}}", u32::from(c))?;
        } else if quoted && matches!(c, '"' | '\\') {
            write!(f, "\\{c}")?;
        } else {
            f.write_char(c)?;
        }
    }
    if chars.next().is_some() {
        f.write_str("...")?;
    }
    Ok(())
}
