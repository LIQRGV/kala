//! Slice 5 item 4: the read commands' `--json` answers, checked against the
//! kala-core API and against the headline facts varrow.yaml's comments state.

mod common;
use common::*;
use kala_core::{Direction, TxId, Value, ValueType};
use serde_json::{Value as Json, json};

fn handles(j: &Json, key: &str) -> Vec<String> {
    j[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            let e = if n.get("entity").is_some() {
                &n["entity"]
            } else {
                n
            };
            e["label"].as_str().unwrap().to_string()
        })
        .collect()
}

fn sorted(mut v: Vec<&str>) -> Vec<String> {
    v.sort();
    v.into_iter().map(String::from).collect()
}

fn sorted_labels(j: &Json, key: &str) -> Vec<String> {
    let mut v = handles(j, key);
    v.sort();
    v
}

/// The canon just before the retcon (tx 11 of the fixture, the last one).
fn pre_retcon(fx: &Fx) -> u64 {
    let n = fx.mem.latest_tx().0 - 1;
    // Sanity: the retcon changes Edwin's allies, so this really is "before".
    let before = Exp::new(&fx.mem, Some(n), Some(-195)).neighbor_handles(
        fx.id("edwin"),
        "ally_of",
        Direction::Both,
    );
    let after = Exp::new(&fx.mem, None, Some(-195)).neighbor_handles(
        fx.id("edwin"),
        "ally_of",
        Direction::Both,
    );
    assert_ne!(before, after);
    n
}

// ---- asof -----------------------------------------------------------------

#[test]
fn asof_whole_entity_matches_the_api_at_many_ticks() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    for t in [
        -400, -340, -310, -300, -290, -260, -250, -230, -208, -190, -180, -170, 0, 700,
    ] {
        let ts = format!("@{t}");
        let got = fx.json("asof", &["edwin", &ts]);
        assert_eq!(
            got,
            Exp::new(&fx.mem, None, Some(t)).asof(edwin, None),
            "tick {t}"
        );
    }
}

#[test]
fn asof_edwins_name_changes_at_his_coronation() {
    let fx = varrow();
    let at = |t: &str| fx.json("asof", &["edwin", "name", t]);
    let before = at("@-181");
    let v = &before["attributes"][0];
    assert_eq!(v["attr"], json!("name"));
    assert_eq!(v["values"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["values"][0]["value"],
        json!({"type": "string", "value": "Edwin"})
    );
    assert_eq!(
        v["values"][0]["since"],
        json!({"tick": -300, "precision": "exact"})
    );
    assert_eq!(before["entity"]["name"], json!("Edwin"));

    let after = at("@-180");
    let v = &after["attributes"][0];
    assert_eq!(v["values"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["values"][0]["value"],
        json!({"type": "string", "value": "the Grey King"})
    );
    assert_eq!(
        v["values"][0]["since"],
        json!({"tick": -180, "precision": "year"})
    );
    assert_eq!(after["entity"]["name"], json!("the Grey King"));
    assert_eq!(after["entity"]["label"], json!("edwin"));
}

#[test]
fn asof_with_an_attribute_lists_it_even_when_empty() {
    let fx = varrow();
    let j = fx.json("asof", &["edwin", "ally_of", "@-400"]);
    assert_eq!(j["attributes"], json!([{"attr": "ally_of", "values": []}]));
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, Some(-400)).asof(fx.id("edwin"), Some("ally_of"))
    );
}

#[test]
fn asof_without_an_attribute_omits_empty_ones_in_schema_order() {
    let fx = varrow();
    let j = fx.json("asof", &["edwin", "@-210"]);
    let names: Vec<&str> = j["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["attr"].as_str().unwrap())
        .collect();
    let schema: Vec<String> = fx.mem.attributes().map(|a| a.name.clone()).collect();
    let mut last = 0;
    for n in &names {
        let i = schema
            .iter()
            .position(|s| s == n)
            .expect("schema attribute");
        assert!(i >= last, "{names:?} not in schema order {schema:?}");
        last = i;
    }
    assert!(names.contains(&"name") && names.contains(&"located_in"));
    assert!(
        !names.contains(&"disguised"),
        "Edwin himself has no `disguised`: {names:?}"
    );
    for a in j["attributes"].as_array().unwrap() {
        assert!(!a["values"].as_array().unwrap().is_empty(), "{a}");
    }
    // Nothing is known about Edwin that early.
    let early = fx.json("asof", &["edwin", "@-400"]);
    assert_eq!(early["attributes"], json!([]));
}

#[test]
fn asof_unknown_attribute_is_exit_1() {
    let fx = varrow();
    let err = fx.fails(1, "asof", &["edwin", "no_such_attr"]);
    assert!(err.contains("no_such_attr"), "stderr: {err}");
}

#[test]
fn asof_an_inverse_name_is_exit_1() {
    let fx = build(TYPED);
    let want = fx
        .mem
        .as_of(fx.id("bob"), "child_of", kala_core::Tick(10))
        .unwrap_err()
        .to_string();
    let err = fx.fails(1, "asof", &["bob", "child_of", "@10"]);
    assert!(
        err.contains(&want) || err.contains("child_of"),
        "stderr: {err}"
    );
    // The real attribute on the other side works.
    let j = fx.json("asof", &["alice", "parent_of", "@10"]);
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, Some(10)).asof(fx.id("alice"), Some("parent_of"))
    );
    assert_eq!(j["attributes"][0]["values"].as_array().unwrap().len(), 2);
}

