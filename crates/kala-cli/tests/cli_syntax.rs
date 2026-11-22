//! Slice 5 item 3: how read commands take an entity, a tick and a canon.
//! Expected answers come from the kala-core API on an in-memory twin.

mod common;
use common::*;
use kala_core::{Direction, TxId};
use serde_json::json;

#[test]
fn entity_by_handle_and_by_id_give_the_same_answer() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    let by_id = fx.mem.format_id(edwin);
    let a = fx.json("asof", &["edwin", "@-210"]);
    let b = fx.json("asof", &[&by_id, "@-210"]);
    assert_eq!(a, b);
    assert_eq!(a, Exp::new(&fx.mem, None, Some(-210)).asof(edwin, None));
    assert_eq!(a["entity"]["id"], json!(by_id));
    assert_eq!(a["entity"]["handle"], json!("edwin"));
}

#[test]
fn unknown_handle_is_exit_1_naming_the_argument() {
    let fx = varrow();
    let err = fx.fails(1, "asof", &["nobody_here"]);
    assert!(err.contains("nobody_here"), "stderr: {err}");
}

#[test]
fn unknown_handle_is_echoed_escaped() {
    let fx = varrow();
    let err = fx.fails(1, "asof", &["no\u{1b}such"]);
    assert!(err.contains("no\\u{1b}such"), "stderr: {err}");
}

#[test]
fn well_formed_but_undefined_id_is_the_reads_unknown_entity_error() {
    let fx = varrow();
    let ghost = "CHARACTER#9999";
    let id = fx.mem.parse_id(ghost).expect("parses");
    let want = fx
        .mem
        .at_canon(fx.mem.latest_tx())
        .as_of(id, "name", fx.mem.latest_tick())
        .unwrap_err()
        .to_string();
    let err = fx.fails(1, "asof", &[ghost]);
    assert!(err.contains(&want), "want {want:?} in stderr: {err}");
}

#[test]
fn an_edge_id_works_where_an_entity_does() {
    let fx = varrow();
    let eid = fx
        .mem
        .edge_id(fx.id("edwin"), "present", fx.id("carnath"))
        .unwrap();
    let name = fx.mem.format_id(eid.into());
    assert!(name.starts_with("edge#"), "{name}");
    let got = fx.json("asof", &[&name, "@-240"]);
    assert_eq!(
        got,
        Exp::new(&fx.mem, None, Some(-240)).asof(eid.into(), None)
    );
    assert_eq!(got["entity"]["id"], json!(name));
    assert_eq!(got["entity"]["label"], json!(name));
    assert_eq!(got["entity"]["handle"], json!(null));
    // The disguise property of Edwin's stay in Carnath, asserted @-250.
    let attrs = got["attributes"].as_array().unwrap();
    assert_eq!(attrs.len(), 1, "{got}");
    assert_eq!(attrs[0]["attr"], json!("disguised"));
    assert_eq!(
        attrs[0]["values"][0]["value"],
        json!({"type": "bool", "value": true})
    );
    assert_eq!(
        attrs[0]["values"][0]["since"],
        json!({"tick": -250, "precision": "exact"})
    );
}

#[test]
fn undefined_edge_id_is_exit_1() {
    let fx = varrow();
    let err = fx.fails(1, "asof", &["edge#99999"]);
    assert!(err.contains("99999"), "stderr: {err}");
}

#[test]
fn tick_positional_and_at_flag_are_the_same() {
    let fx = varrow();
    let a = fx.json("asof", &["edwin", "@-210"]);
    let b = fx.json("asof", &["edwin", "--at", "-210"]);
    assert_eq!(a, b);
    assert_eq!(a["tick"], json!(-210));
    // Same for the other tick-taking reads.
    let n1 = fx.json("neighbors", &["edwin", "ally_of", "--both", "@-210"]);
    let n2 = fx.json("neighbors", &["edwin", "ally_of", "--both", "--at", "-210"]);
    assert_eq!(n1, n2);
    assert_eq!(n1["tick"], json!(-210));
}

#[test]
fn extreme_ticks_parse() {
    let fx = varrow();
    let min = i64::MIN.to_string();
    let max = i64::MAX.to_string();
    let a = fx.json("asof", &["edwin", "--at", &min]);
    assert_eq!(a["tick"], json!(i64::MIN));
    let b = fx.json("asof", &["edwin", &format!("@{max}")]);
    assert_eq!(b["tick"], json!(i64::MAX));
}

#[test]
fn positional_and_flag_tick_together_is_a_usage_error() {
    let fx = varrow();
    fx.fails(2, "asof", &["edwin", "@5", "--at", "5"]);
}

