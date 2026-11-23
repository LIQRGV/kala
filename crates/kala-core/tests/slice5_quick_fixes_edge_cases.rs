//! Slice 5, item 0 edge cases (CONFIRM step).
//!
//! Spec (PM decision): no Cc character and no bidi control (U+202A-U+202E,
//! U+2066-U+2069) from input may appear raw in any `Display` of `Error` /
//! `FixtureError`, except the YAML parser's own layout newlines; echoed input
//! is cut at 64 chars + `...`. A YAML tag on any mapping key is refused.
use kala_core::*;
use kala_fixture::{FixtureErrorKind, load_fixture};

const BIDI: [char; 9] = [
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}',
];

const SCHEMA: &str = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, type: string }\n    - { name: ally_of, type: ref, cardinality: many }\nentities:\n  CHARACTER: [edwin, mara]\n";

fn is_bidi(c: char) -> bool {
    BIDI.contains(&c)
}

/// Strict: no control char and no bidi control at all.
fn clean(msg: &str) {
    assert!(
        !msg.chars().any(|c| c.is_control() || is_bidi(c)),
        "raw control/bidi char in message: {msg:?}"
    );
}

/// Parser-produced text: the parser's own layout newlines are tolerated.
fn clean_but_newlines(msg: &str) {
    assert!(
        !msg.chars()
            .any(|c| (c.is_control() && c != '\n') || is_bidi(c)),
        "raw control/bidi char in message: {msg:?}"
    );
}

fn fixture_err(yaml: &str) -> String {
    match load_fixture(yaml) {
        Ok(_) => panic!("fixture unexpectedly loaded:\n{yaml}"),
        Err(e) => e.to_string(),
    }
}

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

fn world() -> World {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    ));
    World::in_memory(schema).unwrap()
}

// ------------------------------------------------------------ bidi, fixture

#[test]
fn bidi_in_unknown_handle_fixture_is_escaped_for_every_bidi_char() {
    for c in BIDI {
        let esc = format!("\\u{:04x}", c as u32);
        let yaml =
            format!("{SCHEMA}operations:\n  - assert: [\"a{esc}b\", name, \"x\"]\n    tick: 1\n");
        let msg = fixture_err(&yaml);
        clean(&msg);
        assert!(msg.contains(&format!("a\\u{{{:x}}}b", c as u32)), "{msg:?}");
    }
}

#[test]
fn bidi_in_unknown_attribute_fixture_is_escaped() {
    let yaml =
        format!("{SCHEMA}operations:\n  - assert: [edwin, \"a\\u202eb\", \"x\"]\n    tick: 1\n");
    let msg = fixture_err(&yaml);
    clean(&msg);
    assert!(msg.contains("a\\u{202e}b"), "{msg:?}");
}

#[test]
fn bidi_in_duplicate_label_fixture_is_escaped() {
    let yaml = format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"a\"]\n    tick: 1\n    label: \"l\\u202ex\"\n  - assert: [edwin, name, \"b\"]\n    tick: 2\n    label: \"l\\u202ex\"\n"
    );
    let msg = fixture_err(&yaml);
    clean(&msg);
    assert!(msg.contains("l\\u{202e}x"), "{msg:?}");
}

#[test]
fn bidi_in_unknown_label_fixture_is_escaped() {
    let yaml = format!("{SCHEMA}operations:\n  - correct: \"l\\u2067x\"\n    delete: true\n");
    let msg = fixture_err(&yaml);
    clean(&msg);
    assert!(msg.contains("l\\u{2067}x"), "{msg:?}");
}

#[test]
fn bidi_in_unknown_node_type_in_entities_is_escaped() {
    let yaml = "schema:\n  node_types: [CHARACTER]\nentities:\n  \"BAD\\u202eT\": [x]\n";
    let msg = fixture_err(yaml);
    clean(&msg);
    assert!(msg.contains("BAD\\u{202e}T"), "{msg:?}");
}

#[test]
fn bidi_in_duplicate_handle_in_entities_is_escaped() {
    let yaml = "schema:\n  node_types: [CHARACTER]\nentities:\n  CHARACTER: [\"d\\u202ex\", \"d\\u202ex\"]\n";
    let msg = fixture_err(yaml);
    clean(&msg);
    assert!(msg.contains("d\\u{202e}x"), "{msg:?}");
}

// ------------------------------------------------------------ bidi, Error