#[test]
fn asof_typed_values_have_their_types() {
    let fx = build(TYPED);
    let j = fx.json("asof", &["alice", "@10"]);
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, Some(10)).asof(fx.id("alice"), None)
    );
    let find = |a: &str| {
        j["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["attr"] == a)
            .unwrap()["values"][0]["value"]
            .clone()
    };
    assert_eq!(find("age"), json!({"type": "i64", "value": -5}));
    assert_eq!(find("height"), json!({"type": "f64", "value": 1.5}));
    assert_eq!(find("brave"), json!({"type": "bool", "value": true}));
    assert_eq!(find("name"), json!({"type": "string", "value": "Alice"}));
    let home = find("home");
    assert_eq!(home["type"], json!("ref"));
    assert_eq!(
        home["value"],
        json!({"id": "PLACE#1", "handle": "keep", "name": null, "label": "keep"})
    );
}

#[test]
fn asof_greg_overlap_draft_has_two_places_and_the_resolving_retract_leaves_one() {
    let fx = varrow();
    let w = &fx.mem;
    let greg = fx.id("greg");
    // Canons whose located_in overlaps name Greg: the draft between tx 7 and tx 9.
    let draft: Vec<u64> = (1..=w.latest_tx().0)
        .filter(|&tx| {
            w.at_canon(TxId(tx))
                .overlaps("located_in")
                .unwrap()
                .iter()
                .any(|o| o.entity == greg)
        })
        .collect();
    assert!(
        !draft.is_empty() && *draft.last().unwrap() < w.latest_tx().0,
        "{draft:?}"
    );
    let c = draft.last().unwrap().to_string();

    let j = fx.json("asof", &["greg", "located_in", "@601", "--canon", &c]);
    let vals = j["attributes"][0]["values"].as_array().unwrap();
    assert_eq!(vals.len(), 2, "{j}");
    let mut labels: Vec<&str> = vals
        .iter()
        .map(|v| v["value"]["value"]["label"].as_str().unwrap())
        .collect();
    labels.sort();
    assert_eq!(labels, ["ashford", "varrow"]);
    let since = |label: &str| {
        vals.iter()
            .find(|v| v["value"]["value"]["label"] == label)
            .unwrap()["since"]
            .clone()
    };
    assert_eq!(since("varrow"), json!({"tick": 200, "precision": "exact"}));
    assert_eq!(since("ashford"), json!({"tick": 601, "precision": "exact"}));
    assert_eq!(
        j,
        Exp::new(w, Some(draft[0].max(*draft.last().unwrap())), Some(601))
            .asof(greg, Some("located_in"))
    );

    let now = fx.json("asof", &["greg", "located_in", "@601"]);
    let vals = now["attributes"][0]["values"].as_array().unwrap();
    assert_eq!(vals.len(), 1, "{now}");
    assert_eq!(vals[0]["value"]["value"]["label"], json!("ashford"));
    // Varrow [200, 250): gone at 250, held at 249.
    assert_eq!(
        fx.json("asof", &["greg", "located_in", "@249"])["attributes"][0]["values"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fx.json("asof", &["greg", "located_in", "@250"])["attributes"][0]["values"],
        json!([])
    );
}

// ---- neighbors --------------------------------------------------------------

#[test]
fn edwins_allies_match_the_api_in_every_direction() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    for t in [
        -330, -300, -290, -270, -250, -240, -210, -195, -185, -175, -160, -100, -50, 0, 700,
    ] {
        for (flag, dir) in [
            ("--both", Direction::Both),
            ("--out", Direction::Out),
            ("--in", Direction::In),
        ] {
            let ts = format!("@{t}");
            let got = fx.json("neighbors", &["edwin", "ally_of", flag, &ts]);
            assert_eq!(
                got,
                Exp::new(&fx.mem, None, Some(t)).neighbors(edwin, "ally_of", dir),
                "{flag} @{t}"
            );
        }
    }
}

