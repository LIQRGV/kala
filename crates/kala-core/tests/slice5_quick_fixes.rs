//! Slice 5, item 0 quick fixes (RED step).
//!
//!  (a) slice-4 finding M1: a YAML tag on a mapping key (`!!binary YXNzZXJ0:`,
//!      `!!str assert:`, ...) must be refused (`Yaml` or `Invalid`), writing nothing.
//!  (c) slice-4 finding L2: user-supplied strings echoed in `Error` /
//!      `FixtureError` messages have control chars (Unicode Cc) escaped
//!      (`\u{1b}`, `\n`) and are cut to their first 64 chars plus `...`.
//!      Printable non-ASCII is kept. Only `Display` is asserted, never payloads.
use kala_core::*;
use kala_fixture::{FixtureErrorKind, load_fixture};

// ------------------------------------------------------------------ (a)

const SCHEMA: &str = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, type: string }\nentities:\n  CHARACTER: [edwin]\n";

fn refused(yaml: &str) {
    match load_fixture(yaml) {
        Ok(_) => panic!("a tagged key was accepted:\n{yaml}"),
        Err(e) => assert!(
            matches!(
                e.kind,
                FixtureErrorKind::Yaml(_) | FixtureErrorKind::Invalid(_)
            ),
            "wrong error kind for\n{yaml}\n{e:?}"
        ),
    }
}

#[test]
fn m1_binary_tagged_op_key_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - {{ !!binary YXNzZXJ0: [edwin, name, \"Edwin\"], tick: 1 }}\n"
    ));
}

#[test]
fn m1_str_tagged_op_key_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - !!str assert: [edwin, name, \"Edwin\"]\n    tick: 1\n"
    ));
}

#[test]
fn m1_tagged_op_key_inside_transactions_is_refused() {
    refused(&format!(
        "{SCHEMA}transactions:\n  - ops:\n      - {{ !!str assert: [edwin, name, \"Edwin\"], tick: 1 }}\n"
    ));
}

#[test]
fn m1_tagged_top_level_key_is_refused() {
    refused(
        "!!str schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, type: string }\nentities:\n  CHARACTER: [edwin]\n",
    );
}

#[test]
fn m1_tagged_schema_attribute_field_name_is_refused() {
    refused(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { !!str name: name, type: string }\nentities:\n  CHARACTER: [edwin]\n",
    );
}

// ------------------------------------------------------------------ (c)

fn no_control_chars(msg: &str) {
    assert!(
        !msg.chars().any(|c| c.is_control()),
        "raw control char in message: {msg:?}"
    );
}

fn fixture_err(yaml: &str) -> String {
    match load_fixture(yaml) {
        Ok(_) => panic!("fixture unexpectedly loaded:\n{yaml}"),
        Err(e) => e.to_string(),
    }
}

/// A fixture that asserts on handle `h` (a YAML double-quoted scalar body,
/// so `\x1b`, `\n` escapes are YAML escapes) that was never created.
fn unknown_handle_fixture(h_yaml_escaped: &str) -> String {
    format!("{SCHEMA}operations:\n  - assert: [\"{h_yaml_escaped}\", name, \"x\"]\n    tick: 1\n")
}

#[test]
fn l2_unknown_handle_escapes_esc() {
    let msg = fixture_err(&unknown_handle_fixture("\\x1b[31mred"));
    no_control_chars(&msg);
    assert!(msg.contains("\\u{1b}[31mred"), "{msg:?}");
}

#[test]
fn l2_unknown_handle_escapes_newline() {
    let msg = fixture_err(&unknown_handle_fixture("a\\nb\\u007fc\\x85d"));
    no_control_chars(&msg);
    assert!(msg.contains("a\\nb"), "{msg:?}");
    assert!(
        msg.contains("\\u{7f}") && msg.contains("\\u{85}"),
        "{msg:?}"
    );
}

#[test]
fn l2_unknown_handle_keeps_printable_non_ascii() {
    let msg = fixture_err(&unknown_handle_fixture("Ærendil東"));
    assert!(msg.contains("Ærendil東"), "{msg:?}");
}

