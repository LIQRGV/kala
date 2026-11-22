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

/// Shows `self.0` with every [`must_escape`] char escaped (`Cc` as
/// `char::escape_debug` does: `\u{1b}`, `\n`; the rest as `\u{202e}`), cut to
/// its first [`MAX_ECHO_CHARS`] chars (counted in the original string) plus `...`.
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

/// Whether `c` must never be echoed raw to a terminal: every Unicode `Cc`
/// (C0, DEL, C1), the bidi embeddings/overrides U+202A..U+202E and isolates
/// U+2066..U+2069, the bidi marks U+061C (ALM), U+200E (LRM) and U+200F (RLM),
/// the line/paragraph separators U+2028 and U+2029, U+FEFF (BOM / ZWNBSP),
/// and the invisible format chars: U+200B (ZWSP), U+2060..U+2064 (word
/// joiner, invisible operators), U+206A..U+206F (deprecated format
/// controls), U+180E (Mongolian vowel separator), U+00AD (soft hyphen),
/// U+FFF9..U+FFFB (interlinear annotation) and the tag characters
/// U+E0000..U+E007F. ZWJ/ZWNJ (U+200C, U+200D) stay raw: emoji and many
/// scripts need them.
///
/// The non-`Cc` ones are format or separator chars that reorder, hide or
/// break the text around them. This is the one predicate shared by
/// kala-core's error messages and every front-end (the CLI) that prints
/// input or world data; escape a char it accepts as `\u{..}`.
pub fn must_escape(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                | '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{FEFF}'
                | '\u{200B}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{180E}'
                | '\u{00AD}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

fn write_echo(f: &mut fmt::Formatter<'_>, s: &str, quoted: bool) -> fmt::Result {
    let mut chars = s.chars();
    for c in chars.by_ref().take(MAX_ECHO_CHARS) {
        if c.is_control() {
            write!(f, "{}", c.escape_debug())?;
        } else if must_escape(c) {
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