#[test]
fn edwins_allies_headline_facts_and_the_retcon() {
    let fx = varrow();
    let both = |t: &str, canon: Option<&str>| {
        let mut a = vec!["edwin", "ally_of", "--both", t];
        if let Some(c) = canon {
            a.extend(["--canon", c]);
        }
        sorted_labels(&fx.json("neighbors", &a), "neighbors")
    };
    assert_eq!(
        both("@-310", None),
        sorted(vec!["greg"]),
        "prequel alliance [-320,-300)"
    );
    assert_eq!(both("@-250", None), sorted(vec!["tomas", "greg", "hild"]));
    assert_eq!(
        both("@-205", None),
        sorted(vec!["tomas", "greg", "mara"]),
        "Hild left @-240, Mara joined @-208"
    );
    // The retcon moved Greg's end from -190 to -200 and deleted Wren.
    assert_eq!(both("@-195", None), sorted(vec!["tomas", "mara"]));
    assert_eq!(both("@-170", None), sorted(vec!["tomas", "mara"]));
    let pre = pre_retcon(&fx).to_string();
    assert_eq!(
        both("@-195", Some(&pre)),
        sorted(vec!["tomas", "greg", "mara"])
    );
    assert_eq!(
        both("@-170", Some(&pre)),
        sorted(vec!["tomas", "mara", "wren"])
    );
    // Mara re-allied from her side @-60 (a different bidirectional write, one edge).
    assert_eq!(both("@-60", None), sorted(vec!["tomas", "mara"]));
    assert_eq!(both("@-100", None), sorted(vec!["tomas"]));
}

#[test]
fn bidirectional_neighbors_are_one_edge_seen_from_either_end() {
    let fx = varrow();
    let from_edwin = fx.json("neighbors", &["edwin", "ally_of", "--both", "@-205"]);
    let from_mara = fx.json("neighbors", &["mara", "ally_of", "--both", "@-205"]);
    let edge_to_mara = from_edwin["neighbors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["entity"]["label"] == "mara")
        .unwrap()["edge"]
        .clone();
    let edge_to_edwin = from_mara["neighbors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["entity"]["label"] == "edwin")
        .unwrap()["edge"]
        .clone();
    assert_eq!(edge_to_mara, edge_to_edwin);
    assert!(edge_to_mara.as_str().unwrap().starts_with("edge#"));
}

#[test]
fn neighbors_sorted_by_entity_id() {
    let fx = varrow();
    let j = fx.json("neighbors", &["edwin", "ally_of", "--both", "@-250"]);
    let ids: Vec<kala_core::EntityId> = j["neighbors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            fx.mem
                .parse_id(n["entity"]["id"].as_str().unwrap())
                .unwrap()
        })
        .collect();
    assert_eq!(ids.len(), 3);
    assert!(ids.windows(2).all(|p| p[0] < p[1]), "{ids:?}");
}

#[test]
fn who_is_in_varrow_at_several_ticks() {
    let fx = varrow();
    let varrow_id = fx.id("varrow");
    for t in [
        -400, -395, -300, -260, -225, -190, -95, -50, 20, 199, 200, 210, 249, 250, 260, 601, 700,
    ] {
        let ts = format!("@{t}");
        let got = fx.json("neighbors", &["varrow", "located_in", "--in", &ts]);
        assert_eq!(
            got,
            Exp::new(&fx.mem, None, Some(t)).neighbors(varrow_id, "located_in", Direction::In),
            "@{t}"
        );
    }
    let at = |t: &str, c: Option<&str>| {
        let mut a = vec!["varrow", "located_in", "--in", t];
        if let Some(c) = c {
            a.extend(["--canon", c]);
        }
        sorted_labels(&fx.json("neighbors", &a), "neighbors")
    };
    assert!(at("@210", None).contains(&"greg".to_string()));
    assert!(
        !at("@260", None).contains(&"greg".to_string()),
        "Greg left @250"
    );
    assert!(at("@-300", None).contains(&"osric".to_string()));
    assert!(at("@-215", None).contains(&"mara".to_string()));
    assert!(
        !at("@-190", None).contains(&"mara".to_string()),
        "Mara moved to Graymoor @-190"
    );
    // Before the resolving retract (tx 9), Greg was still in Varrow at 260.
    let draft = (fx.mem.latest_tx().0 - 3).to_string();
    assert!(at("@260", Some(&draft)).contains(&"greg".to_string()));
}

#[test]
fn neighbors_of_a_non_ref_attribute_is_exit_1() {
    let fx = varrow();
    let want = fx
        .mem
        .at_canon(fx.mem.latest_tx())
        .neighbors(fx.id("edwin"), "name", Direction::Out, kala_core::Tick(0))
        .unwrap_err()
        .to_string();
    let err = fx.fails(1, "neighbors", &["edwin", "name"]);
    assert!(err.contains(&want), "want {want:?}: {err}");
}

#[test]
fn neighbors_inverse_name_is_the_same_edge_read_from_the_other_end() {
    let fx = build(TYPED);
    let bob = fx.id("bob");
    let j = fx.json("neighbors", &["bob", "child_of", "--out", "@10"]);
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, Some(10)).neighbors(bob, "child_of", Direction::Out)
    );
    assert_eq!(handles(&j, "neighbors"), vec!["alice"]);
    let kids = fx.json("neighbors", &["alice", "parent_of", "@10"]);
    assert_eq!(
        sorted_labels(&kids, "neighbors"),
        sorted(vec!["bob", "carol"])
    );
    // Both ends agree on the edge ID.
    assert_eq!(
        j["neighbors"][0]["edge"],
        kids["neighbors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["entity"]["label"] == "bob")
            .unwrap()["edge"]
    );
}

