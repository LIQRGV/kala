//! Slice 5 items 3, 4, 7 (confirm step): edge cases of the read commands.
//!
//! Decisions encoded here:
//! - a lone `@T` in `asof`'s attribute slot is the tick, not an attribute;
//! - `edge` takes `edge#N [@T]` or `SRC ATTR TGT [@T]`; other arity is exit 2;
//! - `--at +5` and `@+5` are accepted and mean 5 (i64 parse semantics);
//! - an edge defined after the chosen canon does not exist at that canon (exit 1);
//! - handles are shown escaped, with the backslash escaped too (`\\`), so a
//!   literal `\u{1b}` text and a real ESC are distinguishable;
//! - long string values are shown in full in text and JSON.

mod common;

use common::*;
use kala_core::{Direction, EntityId, Value};
use serde_json::json;

const SCHEMA_HEAD: &str = "schema:\n  node_types: [CHARACTER, PLACE]\n";

// ---------------------------------------------------------------------------
// `asof <e> <attr> @T` vs `asof <e> @T`
// ---------------------------------------------------------------------------

#[test]
fn asof_lone_tick_in_the_attr_slot_is_the_tick_not_an_attribute() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    let whole = fx.json("asof", &["edwin", "@-205"]);
    assert_eq!(
        whole,
        fx.exp().with_tick(kala_core::Tick(-205)).asof(edwin, None)
    );
    assert_eq!(whole["tick"], json!(-205));
    assert!(
        whole["attributes"].as_array().unwrap().len() > 1,
        "all attributes listed"
    );
}

#[test]
fn asof_attr_then_tick_lists_just_that_attribute() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    let one = fx.json("asof", &["edwin", "ally_of", "@-205"]);
    assert_eq!(
        one,
        fx.exp()
            .with_tick(kala_core::Tick(-205))
            .asof(edwin, Some("ally_of"))
    );
    let attrs = one["attributes"].as_array().unwrap();
    assert_eq!(attrs.len(), 1);
    assert_eq!(attrs[0]["attr"], json!("ally_of"));
    // Same through --at.
    assert_eq!(one, fx.json("asof", &["edwin", "ally_of", "--at", "-205"]));
}

// ---------------------------------------------------------------------------
// edge arity
// ---------------------------------------------------------------------------

#[test]
fn edge_with_a_tick_but_a_two_word_triple_is_a_usage_error() {
    let fx = varrow();
    fx.fails(2, "edge", &["edwin", "present", "@5"]);
}

#[test]
fn edge_with_four_plain_words_is_a_usage_error() {
    let fx = varrow();
    fx.fails(2, "edge", &["edwin", "present", "carnath", "extra"]);
    fx.fails(2, "edge", &["edwin", "present"]);
}

#[test]
fn edge_by_id_with_and_without_a_tick() {
    let fx = varrow();
    let (e, c) = (fx.id("edwin"), fx.id("carnath"));
    let eid = fx
        .mem
        .format_id(EntityId::from(fx.mem.edge_id(e, "present", c).unwrap()));
    let want = fx
        .exp()
        .with_tick(kala_core::Tick(-240))
        .edge(e, "present", c);
    assert_eq!(fx.json("edge", &[&eid, "@-240"]), want);
    assert_eq!(
        fx.json("edge", &["edwin", "present", "carnath", "@-240"]),
        want
    );
}

// ---------------------------------------------------------------------------
// `+5`
// ---------------------------------------------------------------------------

#[test]
fn a_plus_sign_on_a_tick_means_the_same_number() {
    let fx = varrow();
    let plain = fx.json("asof", &["edwin", "@5"]);
    assert_eq!(plain["tick"], json!(5));
    assert_eq!(fx.json("asof", &["edwin", "@+5"]), plain);
    assert_eq!(fx.json("asof", &["edwin", "--at", "+5"]), plain);
    assert_eq!(fx.json("asof", &["edwin", "--at", "5"]), plain);
}

