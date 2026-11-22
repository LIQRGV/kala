//! Slice 7, item 00 (d), L2: `must_escape` also covers zero-width and
//! invisible formatting characters.
//!
//! Expected: `kala_core::must_escape(c)` is also true for U+200B,
//! U+2060..=U+2064, U+206A..=U+206F, U+180E, U+00AD, U+FFF9..=U+FFFB and
//! U+E0000..=U+E007F; still false for letters, digits, spaces, emoji, CJK.

use kala_core::*;

fn newly_escaped() -> Vec<char> {
    let mut v = vec!['\u{200B}', '\u{180E}', '\u{00AD}'];
    v.extend('\u{2060}'..='\u{2064}');
    v.extend('\u{206A}'..='\u{206F}');
    v.extend('\u{FFF9}'..='\u{FFFB}');
    v.extend('\u{E0000}'..='\u{E007F}');
    v
}

#[test]
fn invisible_and_tag_characters_must_escape() {
    for c in newly_escaped() {
        assert!(must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn neighbours_of_the_new_ranges_stay_unescaped() {
    for c in [
        '\u{00AC}',
        '\u{00AE}',
        '\u{200A}',
        '\u{200C}',
        '\u{200D}',
        '\u{2065}',
        '\u{FFF8}',
        '\u{FFFC}',
        '\u{E0080}',
        '\u{DFFFF}',
    ] {
        assert!(!must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn ordinary_text_still_does_not_need_escaping() {
    for c in [
        ' ',
        'a',
        'Z',
        '0',
        '9',
        '~',
        'é',
        'ß',
        '漢',
        'ひ',
        'Ω',
        '\u{1F600}',
        '\u{1F468}',
        '\u{A0}',
        '\u{200D}',
        '\u{FE0F}',
    ] {
        assert!(!must_escape(c), "U+{:04X}", u32::from(c));
    }
}

#[test]
fn a_core_error_echo_escapes_the_new_characters() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let e = w.create_entity("CHARACTER", None).unwrap();
    for (c, want) in [
        ('\u{200B}', "\\u{200b}"),
        ('\u{00AD}', "\\u{ad}"),
        ('\u{2060}', "\\u{2060}"),
        ('\u{E0041}', "\\u{e0041}"),
    ] {
        let attr = format!("a{c}b");
        let text = w.as_of(e, &attr, Tick(0)).unwrap_err().to_string();
        assert!(
            !text.contains(c),
            "raw U+{:04X} echoed: {text:?}",
            u32::from(c)
        );
        assert!(text.contains(&format!("a{want}b")), "{want} in {text:?}");
        w.create_entity("CHARACTER", Some(&attr))
            .expect("a handle with an invisible character is valid");
        let text = w
            .create_entity("CHARACTER", Some(&attr))
            .unwrap_err()
            .to_string();
        assert!(!text.contains(c), "handle: {text:?}");
    }
}
