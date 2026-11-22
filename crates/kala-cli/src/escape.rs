//! The one escaper for every piece of human-readable text the CLI prints.
//!
//! Output goes to terminals, so nothing that came from input (argv, paths,
//! fixture or world data, kala-core and clap messages echoing them) may carry
//! raw control characters (an ANSI escape could repaint the screen) or bidi
//! controls (which reorder the text around them). Escaped characters are
//! shown in their `char::escape_debug` form (`\u{1b}`, `\u{9b}`, `\n`), the
//! same form kala-core's error messages use; bidi controls as `\u{202e}`.
//! In entity labels (handles) a `\` is doubled too, so an escaped control
//! can't be mistaken for the literal text of one.

use std::fmt::Write;

/// The most chars of an echoed user value shown; longer values are cut and
/// get `...`.
pub const MAX_VALUE_CHARS: usize = 200;

/// The bidi embedding/override (U+202A..U+202E) and isolate (U+2066..U+2069)
/// controls: format chars, not control chars, but they reorder text.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether `c` is escaped. `layout` keeps `\n` and `\t` (the CLI's own line
/// layout); otherwise they are escaped too.
fn needs_escape(c: char, layout: bool) -> bool {
    if layout && matches!(c, '\n' | '\t') {
        return false;
    }
    // `is_control` is Unicode `Cc`: C0, DEL and C1 (U+0080..U+009F).
    c.is_control() || is_bidi_control(c)
}

fn push_escaped(out: &mut String, c: char) {
    if c.is_control() {
        // Infallible: writing to a String.
        let _ = write!(out, "{}", c.escape_debug());
    } else {
        let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
    }
}

/// Escapes text the CLI lays out itself (messages, help, rendered errors):
/// every control and bidi char except `\n` and `\t`. Not truncated.
pub fn text(s: &str) -> String {
    escape_all(s, true)
}

/// Every char of `s` escaped per [`needs_escape`], nothing cut.
fn escape_all(s: &str, layout: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if needs_escape(c, layout) {
            push_escaped(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Escapes one echoed user value (a path, an argument, a world string;
/// entity labels go through [`label`]):
/// every control and bidi char, newlines and tabs included, cut to
/// [`MAX_VALUE_CHARS`] chars plus `...`.
pub fn value(s: &str) -> String {
    cut_escaped(s, false)
}

/// Escapes an entity label (a handle, a name or an ID) for text: as
/// [`value`], and `\` doubled, so a label holding the literal text
/// `\u{1b}` reads differently from one holding a real ESC (shown as
/// `\u{1b}`). JSON shows the label raw.
pub fn label(s: &str) -> String {
    cut_escaped(s, true)
}

/// [`value`], with `\` doubled if `backslash`.
fn cut_escaped(s: &str, backslash: bool) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_VALUE_CHARS * 4));
    let mut chars = s.chars();
    for c in chars.by_ref().take(MAX_VALUE_CHARS) {
        if backslash && c == '\\' {
            out.push_str("\\\\");
        } else if needs_escape(c, false) {
            push_escaped(&mut out, c);
        } else {
            out.push(c);
        }
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

/// Escapes text for one output line: every control and bidi char,
/// newlines and tabs included, like [`value`] but not truncated (for
/// kala-core's messages, which already cut the input they echo).
pub fn line(s: &str) -> String {
    escape_all(s, false)
}

/// Whether [`value`] would change `s`.
pub fn value_needs_escape(s: &str) -> bool {
    s.chars().any(|c| needs_escape(c, false)) || s.chars().nth(MAX_VALUE_CHARS).is_some()
}

/// A path for display: [`value`] of its lossy `display()` form.
pub fn path(p: &std::path::Path) -> String {
    value(&p.display().to_string())
}

/// A world string for display, in double quotes: every control and bidi
/// char escaped (newlines and tabs included), and `"` and `\` escaped so
/// the quotes stay unambiguous. Not truncated: this is the data the user
/// asked to read.
pub fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if needs_escape(c, false) => push_escaped(&mut out, c),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Whether a JSON writer must emit `c` as a `\u` escape: every control and
/// bidi char. (JSON itself only requires it below U+0020; C1 controls, DEL
/// and bidi controls are escaped too so `--json` is terminal-safe.)
pub fn json_needs_escape(c: char) -> bool {
    needs_escape(c, false)
}