#[test]
fn malformed_ticks_are_usage_errors() {
    let fx = varrow();
    for bad in [
        &["@abc"][..],
        &["@"],
        &["--at", "99999999999999999999"],
        &["--at", "abc"],
        &["@1.5"],
    ] {
        let mut args = vec!["edwin"];
        args.extend_from_slice(bad);
        fx.fails(2, "asof", &args);
    }
    // The same rule on the other tick-taking commands.
    fx.fails(2, "neighbors", &["edwin", "ally_of", "@abc"]);
    fx.fails(2, "find", &["name", "Edwin", "@abc"]);
    fx.fails(2, "list", &["CHARACTER", "--at", "x"]);
}

#[test]
fn no_tick_means_the_latest_tick_of_the_chosen_canon() {
    let fx = varrow();
    let w = &fx.mem;
    let latest = fx.json("asof", &["edwin"]);
    assert_eq!(latest["tick"], json!(w.latest_tick().0));
    assert_eq!(latest["canon"], json!(w.latest_tx().0));

    // An edge defined after the canon does not exist there, so the canon
    // must be at or after the tx that defines edwin-present-carnath.
    let eid = w
        .edge_id(fx.id("edwin"), "present", fx.id("carnath"))
        .expect("edge exists");
    let def_tx = w
        .entries()
        .find_map(|e| match e.body {
            kala_core::log::EntryBody::DefineEdge { id, .. } if id == eid => Some(e.tx.0),
            _ => None,
        })
        .expect("DefineEdge entry");
    let older = (def_tx..w.latest_tx().0)
        .find(|&tx| w.at_canon(TxId(tx)).latest_tick() != w.latest_tick())
        .expect("a canon at/after the edge's tx with a smaller latest tick");
    let want = w.at_canon(TxId(older)).latest_tick();
    assert_ne!(want, w.latest_tick());
    let c = older.to_string();
    let got = fx.json("asof", &["edwin", "--canon", &c]);
    assert_eq!(got["tick"], json!(want.0));
    assert_eq!(got["canon"], json!(older));
    for (cmd, rest) in [
        ("neighbors", vec!["edwin", "ally_of"]),
        ("find", vec!["name", "Edwin"]),
        ("list", vec!["CHARACTER"]),
        ("edge", vec!["edwin", "present", "carnath"]),
    ] {
        let mut r = rest.clone();
        r.extend(["--canon", &c]);
        let j = fx.json(cmd, &r);
        assert_eq!(j["tick"], json!(want.0), "{cmd}: {j}");
    }
}

#[test]
fn every_tick_taking_read_prints_the_tick_it_used_in_its_text_header() {
    let fx = varrow();
    for (cmd, rest) in [
        ("asof", vec!["edwin"]),
        ("neighbors", vec!["edwin", "ally_of"]),
        ("find", vec!["name", "Edwin"]),
        ("list", vec!["CHARACTER"]),
        ("edge", vec!["edwin", "present", "carnath"]),
    ] {
        let mut explicit = rest.clone();
        explicit.push("@-213");
        let out = fx.text(cmd, &explicit);
        assert!(out.contains("at tick -213"), "{cmd}: {out}");
        let out = fx.text(cmd, &rest);
        let want = format!("at tick {}", fx.mem.latest_tick().0);
        assert!(out.contains(&want), "{cmd} without a tick: {out}");
    }
}

#[test]
fn canon_accepts_tx_hash_and_bare_number_on_every_read() {
    let fx = varrow();
    let n = fx.mem.latest_tx().0 - 3;
    let hash = format!("tx#{n}");
    let bare = n.to_string();
    for (cmd, rest) in [
        ("asof", vec!["edwin", "ally_of", "@-195"]),
        ("neighbors", vec!["edwin", "ally_of", "--both", "@-195"]),
        ("history", vec!["edwin", "ally_of"]),
        ("find", vec!["name", "Edwin", "@-195"]),
        ("list", vec!["CHARACTER", "@-195"]),
        ("edge", vec!["edwin", "present", "carnath", "@-195"]),
    ] {
        let mut a = rest.clone();
        a.extend(["--canon", &hash]);
        let mut b = rest.clone();
        b.extend(["--canon", &bare]);
        let ja = fx.json(cmd, &a);
        let jb = fx.json(cmd, &b);
        assert_eq!(ja, jb, "{cmd}");
        assert_eq!(ja["canon"], json!(n), "{cmd}: {ja}");
    }
}

#[test]
fn canon_is_accepted_by_doctor_too() {
    let fx = varrow();
    let n = fx.mem.latest_tx().0 - 3;
    let a = fx.run("doctor", &["--canon", &format!("tx#{n}")]);
    let b = fx.run("doctor", &["--canon", &n.to_string()]);
    assert!(a.status.code().unwrap() < 2, "stderr: {}", stderr(&a));
    assert_eq!(code(&a), code(&b));
    assert!(!a.stdout.is_empty(), "doctor prints a report");
    assert_eq!(a.stdout, b.stdout);
}

