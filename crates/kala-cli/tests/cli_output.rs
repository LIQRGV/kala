//! Slice 5 item 7: output formats. Text shows labels, quoted strings and
//! `-208 (year)` ticks; `--json` is one stable document; control characters
//! never reach the terminal raw.

mod common;
use common::*;
use serde_json::json;

const READS: [(&str, &[&str]); 6] = [
    ("asof", &["edwin", "@-205"]),
    ("neighbors", &["edwin", "ally_of", "--both", "@-205"]),
    ("history", &["edwin", "ally_of"]),
    ("find", &["name", "Edwin", "@-200"]),
    ("list", &["CHARACTER"]),
    ("edge", &["edwin", "present", "carnath", "@-240"]),
];

#[test]
fn every_read_accepts_json_and_prints_exactly_one_document() {
    let fx = varrow();
    for (cmd, rest) in READS {
        let mut args = rest.to_vec();
        args.push("--json");
        let o = fx.run(cmd, &args);
        assert_eq!(code(&o), 0, "{cmd}: {}", stderr(&o));
        assert!(o.stderr.is_empty(), "{cmd}: {}", stderr(&o));
        let out = stdout(&o);
        let j = parse_one_json(&out);
        assert_eq!(j["command"], json!(cmd));
        no_raw_controls(&out);
    }
}

#[test]
fn json_field_order_is_stable() {
    let fx = varrow();
    let order: [(&str, &[&str], &[&str]); 6] = [
        (
            "asof",
            &["edwin", "@-205"],
            &["command", "canon", "tick", "entity", "attributes"],
        ),
        (
            "neighbors",
            &["edwin", "ally_of", "--both", "@-205"],
            &[
                "command",
                "canon",
                "tick",
                "entity",
                "attr",
                "direction",
                "neighbors",
            ],
        ),
        (
            "history",
            &["edwin", "ally_of"],
            &["command", "canon", "entity", "attr", "changes"],
        ),
        (
            "find",
            &["name", "Edwin", "@-200"],
            &[
                "command",
                "canon",
                "tick",
                "attr",
                "value",
                "case_insensitive",
                "entities",
            ],
        ),
        (
            "list",
            &["CHARACTER"],
            &["command", "canon", "tick", "node_type", "entities"],
        ),
        (
            "edge",
            &["edwin", "present", "carnath", "@-240"],
            &[
                "command",
                "canon",
                "tick",
                "edge",
                "source",
                "attr",
                "target",
                "holds",
                "properties",
            ],
        ),
    ];
    for (cmd, rest, keys) in order {
        let mut args = rest.to_vec();
        args.push("--json");
        let out = stdout(&fx.run(cmd, &args));
        let pos: Vec<usize> = keys
            .iter()
            .map(|k| {
                out.find(&format!("\"{k}\":"))
                    .unwrap_or_else(|| panic!("{cmd}: no key {k} in {out}"))
            })
            .collect();
        assert!(
            pos.windows(2).all(|p| p[0] < p[1]),
            "{cmd}: keys {keys:?} out of order at {pos:?}: {out}"
        );
    }
    // Inside an entity object: id, handle, name, label.
    let out = stdout(&fx.run("list", &["CHARACTER", "--json"]));
    let first = &out[out.find("\"entities\"").unwrap()..];
    let pos: Vec<usize> = ["id", "handle", "name", "label"]
        .iter()
        .map(|k| first.find(&format!("\"{k}\"")).unwrap())
        .collect();
    assert!(pos.windows(2).all(|p| p[0] < p[1]), "{first}");
    // A change point: tick, precision, op, value.
    let out = stdout(&fx.run("history", &["edwin", "name", "--json"]));
    let ch = &out[out.find("\"changes\"").unwrap()..];
    let pos: Vec<usize> = ["tick", "precision", "op", "value"]
        .iter()
        .map(|k| ch.find(&format!("\"{k}\"")).unwrap())
        .collect();
    assert!(pos.windows(2).all(|p| p[0] < p[1]), "{ch}");
}

#[test]
fn the_same_command_twice_is_byte_identical() {
    let fx = varrow();
    for json in [false, true] {
        for (cmd, rest) in READS {
            let mut args = rest.to_vec();
            if json {
                args.push("--json");
            }
            let a = fx.run(cmd, &args);
            let b = fx.run(cmd, &args);
            assert_eq!(code(&a), 0, "{cmd}: {}", stderr(&a));
            assert!(!a.stdout.is_empty(), "{cmd} printed nothing");
            assert_eq!(a.stdout, b.stdout, "{cmd} json={json}");
        }
    }
}

#[test]
fn text_shows_labels_not_raw_ids() {
    let fx = varrow();
    for (cmd, rest) in READS {
        let out = fx.text(cmd, rest);
        no_raw_controls(&out);
        for raw in ["CHARACTER#", "PLACE#", "ITEM#", "EVENT#"] {
            assert!(!out.contains(raw), "{cmd}: raw ID {raw} in:\n{out}");
        }
    }
    let out = fx.text("neighbors", &["edwin", "ally_of", "--both", "@-205"]);
    for who in ["tomas", "greg", "mara"] {
        assert!(out.contains(who), "{out}");
    }
    assert!(!out.contains("wren") && !out.contains("hild"), "{out}");
    assert!(
        out.contains("edwin"),
        "header names the entity by label: {out}"
    );

    let out = fx.text("asof", &["edwin", "located_in", "@-205"]);
    assert!(out.contains("carnath"), "ref values print by label: {out}");
    let out = fx.text("list", &["CHARACTER"]);
    assert!(
        out.contains("edwin") && out.contains("kael") && out.contains("v24"),
        "{out}"
    );
}