#[test]
fn bidi_in_unknown_attribute_error_is_escaped() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let msg = w
        .assert(e, "a\u{202e}b", Value::Str("x".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    clean(&msg);
    assert!(msg.contains("a\\u{202e}b"), "{msg:?}");
}

#[test]
fn bidi_in_unknown_node_type_error_is_escaped() {
    let mut w = world();
    let msg = w
        .create_entity("BAD\u{202e}", None)
        .unwrap_err()
        .to_string();
    clean(&msg);
    assert!(msg.contains("BAD\\u{202e}"), "{msg:?}");
    let msg = match w.entities_of_type("BAD\u{2066}") {
        Ok(_) => panic!("unknown node type accepted"),
        Err(e) => e.to_string(),
    };
    clean(&msg);
    assert!(msg.contains("BAD\\u{2066}"), "{msg:?}");
}

#[test]
fn bidi_in_duplicate_handle_error_is_escaped() {
    let mut w = world();
    w.create_entity("CHARACTER", Some("d\u{202e}x")).unwrap();
    let msg = w
        .create_entity("CHARACTER", Some("d\u{202e}x"))
        .unwrap_err()
        .to_string();
    clean(&msg);
    assert!(msg.contains("d\\u{202e}x"), "{msg:?}");
}

#[test]
fn bidi_in_duplicate_schema_name_error_is_escaped() {
    let n = "n\u{202e}x";
    let msg = World::in_memory(Schema::new().node_type(n).node_type(n))
        .expect_err("refused")
        .to_string();
    clean(&msg);
    assert!(msg.contains("n\\u{202e}x"), "{msg:?}");
}

// ------------------------------------------------------------ bidi, parser text

#[test]
fn bidi_in_a_yaml_syntax_error_line_is_not_raw_in_the_message() {
    for yaml in [
        "schema: [\u{202e}x\n",
        "a: \u{202e}b: : :\n  - {\n",
        "\u{2066}: [unterminated\n",
    ] {
        let msg = fixture_err(yaml);
        clean_but_newlines(&msg);
    }
}

#[test]
fn bidi_in_a_wrong_type_value_is_not_raw_in_the_message() {
    let msg = fixture_err("schema: \"\u{202e}abc\"\n");
    clean_but_newlines(&msg);
    let msg = fixture_err("schema: \"\\u202eabc\"\n");
    clean_but_newlines(&msg);
}

#[test]
fn bidi_in_an_unknown_top_level_key_is_not_raw_in_the_message() {
    let msg = fixture_err("\"k\\u202ex\": 1\n");
    clean_but_newlines(&msg);
}

// ------------------------------------------------------------ UnknownEdge

fn edge_yaml(src: &str, tgt: &str) -> String {
    format!(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - {{ name: name, type: string }}\n    - {{ name: ally_of, type: ref, cardinality: many }}\nentities:\n  CHARACTER: [\"{src}\", \"{tgt}\"]\noperations:\n  - assert: [[\"{src}\", ally_of, \"{tgt}\"], name, \"x\"]\n    tick: 1\n"
    )
}

#[test]
fn unknown_edge_with_esc_in_handles_is_escaped() {
    let msg = fixture_err(&edge_yaml("a\\x1b[31m", "b\\x1bc"));
    assert!(msg.contains("no edge"), "{msg:?}");
    clean(&msg);
    assert!(msg.contains("a\\u{1b}[31m"), "{msg:?}");
}

#[test]
fn unknown_edge_with_bidi_in_handles_is_escaped() {
    let msg = fixture_err(&edge_yaml("a\\u202eb", "c\\u2069d"));
    assert!(msg.contains("no edge"), "{msg:?}");
    clean(&msg);
    assert!(msg.contains("a\\u{202e}b"), "{msg:?}");
}

#[test]
fn unknown_edge_with_very_long_handles_is_bounded() {
    let (s, t) = ("S".repeat(200), "T".repeat(200));
    let msg = fixture_err(&edge_yaml(&s, &t));
    assert!(msg.contains("no edge"), "{msg:?}");
    assert!(msg.ends_with("..."), "{msg:?}");
    assert!(
        !msg.contains('T'),
        "target is past the 64-char cut: {msg:?}"
    );
    assert!(msg.matches('S').count() <= 64, "{msg:?}");
    assert!(
        msg.chars().count() < 200,
        "{} chars: {msg:?}",
        msg.chars().count()
    );
}

#[test]
fn unknown_edge_as_a_ref_value_is_escaped_and_bounded() {
    let yaml = format!(
        "{SCHEMA}operations:\n  - assert: [edwin, ally_of, [edwin, \"x\\x1b{}\", mara]]\n    tick: 1\n",
        "Y".repeat(200)
    );
    // `x<ESC>YYY…` is not an attribute: the edge lookup yields nothing.
    let msg = fixture_err(&yaml);
    clean(&msg);
    assert!(
        msg.chars().count() < 300,
        "{} chars: {msg:?}",
        msg.chars().count()
    );
}

// ------------------------------------------------------------ M1 positions

#[test]
fn m1_tagged_key_inside_replace_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"a\"]\n    tick: 1\n    label: l\n  - correct: l\n    replace: {{ !!str tick: -280 }}\n"
    ));
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"a\"]\n    tick: 1\n    label: l\n  - correct: l\n    replace:\n      !!str tick: -280\n"
    ));
}

