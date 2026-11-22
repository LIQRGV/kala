//! Slice 7, item 00 (d), L2: a Datalog error echo shows zero-width and tag
//! characters as lowercase `\u{..}`. (Needs `must_escape` to cover them.)

use kala_datalog::parse;

#[test]
fn syntax_error_echo_escapes_zero_width_space_and_tag_chars() {
    for (c, want) in [
        ('\u{200B}', "\\u{200b}"),
        ('\u{E0041}', "\\u{e0041}"),
        ('\u{00AD}', "\\u{ad}"),
    ] {
        let q = format!("[:find ?e :where [?e :x foo{c}bar]]");
        let e = parse(&q).expect_err("bare word is a syntax error");
        for text in [e.message.clone(), e.to_string()] {
            assert!(
                !text.contains(c),
                "raw U+{:04X} echoed: {text:?}",
                u32::from(c)
            );
        }
        assert!(
            e.message.contains(want) || e.to_string().contains(want),
            "expected {want} in {:?}",
            e.message
        );
    }
}