// ---------------------------------------------------------------------------
// find on f64 and ref-to-edge
// ---------------------------------------------------------------------------

fn f64_world() -> Fx {
    build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: reach, type: f64 }}\nentities:\n  CHARACTER: [zero, nzero, far, near, ninf]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [zero, reach, 0.0]\n      - assert: [nzero, reach, -0.0]\n      - assert: [far, reach, .inf]\n      - assert: [ninf, reach, -.inf]\n      - assert: [near, reach, 1.5]\n"
    ))
}

#[test]
fn find_f64_infinity_matches_only_the_infinite_entity() {
    let fx = f64_world();
    let got = fx.json("find", &["reach", "inf"]);
    let want = fx.exp().find(
        "reach",
        &Value::F64(f64::INFINITY),
        false,
        vec![fx.id("far")],
    );
    assert_eq!(got, want);
    assert_eq!(got["value"], json!({"type": "f64", "value": "inf"}));
    let neg = fx.json("find", &["reach", "--", "-inf"]);
    assert_eq!(neg["value"], json!({"type": "f64", "value": "-inf"}));
    assert_eq!(neg["entities"].as_array().unwrap().len(), 1);
    assert_eq!(neg["entities"][0]["label"], json!("ninf"));
}

#[test]
fn find_f64_negative_zero_equals_zero_lookup() {
    let fx = f64_world();
    let pos = fx.json("find", &["reach", "0.0"]);
    let neg = fx.json("find", &["reach", "--", "-0.0"]);
    assert_eq!(pos, neg);
    let mut labels: Vec<&str> = pos["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["label"].as_str().unwrap())
        .collect();
    labels.sort();
    assert_eq!(
        labels,
        ["nzero", "zero"],
        "-0.0 was normalized to 0.0 on write"
    );
}

#[test]
fn find_f64_nan_is_a_user_error() {
    let fx = f64_world();
    fx.fails(1, "find", &["reach", "NaN"]);
}

#[test]
fn find_a_ref_attribute_by_an_edge_id() {
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: knows, type: ref, cardinality: many }}\n    - {{ name: about, type: ref }}\nentities:\n  CHARACTER: [alice, bob, note]\ntransactions:\n  - tick: 3\n    ops:\n      - assert: [alice, knows, bob]\n      - assert: [note, about, [alice, knows, bob]]\n"
    ));
    let (a, b, n) = (fx.id("alice"), fx.id("bob"), fx.id("note"));
    let eid = EntityId::from(fx.mem.edge_id(a, "knows", b).unwrap());
    let name = fx.mem.format_id(eid);
    let got = fx.json("find", &["about", &name]);
    assert_eq!(
        got,
        fx.exp().find("about", &Value::Ref(eid), false, vec![n])
    );
    assert_eq!(got["value"]["value"]["id"], json!(name));
}

// ---------------------------------------------------------------------------
// list of an empty declared type
// ---------------------------------------------------------------------------

#[test]
fn list_of_a_declared_type_with_no_entities_is_empty_and_ok() {
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [zed]\n"
    ));
    let j = fx.json("list", &["PLACE"]);
    assert_eq!(j["node_type"], json!("PLACE"));
    assert_eq!(j["entities"], json!([]));
    let t = fx.text("list", &["PLACE"]);
    assert!(!t.contains("error"), "{t}");
}

// ---------------------------------------------------------------------------
// canon 0 (genesis)
// ---------------------------------------------------------------------------

#[test]
fn asof_at_canon_zero_holds_nothing_and_reports_tick_zero() {
    let fx = varrow();
    let o = fx.run("asof", &["edwin", "--canon", "0", "--json"]);
    if code(&o) == 0 {
        let j = parse_one_json(&stdout(&o));
        assert_eq!(j["canon"], json!(0));
        assert_eq!(j["tick"], json!(0));
        assert_eq!(j["attributes"], json!([]));
    } else {
        panic!(
            "asof at genesis must read (nothing holds), got stderr: {}",
            stderr(&o)
        );
    }
}

