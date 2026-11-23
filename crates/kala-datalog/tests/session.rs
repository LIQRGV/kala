use kala_core::{Tick, TxId, World};
use kala_datalog::{SessionDefaults, parse_rules, prepare_with};
use kala_fixture::load_fixture;
use kala_query::{Limits, Params};

fn world() -> World {
    load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml")).unwrap()
}

#[test]
fn defaults_are_fallbacks_and_explicit_query_values_win() {
    let world = world();
    let defaults = SessionDefaults {
        at: Some(Tick(-195)),
        canon: Some(TxId(11)),
        ..Default::default()
    };
    let q = "[:find ?a :where [:edwin :ally_of ?a]]";
    let p = prepare_with(&world, q, &defaults).unwrap();
    let r = kala_query::run(&world, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(r.tick, Tick(-195));
    assert_eq!(r.canon, TxId(11));
    assert_eq!(r.rows.len(), 3);
    let q = "[:find ?a :at -200 :canon 12 :where [:edwin :ally_of ?a]]";
    let p = prepare_with(&world, q, &defaults).unwrap();
    let r = kala_query::run(&world, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(r.tick, Tick(-200));
    assert_eq!(r.canon, TxId(12));
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn session_rules_merge_and_query_rules_replace_all_matching_definitions() {
    let world = world();
    let rules = parse_rules(
        "[[(allied ?a ?b) [?a :ally_of ?b]] [(allied ?a ?b) [?a :ally_of ?x] (allied ?x ?b)]]",
    )
    .unwrap();
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].name(), "allied");
    let defaults = SessionDefaults {
        rules,
        ..Default::default()
    };
    let p = prepare_with(
        &world,
        "[:find ?b :at 600 :where (allied :edwin ?b)]",
        &defaults,
    )
    .unwrap();
    let r = kala_query::run(&world, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(r.rows.len(), 4);
    let p = prepare_with(
        &world,
        "[:find ?b :at -210 :rules [[(allied ?a ?b) [?a :ally_of ?b]]] :where (allied :edwin ?b)]",
        &defaults,
    )
    .unwrap();
    let r = kala_query::run(&world, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn bad_session_rule_errors_name_rule_and_its_definition_location() {
    let world = world();
    let source = "[\n [(broken ?a ?b)\n  [?a :unknown ?b]]\n]";
    let defaults = SessionDefaults {
        rules: parse_rules(source).unwrap(),
        ..Default::default()
    };
    let error = prepare_with(&world, "[:find ?b :where (broken :edwin ?b)]", &defaults)
        .unwrap_err()
        .to_string();
    assert!(error.contains("broken"), "{error}");
    assert!(error.contains("line 3, column 7"), "{error}");
}

#[test]
fn parse_rules_preserves_source_and_refuses_bad_shapes() {
    let source = "[[(one ?a) [?a :name _]]]";
    let rules = parse_rules(source).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].source(), "[(one ?a) [?a :name _]]");
    for bad in ["", "[(one ?a)]", "[[(one ?a)]]", "[] []"] {
        assert!(parse_rules(bad).is_err(), "accepted {bad:?}");
    }
}

#[test]
fn merging_session_rules_enforces_pattern_and_rule_and_source_limits() {
    use kala_datalog::{ErrorKind, MAX_PATTERNS, MAX_QUERY_BYTES, MAX_RULES};
    let world = world();
    let many_patterns = format!("[[(r ?e) {}]]", "[?e :name _] ".repeat(MAX_PATTERNS));
    let defaults = SessionDefaults {
        rules: parse_rules(&many_patterns).unwrap(),
        ..Default::default()
    };
    let error =
        prepare_with(&world, "[:find ?e :where [?e :name _] (r ?e)]", &defaults).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Limit, "{error}");
    let mut rules = Vec::new();
    // A chain r0 -> r1 -> ... so every definition is reachable from the query
    // (unreachable session rules are not merged, see below).
    for i in 0..=MAX_RULES {
        let next = i + 1;
        rules.extend(parse_rules(&format!("[[(r{i} ?e) [?e :name _] (r{next} ?e)]]")).unwrap());
    }
    let defaults = SessionDefaults {
        rules,
        ..Default::default()
    };
    let error = prepare_with(&world, "[:find ?e :where (r0 ?e)]", &defaults).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Limit, "{error}");
    let huge_rule = format!(
        "[[(r ?e) [?e :name \"{}\"]]]",
        "a".repeat(MAX_QUERY_BYTES - 50)
    );
    let defaults = SessionDefaults {
        rules: parse_rules(&huge_rule).unwrap(),
        ..Default::default()
    };
    let source = format!("[:find ?e :where (r ?e)] ;{}", "x".repeat(100));
    let error = prepare_with(&world, &source, &defaults).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Limit, "{error}");
}

// Review round 1: only session rules a query can reach are merged, directly
// or through other rules and negations.
#[test]
fn unreachable_session_rules_are_not_compiled() {
    let world = world();
    let rules = parse_rules(
        "[[(broken ?e) [?e :ally_off ?x]] [(ally ?a ?b) [?a :ally_of ?b]] [(via ?a ?b) (ally ?a ?b)]]",
    )
    .unwrap();
    let defaults = SessionDefaults {
        rules,
        ..Default::default()
    };
    prepare_with(&world, "[:find ?a :where [:edwin :ally_of ?a]]", &defaults).unwrap();
    prepare_with(&world, "[:find ?b :where (via :edwin ?b)]", &defaults).unwrap();
    prepare_with(
        &world,
        "[:find ?b :where [:edwin :ally_of ?b] (not (via :mara ?b))]",
        &defaults,
    )
    .unwrap();
    let err = prepare_with(&world, "[:find ?e :where (broken ?e)]", &defaults).unwrap_err();
    assert!(err.to_string().contains("rule `broken`"), "{err}");
}