#[test]
fn neighbors_unknown_attribute_is_exit_1() {
    let fx = varrow();
    let err = fx.fails(1, "neighbors", &["edwin", "no_such_attr"]);
    assert!(err.contains("no_such_attr"), "stderr: {err}");
}

// ---- history ----------------------------------------------------------------

#[test]
fn history_matches_the_api_at_the_latest_canon_and_before_the_retcon() {
    let fx = varrow();
    let edwin = fx.id("edwin");
    for attr in ["ally_of", "name", "located_in", "alias", "present"] {
        let got = fx.json("history", &["edwin", attr]);
        assert_eq!(
            got,
            Exp::new(&fx.mem, None, None).history(edwin, attr),
            "{attr}"
        );
        let pre = pre_retcon(&fx);
        let got = fx.json("history", &["edwin", attr, "--canon", &pre.to_string()]);
        assert_eq!(
            got,
            Exp::new(&fx.mem, Some(pre), None).history(edwin, attr),
            "{attr} pre-retcon"
        );
    }
}

#[test]
fn history_of_allies_differs_across_the_retcon() {
    let fx = varrow();
    let now = fx.json("history", &["edwin", "ally_of"]);
    let pre = fx.json(
        "history",
        &["edwin", "ally_of", "--canon", &pre_retcon(&fx).to_string()],
    );
    assert_ne!(now["changes"], pre["changes"]);
    let has = |j: &Json, who: &str| {
        j["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["value"]["value"]["label"] == who)
    };
    assert!(
        has(&pre, "wren") && !has(&now, "wren"),
        "Wren's alliance was retconned away"
    );
    // Greg's alliance ended @-190 before the retcon and @-200 after.
    let greg_end = |j: &Json| {
        j["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| {
                c["op"] == "retract"
                    && c["value"]["value"]["label"] == "greg"
                    && c["tick"].as_i64().unwrap() > -250
            })
            .map(|c| c["tick"].clone())
    };
    assert_eq!(greg_end(&pre), Some(json!(-190)));
    assert_eq!(greg_end(&now), Some(json!(-200)));
}