#[test]
fn history_at_canon_zero_is_empty() {
    let fx = varrow();
    let j = fx.json("history", &["edwin", "ally_of", "--canon", "0"]);
    assert_eq!(j["canon"], json!(0));
    assert_eq!(j["changes"], json!([]));
}

#[test]
fn neighbors_and_list_at_canon_zero_are_empty() {
    let fx = varrow();
    let n = fx.json("neighbors", &["edwin", "ally_of", "--both", "--canon", "0"]);
    assert_eq!(n["tick"], json!(0));
    assert_eq!(n["neighbors"], json!([]));
    let l = fx.json("list", &["CHARACTER", "--canon", "0"]);
    assert_eq!(l["entities"], json!([]));
}

// ---------------------------------------------------------------------------
// an edge defined after the chosen canon
// ---------------------------------------------------------------------------

#[test]
fn an_edge_defined_after_the_canon_does_not_exist_at_that_canon() {
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: knows, type: ref, cardinality: many }}\nentities:\n  CHARACTER: [alice, bob, cy]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [alice, knows, bob]\n  - tick: 2\n    ops:\n      - assert: [alice, knows, cy]\n"
    ));
    let (a, c) = (fx.id("alice"), fx.id("cy"));
    let later = fx
        .mem
        .format_id(EntityId::from(fx.mem.edge_id(a, "knows", c).unwrap()));
    // The later edge is readable at the latest canon, and at its own canon.
    assert_eq!(fx.json("edge", &[&later])["holds"], json!(true));
    let own = fx.mem.latest_tx().0.to_string();
    fx.json("edge", &[&later, "--canon", &own]);
    // Before it was defined: it does not exist, for both ways of naming it.
    let early = (fx.mem.latest_tx().0 - 1).to_string();
    fx.fails(1, "edge", &[&later, "--canon", &early]);
    fx.fails(1, "edge", &["alice", "knows", "cy", "--canon", &early]);
}

// ---------------------------------------------------------------------------
// labels: no name attribute, i64 name, escaping
// ---------------------------------------------------------------------------

#[test]
fn a_world_without_a_name_attribute_labels_by_handle_with_null_name() {
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: knows, type: ref, cardinality: many }}\nentities:\n  CHARACTER: [alice, bob]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [alice, knows, bob]\n"
    ));
    let j = fx.json("asof", &["alice"]);
    assert_eq!(j["entity"]["label"], json!("alice"));
    assert_eq!(j["entity"]["name"], json!(null));
    let l = fx.json("list", &["CHARACTER"]);
    for e in l["entities"].as_array().unwrap() {
        assert_eq!(e["name"], json!(null));
        assert_eq!(e["label"], e["handle"]);
    }
    let n = fx.json("neighbors", &["alice", "knows"]);
    assert_eq!(n["neighbors"][0]["entity"]["label"], json!("bob"));
}

#[test]
fn a_name_attribute_of_type_i64_is_not_a_name() {
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: name, type: i64 }}\nentities:\n  CHARACTER: [alice]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [alice, name, 42]\n"
    ));
    let j = fx.json("asof", &["alice"]);
    assert_eq!(j["entity"]["name"], json!(null));
    assert_eq!(j["entity"]["label"], json!("alice"));
    // The i64 is still a plain attribute value.
    assert_eq!(
        j["attributes"][0]["values"][0]["value"],
        json!({"type": "i64", "value": 42})
    );
    let t = fx.text("asof", &["alice"]);
    assert!(t.contains("42"), "{t}");
}

fn backslash_world() -> Fx {
    // `lit` is the 7 characters a \ u { 1 b } ; `real` is `a` + a real ESC.
    build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: name, type: string }}\nentities:\n  CHARACTER: [\"a\\\\u{{1b}}\", \"a\\u001B\", \"back\\\\slash\"]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [\"back\\\\slash\", name, \"x\"]\n"
    ))
}

