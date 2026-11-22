//! Slice 6 item 00, L1 (predicate half): one shared public predicate for
//! the characters that must never be echoed raw.
//!
//! Expected API: `kala_core::must_escape(c: char) -> bool`, true for every
//! Unicode `Cc` (C0, DEL, C1), the bidi embeddings/overrides U+202A..U+202E,
//! the bidi isolates U+2066..U+2069, and also U+061C, U+200E, U+200F,
//! U+2028, U+2029, U+FEFF. False for everything else.
//!
//! Kept in its own file: until `must_escape` exists this file fails to
//! compile, and the other slice 6 tests (`slice6_followups.rs`) must still
//! compile and fail on assertions.

use kala_core::must_escape;

#[test]
fn every_cc_char_must_escape() {
    for c in ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}') {
        assert!(must_escape(c), "{:?}", c);
    }
    // tab and newline are Cc too
    assert!(must_escape('\t') && must_escape('\n') && must_escape('\r'));
}

#[test]
fn bidi_controls_must_escape() {
    for c in ('\u{202A}'..='\u{202E}').chain('\u{2066}'..='\u{2069}') {
        assert!(must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn the_added_invisible_and_separator_chars_must_escape() {
    for c in [
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{2028}', '\u{2029}', '\u{FEFF}',
    ] {
        assert!(must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn ordinary_text_does_not_need_escaping() {
    for c in [
        ' ',
        'a',
        'Z',
        '0',
        '~',
        '"',
        '\\',
        '\u{A0}',
        '\u{A1}',
        'é',
        'ß',
        '漢',
        '\u{1F600}',
        '\u{200D}',
        '\u{2027}',
        '\u{202F}',
        '\u{2065}',
        '\u{061B}',
        '\u{FEFE}',
        '\u{FF00}',
    ] {
        assert!(!must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn the_boundaries_of_each_range_are_exact() {
    assert!(!must_escape('\u{20}') && must_escape('\u{1f}'));
    assert!(must_escape('\u{7f}') && !must_escape('\u{7e}'));
    assert!(must_escape('\u{9f}') && !must_escape('\u{a0}'));
    assert!(!must_escape('\u{202F}') && !must_escape('\u{2065}'));
}