#[test]
fn text_quotes_strings_and_marks_vague_ticks() {
    let fx = varrow();
    let out = fx.text("asof", &["edwin", "name", "@-179"]);
    assert!(out.contains("\"the Grey King\""), "{out}");
    assert!(
        out.contains("-180 (year)"),
        "vague tick shown with its precision: {out}"
    );
    let out = fx.text("asof", &["edwin", "name", "@-200"]);
    assert!(out.contains("\"Edwin\""), "{out}");
    assert!(out.contains("-300"), "{out}");
    assert!(
        !out.contains("-300 (exact)") && !out.contains("(exact)"),
        "exact ticks are bare: {out}"
    );

    let out = fx.text("history", &["mara", "name"]);
    assert!(out.contains("-100 (decade)"), "{out}");
    assert!(
        out.contains("\"Mara Voss\"") && out.contains("\"Mara\""),
        "{out}"
    );
    assert!(out.contains("assert") && out.contains("retract"), "{out}");
    assert!(!out.contains("(exact)"), "{out}");

    let out = fx.text("find", &["name", "Edwin", "@-200"]);
    assert!(out.contains("edwin"), "{out}");
}

#[test]
fn text_booleans_numbers_and_the_edge_property() {
    let fx = build(TYPED);
    let out = fx.text("asof", &["alice", "@10"]);
    assert!(
        out.contains("-5") && out.contains("1.5") && out.contains("true"),
        "{out}"
    );
    assert!(out.contains("\"Alice\"") && out.contains("keep"), "{out}");
    let fx = varrow();
    let out = fx.text("edge", &["edwin", "present", "carnath", "@-240"]);
    assert!(
        out.contains("disguised") && out.contains("true") && out.contains("-250"),
        "{out}"
    );
    assert!(
        out.contains("edwin") && out.contains("carnath") && out.contains("present"),
        "{out}"
    );
}

#[test]
fn json_and_text_describe_the_same_listing() {
    let fx = varrow();
    let j = fx.json("list", &["CHARACTER"]);
    let text = fx.text("list", &["CHARACTER"]);
    let mut last = 0;
    for e in j["entities"].as_array().unwrap() {
        let label = e["label"].as_str().unwrap();
        let pos = text[last..]
            .find(label)
            .unwrap_or_else(|| panic!("{label} missing or out of order in {text}"));
        last += pos + label.len();
    }
}

// ---- escaping -----------------------------------------------------------------

#[test]
fn control_characters_in_values_are_escaped_in_text() {
    let fx = build(&ctrl_fixture());
    for (cmd, rest) in [
        ("asof", vec!["ctrl", "name", "@1"]),
        ("history", vec!["ctrl", "name"]),
        ("find", vec!["name", CTRL_NAME, "@1"]),
    ] {
        let out = fx.text(cmd, &rest);
        no_raw_controls(&out);
        assert!(
            !out.contains('\u{202e}'),
            "{cmd}: raw bidi override in {out:?}"
        );
        for esc in ["\\u{1b}", "\\u{9b}", "\\u{202e}"] {
            assert!(out.contains(esc), "{cmd}: missing {esc} in {out:?}");
        }
        assert!(out.contains("ctrl"), "{cmd}: {out:?}");
    }
    let out = fx.text("list", &["CHARACTER", "@1"]);
    no_raw_controls(&out);
    assert!(out.contains("ctrl"));
}

#[test]
fn control_characters_in_values_survive_json_exactly() {
    let fx = build(&ctrl_fixture());
    let j = fx.json("asof", &["ctrl", "name", "@1"]);
    assert_eq!(
        j["attributes"][0]["values"][0]["value"],
        json!({"type": "string", "value": CTRL_NAME})
    );
    assert_eq!(j["entity"]["name"], json!(CTRL_NAME));
    let raw = stdout(&fx.run("asof", &["ctrl", "name", "@1", "--json"]));
    assert!(
        !raw.bytes().any(|b| b == 0x1b),
        "raw ESC byte in JSON output: {raw:?}"
    );
    let h = fx.json("history", &["ctrl", "name"]);
    assert_eq!(
        h["changes"][0]["value"],
        json!({"type": "string", "value": CTRL_NAME})
    );
    let f = fx.json("find", &["name", CTRL_NAME, "@1"]);
    assert_eq!(f["value"]["value"], json!(CTRL_NAME));
    assert_eq!(f["entities"][0]["name"], json!(CTRL_NAME));
}

// ---- errors with --json --------------------------------------------------------

#[test]
fn errors_with_json_still_go_to_stderr_with_nothing_on_stdout() {
    let fx = varrow();
    let cases: [(&str, &[&str], &str); 6] = [
        ("asof", &["nobody_here"], "nobody_here"),
        ("asof", &["edwin", "no_such_attr"], "no_such_attr"),
        ("neighbors", &["edwin", "no_such_attr"], "no_such_attr"),
        ("list", &["NOPE_TYPE"], "NOPE_TYPE"),
        ("find", &["no_such_attr", "x"], "no_such_attr"),
        ("edge", &["edge#99999"], "99999"),
    ];
    for (cmd, rest, needle) in cases {
        let mut args = rest.to_vec();
        args.push("--json");
        let err = fx.fails(1, cmd, &args);
        assert!(err.contains(needle), "{cmd}: {err}");
    }
    // Usage errors too.
    let err = fx.fails(2, "asof", &["edwin", "@abc", "--json"]);
    assert!(!err.is_empty());
}