#[test]
fn m1_tagged_key_inside_entities_is_refused() {
    refused("schema:\n  node_types: [CHARACTER]\nentities:\n  !!str CHARACTER: [edwin]\n");
    refused("schema:\n  node_types: [CHARACTER]\nentities: { !!str CHARACTER: [edwin] }\n");
}

#[test]
fn m1_tagged_key_inside_schema_attribute_and_node_types_maps_is_refused() {
    refused("schema:\n  !!str node_types: [CHARACTER]\nentities:\n  CHARACTER: [edwin]\n");
    refused(
        "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, !!str type: string }\n",
    );
}

#[test]
fn m1_non_specific_tag_on_a_key_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - ! assert: [edwin, name, \"x\"]\n    tick: 1\n"
    ));
    refused(&format!(
        "{SCHEMA}operations:\n  - {{ ! assert: [edwin, name, \"x\"], tick: 1 }}\n"
    ));
}

#[test]
fn m1_tag_on_a_key_in_flow_and_in_block_mapping_is_refused_alike() {
    // The tagged key is the second key in both forms.
    refused(&format!(
        "{SCHEMA}operations:\n  - {{ assert: [edwin, name, \"x\"], !!str tick: 1 }}\n"
    ));
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"x\"]\n    !!str tick: 1\n"
    ));
}

#[test]
fn m1_custom_local_tag_on_a_key_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - !mine assert: [edwin, name, \"x\"]\n    tick: 1\n"
    ));
    refused(&format!(
        "{SCHEMA}operations:\n  - !<tag:yaml.org,2002:str> assert: [edwin, name, \"x\"]\n    tick: 1\n"
    ));
}

#[test]
fn m1_tagged_key_in_complex_key_syntax_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - ? !!str assert\n    : [edwin, name, \"x\"]\n    tick: 1\n"
    ));
}

#[test]
fn m1_key_reached_through_an_alias_of_a_tagged_scalar_is_refused() {
    // `label` anchors a `!!binary` scalar ("assert"); the next op uses the
    // alias as a key, so that key would decode to "assert".
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"a\"]\n    tick: 1\n    label: &k !!binary YXNzZXJ0\n  - *k : [edwin, name, \"b\"]\n    tick: 2\n"
    ));
}

#[test]
fn m1_anchored_tagged_key_then_alias_key_is_refused() {
    refused(&format!(
        "{SCHEMA}operations:\n  - &k !!str assert: [edwin, name, \"a\"]\n    tick: 1\n  - *k : [edwin, name, \"b\"]\n    tick: 2\n"
    ));
}

#[test]
fn m1_tagged_key_inside_a_value_that_is_ignored_or_nested_is_refused() {
    // Tagged key nested in a value position (the `name` value is a scalar in
    // a correct op, but nested mappings elsewhere must still be scanned).
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, \"x\"]\n    tick: 1\n    label: l\n  - correct: l\n    replace: {{ value: {{ !!str a: 1 }} }}\n"
    ));
}

#[test]
fn m1_tags_on_values_stay_refused_and_untagged_keys_load() {
    // Pre-existing behavior (guard): explicit tags are refused on values too.
    refused(&format!(
        "{SCHEMA}operations:\n  - assert: [edwin, name, !!str \"x\"]\n    tick: 1\n"
    ));
    let ok = format!("{SCHEMA}operations:\n  - assert: [edwin, name, \"x\"]\n    tick: 1\n");
    assert!(load_fixture(&ok).is_ok());
}

// ------------------------------------------------------------ quoting

#[test]
fn quote_and_backslash_in_an_unknown_attribute_are_escaped_in_error() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let msg = w
        .assert(e, "a\"b\\c", Value::Str("x".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("\"a\\\"b\\\\c\""), "{msg:?}");
}

#[test]
fn a_backslash_before_the_closing_quote_cannot_swallow_it() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let msg = w
        .assert(e, "x\\", Value::Str("v".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    assert!(msg.contains("\"x\\\\\""), "{msg:?}");
}

#[test]
fn an_attribute_that_mimics_a_second_quoted_field_stays_distinguishable() {
    let mut w = world();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let plain = w
        .assert(e, "ab", Value::Str("v".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    let tricky = w
        .assert(e, "a\" \"b", Value::Str("v".into()), Some(Tick(1)))
        .unwrap_err()
        .to_string();
    assert_ne!(plain, tricky);
    assert!(tricky.contains("\"a\\\" \\\"b\""), "{tricky:?}");
}