#[test]
fn history_carries_precision_and_resolves_names_at_the_canons_latest_tick() {
    let fx = varrow();
    let j = fx.json("history", &["edwin", "name"]);
    let ch = j["changes"].as_array().unwrap();
    assert!(
        ch.iter().any(|c| c["tick"] == -180
            && c["precision"] == "year"
            && c["op"] == "assert"
            && c["value"]["value"] == "the Grey King"),
        "{j}"
    );
    assert_eq!(j["entity"]["name"], json!("the Grey King"));
    // Mara's name change is vague to the decade.
    let m = fx.json("history", &["mara", "name"]);
    assert!(
        m["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["tick"] == -100 && c["precision"] == "decade"),
        "{m}"
    );
    // A ref value's name is resolved at the latest tick, not at the change's tick.
    let h = fx.json("history", &["greg", "located_in"]);
    let ashford = h["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["value"]["value"]["label"] == "ashford")
        .unwrap();
    assert_eq!(ashford["value"]["value"]["name"], json!("Ashford"));
}

#[test]
fn history_of_a_never_written_attribute_is_empty() {
    let fx = varrow();
    let j = fx.json("history", &["varrow", "ally_of"]);
    assert_eq!(j["changes"], json!([]));
    assert_eq!(j["attr"], json!("ally_of"));
    assert_eq!(j["entity"]["label"], json!("varrow"));
}

// ---- find -------------------------------------------------------------------

#[test]
fn find_by_name_case_sensitive_and_insensitive() {
    let fx = varrow();
    let s = |x: &str| Value::Str(x.into());
    // Two characters are called Edwin at different times: names are not IDs.
    for t in [-400, -300, -200, -181, -180, 0, 299, 300, 700] {
        let ts = format!("@{t}");
        let e = Exp::new(&fx.mem, None, Some(t));
        let ids = fx
            .mem
            .at_canon(fx.mem.latest_tx())
            .lookup("name", &s("Edwin"), kala_core::Tick(t))
            .unwrap();
        assert_eq!(
            fx.json("find", &["name", "Edwin", &ts]),
            e.find("name", &s("Edwin"), false, ids),
            "@{t}"
        );
        let ids = fx
            .mem
            .at_canon(fx.mem.latest_tx())
            .lookup_ci("name", "edwin", kala_core::Tick(t))
            .unwrap();
        assert_eq!(
            fx.json("find", &["name", "edwin", "-i", &ts]),
            e.find("name", &s("edwin"), true, ids),
            "-i @{t}"
        );
    }
    let names = |args: &[&str]| sorted_labels(&fx.json("find", args), "entities");
    assert_eq!(names(&["name", "Edwin", "@-200"]), ["edwin"]);
    assert_eq!(
        names(&["name", "Edwin", "@-100"]),
        Vec::<String>::new(),
        "he is the Grey King by then"
    );
    assert_eq!(
        names(&["name", "Edwin"]),
        ["kael"],
        "at the latest tick only the second Edwin"
    );
    assert_eq!(
        names(&["name", "edwin", "@-200"]),
        Vec::<String>::new(),
        "exact lookup is case-sensitive"
    );
    assert_eq!(names(&["name", "edwin", "-i", "@-200"]), ["edwin"]);
    assert_eq!(names(&["name", "EDWIN", "-i", "@300"]), ["kael"]);
    // The entity objects carry the name at the command's tick.
    let j = fx.json("find", &["name", "Edwin", "@-200"]);
    assert_eq!(j["value"], json!({"type": "string", "value": "Edwin"}));
    assert_eq!(j["case_insensitive"], json!(false));
    assert_eq!(j["entities"][0]["name"], json!("Edwin"));
    assert_eq!(
        fx.json("find", &["name", "edwin", "-i", "@-200"])["case_insensitive"],
        json!(true)
    );
}

#[test]
fn find_canon_changes_who_matches() {
    let fx = varrow();
    // kael is defined in tx 10, so before it nobody is called Edwin at 300.
    let c = (fx.mem.latest_tx().0 - 2).to_string();
    let j = fx.json("find", &["name", "Edwin", "@300", "--canon", &c]);
    assert_eq!(j["entities"], json!([]));
    assert_eq!(j["canon"], json!(fx.mem.latest_tx().0 - 2));
}

#[test]
fn find_parses_the_value_by_the_attribute_type() {
    let fx = build(TYPED);
    let e = Exp::new(&fx.mem, None, Some(10));
    let v = fx.mem.at_canon(fx.mem.latest_tx());
    let t = kala_core::Tick(10);
    let alice = fx.id("alice");
    let bob = fx.id("bob");
    assert_eq!(
        fx.json("find", &["age", "--", "-5", "@10"]),
        e.find(
            "age",
            &Value::I64(-5),
            false,
            v.lookup("age", &Value::I64(-5), t).unwrap()
        )
    );
    assert_eq!(
        fx.json("find", &["age", "7", "@10"])["entities"][0]["id"],
        json!(fx.mem.format_id(bob))
    );
    assert_eq!(
        fx.json("find", &["height", "1.5", "@10"]),
        e.find("height", &Value::F64(1.5), false, vec![alice])
    );
    assert_eq!(
        fx.json("find", &["brave", "true", "@10"]),
        e.find("brave", &Value::Bool(true), false, vec![alice])
    );
    assert_eq!(
        fx.json("find", &["brave", "false", "@10"])["entities"],
        json!([])
    );
    let keep = fx.id("keep");
    assert_eq!(
        fx.json("find", &["home", "keep", "@10"]),
        e.find("home", &Value::Ref(keep), false, vec![alice])
    );
    let by_id = fx.mem.format_id(keep);
    assert_eq!(
        fx.json("find", &["home", &by_id, "@10"]),
        fx.json("find", &["home", "keep", "@10"])
    );
    // The echoed value is typed and the sorted entities are IDs ascending.
    assert_eq!(
        fx.json("find", &["age", "7", "@10"])["value"],
        json!({"type": "i64", "value": 7})
    );
}

#[test]
fn find_with_a_value_that_does_not_parse_is_exit_1() {
    let fx = build(TYPED);
    for (attr, bad) in [
        ("age", "abc"),
        ("age", "1.5"),
        ("height", "x"),
        ("brave", "maybe"),
        ("home", "nobody_here"),
    ] {
        let err = fx.fails(1, "find", &[attr, bad, "@10"]);
        assert!(err.contains(bad), "{attr} {bad}: {err}");
    }
}

#[test]
fn find_case_insensitive_is_for_string_attributes_only() {
    let fx = build(TYPED);
    let want = fx
        .mem
        .at_canon(fx.mem.latest_tx())
        .lookup_ci("age", "7", kala_core::Tick(10))
        .unwrap_err()
        .to_string();
    let err = fx.fails(1, "find", &["age", "7", "-i", "@10"]);
    assert!(err.contains(&want) || err.contains("age"), "stderr: {err}");
    // positive control on the same world
    assert_eq!(
        fx.json("find", &["name", "ALICE", "-i", "@10"])["entities"][0]["label"],
        json!("alice")
    );
}

#[test]
fn find_on_an_unindexed_attribute_is_the_stores_error() {
    let fx = varrow();
    let want = fx
        .mem
        .at_canon(fx.mem.latest_tx())
        .lookup("biography", &Value::Str("x".into()), kala_core::Tick(0))
        .unwrap_err()
        .to_string();
    let err = fx.fails(1, "find", &["biography", "x"]);
    assert!(err.contains(&want), "want {want:?}: {err}");
    let err = fx.fails(1, "find", &["no_such_attr", "x"]);
    assert!(err.contains("no_such_attr"), "{err}");
}

// ---- list -------------------------------------------------------------------

#[test]
fn list_characters_in_id_order() {
    let fx = varrow();
    let j = fx.json("list", &["CHARACTER"]);
    assert_eq!(j, Exp::new(&fx.mem, None, None).list("CHARACTER"));
    let labels = handles(&j, "entities");
    assert_eq!(
        labels.len(),
        33,
        "8 named, 24 villagers and the second Edwin"
    );
    assert_eq!(labels[0], "edwin");
    assert_eq!(labels.last().unwrap(), "kael");
    assert_eq!(j["node_type"], json!("CHARACTER"));
    assert_eq!(j["entities"][0]["id"], json!("CHARACTER#1"));
    for t in ["PLACE", "ITEM", "EVENT"] {
        assert_eq!(fx.json("list", &[t]), Exp::new(&fx.mem, None, None).list(t));
    }
}

#[test]
fn list_uses_the_canon_for_what_exists_and_the_tick_for_names() {
    let fx = varrow();
    let c = fx.mem.latest_tx().0 - 2;
    let j = fx.json("list", &["CHARACTER", "--canon", &c.to_string(), "@-200"]);
    assert_eq!(j, Exp::new(&fx.mem, Some(c), Some(-200)).list("CHARACTER"));
    assert_eq!(handles(&j, "entities").len(), 32, "kael not defined yet");
    assert_eq!(j["entities"][0]["name"], json!("Edwin"));
    let late = fx.json("list", &["CHARACTER", "@-100"]);
    assert_eq!(late["entities"][0]["name"], json!("the Grey King"));
}

#[test]
fn list_unknown_node_type_is_exit_1() {
    let fx = varrow();
    let err = fx.fails(1, "list", &["NOPE_TYPE"]);
    assert!(err.contains("NOPE_TYPE"), "{err}");
}

// ---- edge -------------------------------------------------------------------

#[test]
fn edge_with_a_property_at_two_ticks() {
    let fx = varrow();
    let (edwin, carnath) = (fx.id("edwin"), fx.id("carnath"));
    let eid = fx
        .mem
        .format_id(fx.mem.edge_id(edwin, "present", carnath).unwrap().into());
    for t in [-300, -250, -240, -221, -220, -210, -181, -180, -170, 700] {
        let ts = format!("@{t}");
        let want = Exp::new(&fx.mem, None, Some(t)).edge(edwin, "present", carnath);
        assert_eq!(
            fx.json("edge", &["edwin", "present", "carnath", &ts]),
            want,
            "triple @{t}"
        );
        assert_eq!(fx.json("edge", &[&eid, &ts]), want, "id @{t}");
    }
    // Headline: disguised from -250, dropped and replaced @-220, edge ends @-180.
    let a = fx.json("edge", &["edwin", "present", "carnath", "@-240"]);
    assert_eq!(a["edge"], json!(eid));
    assert_eq!(a["attr"], json!("present"));
    assert_eq!(a["source"]["label"], json!("edwin"));
    assert_eq!(a["target"]["label"], json!("carnath"));
    assert_eq!(a["holds"], json!(true));
    assert_eq!(a["properties"][0]["attr"], json!("disguised"));
    assert_eq!(
        a["properties"][0]["values"][0]["value"],
        json!({"type": "bool", "value": true})
    );
    assert_eq!(
        a["properties"][0]["values"][0]["since"],
        json!({"tick": -250, "precision": "exact"})
    );
    let b = fx.json("edge", &["edwin", "present", "carnath", "@-210"]);
    assert_eq!(b["holds"], json!(true));
    assert_eq!(
        b["properties"][0]["values"][0]["value"],
        json!({"type": "bool", "value": false})
    );
    assert_eq!(
        b["properties"][0]["values"][0]["since"],
        json!({"tick": -220, "precision": "exact"})
    );
    let c = fx.json("edge", &["edwin", "present", "carnath", "@-170"]);
    assert_eq!(c["holds"], json!(false), "Edwin left Carnath @-180");
    let d = fx.json("edge", &["edwin", "present", "carnath", "@-260"]);
    assert_eq!(d["holds"], json!(false), "not arrived yet");
    assert_eq!(d["properties"], json!([]));
}

#[test]
fn edge_of_another_entity_with_a_property() {
    let fx = varrow();
    let (mara, graymoor) = (fx.id("mara"), fx.id("graymoor"));
    let j = fx.json("edge", &["mara", "present", "graymoor", "@-100"]);
    assert_eq!(
        j,
        Exp::new(&fx.mem, None, Some(-100)).edge(mara, "present", graymoor)
    );
    assert_eq!(j["holds"], json!(true));
    assert_eq!(
        j["properties"][0]["values"][0]["value"],
        json!({"type": "bool", "value": false})
    );
}

#[test]
fn edge_unknown_is_exit_1() {
    let fx = varrow();
    let err = fx.fails(1, "edge", &["edge#99999"]);
    assert!(err.contains("99999"), "{err}");
    // Both ends exist but nothing was ever written between them.
    let err = fx.fails(1, "edge", &["edwin", "present", "oakhollow_nope"]);
    assert!(err.contains("oakhollow_nope"), "{err}");
    let err = fx.fails(1, "edge", &["edwin", "present", "oakhollow"]);
    assert!(err.contains("present") || err.contains("no edge"), "{err}");
    let err = fx.fails(1, "edge", &["edwin", "no_such_attr", "carnath"]);
    assert!(err.contains("no_such_attr"), "{err}");
}

#[test]
fn value_type_float_infinity_is_a_string_in_json() {
    // kala-core accepts +-inf as f64 values; JSON has no such number.
    let yaml = "schema:\n  node_types: [CHARACTER]\n  attributes:\n    - { name: reach, type: f64 }\nentities:\n  CHARACTER: [a]\ntransactions:\n  - tick: 1\n    ops:\n      - assert: [a, reach, .inf]\n";
    let fx = build(yaml);
    assert!(fx.mem.attribute("reach").map(|a| a.value_type) == Some(ValueType::F64));
    let j = fx.json("asof", &["a", "reach", "@1"]);
    assert_eq!(
        j["attributes"][0]["values"][0]["value"],
        json!({"type": "f64", "value": "inf"})
    );
}

// ---- the hard-coded headline facts, checked against the API itself --------------
// (This test needs no CLI: it keeps the story constants above honest.)

#[test]
fn the_headline_facts_hold_in_the_api() {
    let fx = varrow();
    let w = &fx.mem;
    let allies = |canon: Option<u64>, t: i64| {
        let mut v = Exp::new(w, canon, Some(t)).neighbor_handles(
            fx.id("edwin"),
            "ally_of",
            Direction::Both,
        );
        v.sort();
        v
    };
    let pre = pre_retcon(&fx);
    assert_eq!(allies(None, -310), ["greg"]);
    assert_eq!(allies(None, -250), ["greg", "hild", "tomas"]);
    assert_eq!(allies(None, -205), ["greg", "mara", "tomas"]);
    assert_eq!(allies(None, -195), ["mara", "tomas"]);
    assert_eq!(allies(None, -170), ["mara", "tomas"]);
    assert_eq!(allies(None, -100), ["tomas"]);
    assert_eq!(allies(None, -60), ["mara", "tomas"]);
    assert_eq!(allies(Some(pre), -195), ["greg", "mara", "tomas"]);
    assert_eq!(allies(Some(pre), -170), ["mara", "tomas", "wren"]);
    assert_eq!(w.entities_of_type("CHARACTER").unwrap().count(), 33);
    assert_eq!(
        w.at_canon(TxId(w.latest_tx().0 - 2))
            .entities_of_type("CHARACTER")
            .unwrap()
            .count(),
        32
    );
    let name = |t: i64| Exp::new(w, None, Some(t)).name_of(fx.id("edwin"));
    assert_eq!(name(-181).as_deref(), Some("Edwin"));
    assert_eq!(name(-180).as_deref(), Some("the Grey King"));
    let v = w.at_canon(w.latest_tx());
    let t = kala_core::Tick;
    let s = Value::Str("Edwin".into());
    assert_eq!(v.lookup("name", &s, t(-200)).unwrap(), vec![fx.id("edwin")]);
    assert!(v.lookup("name", &s, t(-100)).unwrap().is_empty());
    assert_eq!(v.lookup("name", &s, t(700)).unwrap(), vec![fx.id("kael")]);
    assert!(
        v.lookup("name", &s, t(300))
            .unwrap()
            .contains(&fx.id("kael"))
    );
    assert!(
        w.at_canon(TxId(w.latest_tx().0 - 2))
            .lookup("name", &s, t(300))
            .unwrap()
            .is_empty()
    );
    let in_varrow = |t: i64, canon: Option<u64>| {
        Exp::new(w, canon, Some(t)).neighbor_handles(fx.id("varrow"), "located_in", Direction::In)
    };
    assert!(in_varrow(210, None).contains(&"greg".to_string()));
    assert!(!in_varrow(260, None).contains(&"greg".to_string()));
    assert!(in_varrow(260, Some(w.latest_tx().0 - 3)).contains(&"greg".to_string()));
    assert!(in_varrow(-215, None).contains(&"mara".to_string()));
    assert!(!in_varrow(-190, None).contains(&"mara".to_string()));
    assert!(in_varrow(-300, None).contains(&"osric".to_string()));
    let draft: Vec<u64> = (1..=w.latest_tx().0)
        .filter(|&tx| {
            w.at_canon(TxId(tx))
                .overlaps("located_in")
                .unwrap()
                .iter()
                .any(|o| o.entity == fx.id("greg"))
        })
        .collect();
    assert!(
        !draft.is_empty() && *draft.last().unwrap() < w.latest_tx().0,
        "{draft:?}"
    );
    let e = Exp::new(w, draft.last().copied(), Some(601));
    assert_eq!(
        e.view()
            .as_of(fx.id("greg"), "located_in", e.tick)
            .unwrap()
            .len(),
        2
    );
    let e = Exp::new(w, None, Some(601));
    assert_eq!(
        e.view()
            .as_of(fx.id("greg"), "located_in", e.tick)
            .unwrap()
            .len(),
        1
    );
    let ed = Exp::new(w, None, Some(-240)).edge(fx.id("edwin"), "present", fx.id("carnath"));
    assert_eq!(ed["holds"], json!(true));
    assert_eq!(
        ed["properties"][0]["values"][0]["value"]["value"],
        json!(true)
    );
    let ed = Exp::new(w, None, Some(-210)).edge(fx.id("edwin"), "present", fx.id("carnath"));
    assert_eq!(
        ed["properties"][0]["values"][0]["since"]["tick"],
        json!(-220)
    );
    assert_eq!(
        Exp::new(w, None, Some(-170)).edge(fx.id("edwin"), "present", fx.id("carnath"))["holds"],
        json!(false)
    );
    assert_eq!(
        Exp::new(w, None, Some(-260)).edge(fx.id("edwin"), "present", fx.id("carnath"))["properties"],
        json!([])
    );
    let h = Exp::new(w, None, None).history(fx.id("edwin"), "ally_of");
    assert!(
        h["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["value"]["value"]["label"] != "wren")
    );
    // greg's alliance ends @-200 now, @-190 before the retcon
    let ends = |c: Option<u64>| {
        Exp::new(w, c, None).history(fx.id("edwin"), "ally_of")["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| {
                c["op"] == "retract"
                    && c["value"]["value"]["label"] == "greg"
                    && c["tick"].as_i64().unwrap() > -250
            })
            .map(|c| c["tick"].clone())
    };
    assert_eq!(ends(None), Some(json!(-200)));
    assert_eq!(ends(Some(pre)), Some(json!(-190)));
    let mara = Exp::new(w, None, None).history(fx.id("mara"), "name");
    assert!(
        mara["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["tick"] == -100 && c["precision"] == "decade")
    );
}