#[test]
fn l2_unknown_handle_200_chars_is_truncated_to_64_plus_dots() {
    let h = format!("{}{}", "X".repeat(64), "Y".repeat(136));
    let msg = fixture_err(&unknown_handle_fixture(&h));
    assert!(msg.contains(&format!("{}...", "X".repeat(64))), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_unknown_handle_of_exactly_64_chars_is_not_truncated() {
    let h = "X".repeat(64);
    let msg = fixture_err(&unknown_handle_fixture(&h));
    assert!(msg.contains(&h), "{msg:?}");
    assert!(!msg.contains("..."), "{msg:?}");
}

#[test]
fn l2_truncation_counts_chars_not_bytes() {
    // 65 chars of 3 bytes each: cut after 64 chars.
    let h = format!("{}{}", "東".repeat(64), "Y");
    let msg = fixture_err(&unknown_handle_fixture(&h));
    assert!(msg.contains(&format!("{}...", "東".repeat(64))), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_escaped_and_long_input_is_cut_by_original_char_count() {
    // 64 ESC chars is exactly the limit: all of them are shown, escaped, no dots.
    let h = "\\x1b".repeat(64);
    let msg = fixture_err(&unknown_handle_fixture(&h));
    no_control_chars(&msg);
    assert_eq!(msg.matches("\\u{1b}").count(), 64, "{msg:?}");
    assert!(!msg.contains("..."), "{msg:?}");
}

#[test]
fn l2_unknown_label_in_fixture_is_escaped_and_truncated() {
    let l = format!("\\x1b{}{}", "X".repeat(63), "Y".repeat(50));
    let yaml = format!("{SCHEMA}operations:\n  - correct: \"{l}\"\n    delete: true\n");
    let msg = fixture_err(&yaml);
    no_control_chars(&msg);
    assert!(
        msg.contains(&format!("\\u{{1b}}{}...", "X".repeat(63))),
        "{msg:?}"
    );
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_duplicate_label_in_fixture_is_escaped() {
    let yaml = format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"a\"]\n    tick: 1\n    label: \"l\\x1bx\"\n  - assert: [edwin, name, \"b\"]\n    tick: 2\n    label: \"l\\x1bx\"\n"
    );
    let msg = fixture_err(&yaml);
    no_control_chars(&msg);
    assert!(msg.contains("l\\u{1b}x"), "{msg:?}");
}

#[test]
fn l2_unknown_attribute_in_fixture_is_escaped_and_truncated() {
    let a = format!("a\\x1b\\n{}{}", "X".repeat(70), "Y".repeat(10));
    let yaml = format!("{SCHEMA}operations:\n  - assert: [edwin, \"{a}\", \"x\"]\n    tick: 1\n");
    let msg = fixture_err(&yaml);
    no_control_chars(&msg);
    assert!(msg.contains("a\\u{1b}\\n"), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_unknown_node_type_in_fixture_entities_is_escaped() {
    let yaml = "schema:\n  node_types: [CHARACTER]\nentities:\n  \"BAD\\x1bT\": [x]\n";
    let msg = fixture_err(yaml);
    no_control_chars(&msg);
    assert!(msg.contains("BAD\\u{1b}T"), "{msg:?}");
}

#[test]
fn l2_duplicate_handle_in_fixture_is_escaped_and_truncated() {
    let h = format!("d\\x1b\\n{}{}", "X".repeat(70), "Y".repeat(10));
    let yaml =
        format!("schema:\n  node_types: [CHARACTER]\nentities:\n  CHARACTER: [\"{h}\", \"{h}\"]\n");
    let msg = fixture_err(&yaml);
    no_control_chars(&msg);
    assert!(msg.contains("d\\u{1b}\\nXXX"), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_unknown_node_type_in_fixture_entities_is_truncated() {
    let t = format!("{}{}", "X".repeat(64), "Y".repeat(20));
    let yaml = format!("schema:\n  node_types: [CHARACTER]\nentities:\n  \"{t}\": [x]\n");
    let msg = fixture_err(&yaml);
    assert!(msg.contains(&format!("{}...", "X".repeat(64))), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

/// GUARD (passes today): the YAML parser already sanitizes the control chars
/// it echoes; its own multi-line layout uses real newlines, which are the
/// parser's, not user input, so they are tolerated here (see report).
#[test]
fn l2_yaml_syntax_error_has_no_raw_control_chars_besides_parser_layout_newlines() {
    for yaml in [
        "schema: [\u{1b}\u{7}x\n",
        "a: \u{0}\u{1b}[31m: : :\n  - {\n",
        "\u{1b}\u{9b}: [unterminated\n",
    ] {
        match load_fixture(yaml) {
            Ok(_) => panic!("loaded {yaml:?}"),
            Err(e) => {
                let m = e.to_string();
                assert!(!m.chars().any(|c| c.is_control() && c != '\n'), "{m:?}");
            }
        }
    }
}

/// Shape errors (`Invalid`) echo user text too: unknown keys and bad scalars.
#[test]
fn l2_invalid_unknown_top_level_key_is_escaped_and_truncated() {
    let k = format!("k\\x1b\\n{}{}", "X".repeat(70), "Y".repeat(10));
    let msg = fixture_err(&format!("\"{k}\": 1\n"));
    // (the parser's own multi-line layout newlines are tolerated)
    assert!(!msg.chars().any(|c| c.is_control() && c != '\n'), "{msg:?}");
    assert!(msg.contains("XXX"), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_yaml_error_message_is_bounded() {
    let yaml = format!("schema: [{}\n", "Z".repeat(5000));
    let msg = fixture_err(&yaml);
    assert!(msg.chars().count() < 1000, "{} chars", msg.chars().count());
}

// ------------------------------------------------------------------ (c) Error

fn world() -> World {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    World::in_memory(schema).unwrap()
}

#[test]
fn l2_error_unknown_attribute_escapes_esc_and_newline() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let err = w
        .assert(
            e,
            "bad\u{1b}[31m\nattr",
            Value::Str("x".into()),
            Some(Tick(1)),
        )
        .unwrap_err();
    let msg = err.to_string();
    no_control_chars(&msg);
    assert!(msg.contains("bad\\u{1b}[31m\\nattr"), "{msg:?}");
}

#[test]
fn l2_error_unknown_attribute_truncates() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let a = format!("{}{}", "X".repeat(64), "Y".repeat(100));
    let msg = w
        .assert(e, &a, Value::Str("x".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    assert!(msg.contains(&format!("{}...", "X".repeat(64))), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_error_unknown_attribute_exactly_64_chars_not_truncated_and_non_ascii_kept() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let a = "Æ".repeat(64);
    let msg = w
        .assert(e, &a, Value::Str("x".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    assert!(msg.contains(&a), "{msg:?}");
    assert!(!msg.contains("..."), "{msg:?}");
}

#[test]
fn l2_error_unknown_node_type_via_entities_of_type() {
    let w = world();
    let msg = match w.entities_of_type("BAD\u{1b}") {
        Ok(_) => panic!("unknown node type accepted"),
        Err(e) => e.to_string(),
    };
    no_control_chars(&msg);
    assert!(msg.contains("BAD\\u{1b}"), "{msg:?}");
}

#[test]
fn l2_error_unknown_node_type_via_create_entity_truncates() {
    let mut w = world();
    let t = format!("{}{}", "X".repeat(64), "Y".repeat(20));
    let msg = w.create_entity(&t, None).unwrap_err().to_string();
    assert!(msg.contains(&format!("{}...", "X".repeat(64))), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_error_duplicate_handle_is_escaped_and_truncated() {
    let mut w = world();
    let h = format!("d\u{1b}\n{}{}", "X".repeat(70), "Y".repeat(10));
    w.create_entity("CHARACTER", Some(&h)).unwrap();
    let msg = w
        .create_entity("CHARACTER", Some(&h))
        .unwrap_err()
        .to_string();
    no_control_chars(&msg);
    assert!(msg.contains("d\\u{1b}\\nXXX"), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}

#[test]
fn l2_error_duplicate_schema_name_is_escaped_and_truncated() {
    let n = format!("n\u{1b}\n{}{}", "X".repeat(70), "Y".repeat(10));
    let schema = Schema::new().node_type(&n).node_type(&n);
    let msg = World::in_memory(schema).expect_err("refused").to_string();
    no_control_chars(&msg);
    assert!(msg.contains("n\\u{1b}\\nXXX"), "{msg:?}");
    assert!(!msg.contains('Y'), "{msg:?}");
}