#[test]
fn malformed_canon_is_a_usage_error_on_every_read() {
    let fx = varrow();
    for bad in ["tx#", "-1", "abc", "tx#x", "tx#-1", ""] {
        for (cmd, rest) in [
            ("asof", vec!["edwin"]),
            ("neighbors", vec!["edwin", "ally_of"]),
            ("history", vec!["edwin", "ally_of"]),
            ("find", vec!["name", "Edwin"]),
            ("list", vec!["CHARACTER"]),
            ("edge", vec!["edwin", "present", "carnath"]),
            ("doctor", vec![]),
        ] {
            let mut r = rest.clone();
            let eq = format!("--canon={bad}");
            r.push(&eq);
            let err = fx.fails(2, cmd, &r);
            assert!(
                err.contains("canon"),
                "usage error should name the option: {err}"
            );
        }
    }
    // Positive control: a well-formed canon is accepted.
    assert_eq!(
        fx.json("asof", &["edwin", "--canon", "1"])["canon"],
        json!(1)
    );
}

#[test]
fn canon_past_the_latest_reads_the_latest_and_reports_the_canon_used() {
    let fx = varrow();
    let want = fx.json("asof", &["edwin", "ally_of", "@-195"]);
    let got = fx.json("asof", &["edwin", "ally_of", "@-195", "--canon", "999999"]);
    assert_eq!(got, want);
    assert_eq!(got["canon"], json!(fx.mem.latest_tx().0));
    let got = fx.json("list", &["CHARACTER", "--canon", "tx#999999"]);
    assert_eq!(got["canon"], json!(fx.mem.latest_tx().0));
}

#[test]
fn default_canon_is_the_latest_tx() {
    let fx = varrow();
    for (cmd, rest) in [
        ("asof", vec!["edwin"]),
        ("neighbors", vec!["edwin", "ally_of"]),
        ("history", vec!["edwin", "ally_of"]),
        ("find", vec!["name", "Edwin"]),
        ("list", vec!["CHARACTER"]),
        ("edge", vec!["edwin", "present", "carnath"]),
    ] {
        assert_eq!(
            fx.json(cmd, &rest)["canon"],
            json!(fx.mem.latest_tx().0),
            "{cmd}"
        );
    }
}

#[test]
fn an_older_canon_changes_the_answer_and_the_latest_tick() {
    let fx = varrow();
    // The retcon is the last transaction: before it, Greg's alliance ended @-190.
    let before = fx.mem.latest_tx().0 - 1;
    let c = before.to_string();
    let e = Exp::new(&fx.mem, Some(before), Some(-195));
    let got = fx.json(
        "neighbors",
        &["edwin", "ally_of", "--both", "@-195", "--canon", &c],
    );
    assert_eq!(got, e.neighbors(fx.id("edwin"), "ally_of", Direction::Both));
    let latest = fx.json("neighbors", &["edwin", "ally_of", "--both", "@-195"]);
    assert_ne!(got["neighbors"], latest["neighbors"]);
}

#[test]
fn history_takes_no_tick() {
    let fx = varrow();
    fx.fails(2, "history", &["edwin", "ally_of", "@5"]);
    fx.fails(2, "history", &["edwin", "ally_of", "--at", "5"]);
    // and works without one (positive control, so this isn't a stub passing)
    let j = fx.json("history", &["edwin", "ally_of"]);
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, None).history(fx.id("edwin"), "ally_of")
    );
}

#[test]
fn neighbors_direction_flags_conflict() {
    let fx = varrow();
    fx.fails(2, "neighbors", &["edwin", "ally_of", "--in", "--out"]);
    fx.fails(2, "neighbors", &["edwin", "ally_of", "--both", "--in"]);
    // Default is --out.
    let d = fx.json("neighbors", &["edwin", "ally_of", "@-210"]);
    assert_eq!(d["direction"], json!("out"));
    assert_eq!(
        d,
        fx.json("neighbors", &["edwin", "ally_of", "--out", "@-210"])
    );
}

#[test]
fn edge_takes_an_id_or_a_triple_and_nothing_between() {
    let fx = varrow();
    let eid = fx
        .mem
        .edge_id(fx.id("edwin"), "present", fx.id("carnath"))
        .unwrap();
    let name = fx.mem.format_id(eid.into());
    let by_id = fx.json("edge", &[&name, "@-240"]);
    let by_triple = fx.json("edge", &["edwin", "present", "carnath", "@-240"]);
    assert_eq!(by_id, by_triple);
    assert_eq!(by_id["edge"], json!(name));
    fx.fails(2, "edge", &[&name, "present"]);
    fx.fails(2, "edge", &["edwin", "present"]);
    fx.fails(2, "edge", &["edwin", "present", "carnath", "extra"]);
}