#[test]
fn a_literal_escape_text_and_a_real_esc_are_distinguishable_in_text() {
    let fx = backslash_world();
    let t = fx.text("list", &["CHARACTER"]);
    no_raw_controls(&t);
    let lit = t.lines().filter(|l| l.contains(r"a\\u{1b}")).count();
    let real = t
        .lines()
        .filter(|l| l.contains(r"a\u{1b}") && !l.contains(r"a\\u{1b}"))
        .count();
    assert_eq!(
        (lit, real),
        (1, 1),
        "literal backslash shows as `\\\\`, real ESC as `\\u{{1b}}`:\n{t}"
    );
}

#[test]
fn a_backslash_in_a_handle_is_shown_doubled_in_text_and_raw_in_json() {
    let fx = backslash_world();
    let t = fx.text("asof", &["back\\slash"]);
    assert!(t.contains(r"back\\slash"), "{t}");
    assert!(
        !t.lines()
            .any(|l| l.contains(r"back\slash") && !l.contains(r"back\\slash")),
        "{t}"
    );
    let j = fx.json("asof", &["back\\slash"]);
    assert_eq!(j["entity"]["handle"], json!("back\\slash"));
}

#[test]
fn esc_in_handles_and_names_is_escaped_in_neighbors_and_list_text() {
    let yaml = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: name, type: string }\n    - { name: knows, type: ref, cardinality: many }\nentities:\n  CHARACTER: [\"h\\u001B[31mx\", plain]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [plain, knows, \"h\\u001B[31mx\"]\n      - assert: [plain, name, \"n\\u001B[2Jy\"]\n";
    let fx = build(yaml);
    let l = fx.text("list", &["CHARACTER"]);
    no_raw_controls(&l);
    assert!(l.contains(r"h\u{1b}[31mx"), "{l}");
    let n = fx.text("neighbors", &["plain", "knows"]);
    no_raw_controls(&n);
    assert!(n.contains(r"h\u{1b}[31mx"), "{n}");
    let a = fx.text("asof", &["plain"]);
    no_raw_controls(&a);
    assert!(a.contains(r"n\u{1b}[2Jy"), "{a}");
    let h = fx.text("history", &["plain", "knows"]);
    no_raw_controls(&h);
    // JSON keeps the real characters (escaped as JSON, which is not raw ESC).
    let o = fx.run("list", &["CHARACTER", "--json"]);
    assert!(!o.stdout.contains(&0x1b), "raw ESC in JSON");
    let j = parse_one_json(&stdout(&o));
    assert!(
        j["entities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["handle"] == json!("h\u{1b}[31mx"))
    );
}

// ---------------------------------------------------------------------------
// long values are shown in full
// ---------------------------------------------------------------------------

#[test]
fn a_thousand_char_string_value_is_shown_in_full() {
    let long: String = (0..1000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let fx = build(&format!(
        "{SCHEMA_HEAD}  attributes:\n    - {{ name: bio, type: string, indexed: false }}\nentities:\n  CHARACTER: [alice]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [alice, bio, \"{long}\"]\n"
    ));
    let t = fx.text("asof", &["alice"]);
    assert!(t.contains(&long), "text must carry all 1000 chars");
    assert!(!t.contains("..."), "no truncation marker: {t}");
    let j = fx.json("asof", &["alice"]);
    assert_eq!(
        j["attributes"][0]["values"][0]["value"]["value"],
        json!(long)
    );
    let h = fx.text("history", &["alice", "bio"]);
    assert!(h.contains(&long), "history shows it in full too");
}

#[test]
fn neighbors_and_edge_helpers_agree_with_the_api_on_varrow_both_directions() {
    // Guards the `Direction` import and the shared expectations helpers.
    let fx = varrow();
    let e = fx.id("edwin");
    let want = fx
        .exp()
        .with_tick(kala_core::Tick(-205))
        .neighbors(e, "ally_of", Direction::Both);
    assert_eq!(
        fx.json("neighbors", &["edwin", "ally_of", "--both", "@-205"]),
        want
    );
}
