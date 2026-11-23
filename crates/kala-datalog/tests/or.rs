//! Disjunction is set union, including inside rules and correlated negation.
use kala_core::{Value, World};
use kala_datalog::{ErrorKind, prepare};
use kala_fixture::load_fixture;
use kala_query::{Datum, Limits, Params, run};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn world() -> World {
    load_fixture(
        r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: located_in, type: ref, cardinality: many }
    - { name: name, type: string }
entities:
  CHARACTER: [greg, edwin, mara]
  PLACE: [varrow, ashford, carnath]
transactions:
  - tick: -300
    ops:
      - assert: [greg, located_in, varrow]
      - assert: [edwin, located_in, ashford]
      - assert: [edwin, located_in, varrow]
      - assert: [mara, located_in, carnath]
      - assert: [greg, name, Greg]
      - assert: [edwin, name, Edwin]
      - assert: [mara, name, Mara]
"#,
    )
    .unwrap()
}

fn rows(w: &World, text: &str) -> BTreeSet<Vec<Datum>> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap()
        .rows
        .into_iter()
        .collect()
}

fn refs(w: &World, handles: &[&str]) -> BTreeSet<Vec<Datum>> {
    handles
        .iter()
        .map(|h| vec![Datum::Value(Value::Ref(w.entity_by_handle(h).unwrap()))])
        .collect()
}

#[test]
fn union_deduplicates_and_matches_multi_body_rule() {
    let w = world();
    let text = "[:find ?c :at -215 :where (or [?c :located_in :varrow] [?c :located_in :ashford])]";
    let result = rows(&w, text);
    assert_eq!(result, refs(&w, &["greg", "edwin"]));
    assert_eq!(
        result,
        rows(
            &w,
            "[:find ?c :at -215 :rules [[(there ?c) [?c :located_in :varrow]] [(there ?c) [?c :located_in :ashford]]] :where (there ?c)]"
        )
    );
    assert!(
        prepare(&w, text)
            .unwrap()
            .plan
            .explain(&w)
            .contains("Union")
    );
}

#[test]
fn and_branches_join_and_can_nest_disjunction() {
    let w = world();
    assert_eq!(
        rows(
            &w,
            "[:find ?c :at -215 :where (or (and [?c :located_in :varrow] [?c :name \"Greg\"]) (and (or [?c :located_in :ashford] [?c :located_in :carnath]) [?c :name \"Mara\"]))]"
        ),
        refs(&w, &["greg", "mara"])
    );
}

#[test]
fn branches_must_bind_the_same_variables() {
    let w = world();
    for q in [
        "[:find ?c :where (or [?c :located_in :varrow] [?other :located_in :ashford])]",
        "[:find ?c :where (or [?c :located_in ?p] [?c :located_in :ashford])]",
    ] {
        let e = prepare(&w, q).unwrap_err();
        assert_ne!(
            e.kind,
            ErrorKind::Unsupported,
            "must check branch bindings: {e}"
        );
        assert!(e.to_string().contains("variable"), "{e}");
    }
}

#[test]
fn disjunction_in_rules_and_not() {
    let w = world();
    assert_eq!(
        rows(
            &w,
            "[:find ?c :rules [[(there ?c) (or [?c :located_in :varrow] [?c :located_in :ashford])]] :where (there ?c)]"
        ),
        refs(&w, &["greg", "edwin"])
    );
    assert_eq!(
        rows(
            &w,
            "[:find ?c :where [?c :name ?n] (not (or [?c :located_in :varrow] [?c :located_in :ashford]))]"
        ),
        refs(&w, &["mara"])
    );
}

#[test]
fn outer_bindings_constrain_each_branch() {
    let w = world();
    assert_eq!(
        rows(
            &w,
            "[:find ?c :where [?c :name \"Greg\"] (or [?c :located_in :varrow] [?c :located_in :ashford])]"
        ),
        refs(&w, &["greg"])
    );
}

#[test]
fn or_join_stays_explicitly_unsupported() {
    let w = world();
    let e = prepare(
        &w,
        "[:find ?c :where (or-join [?c] [?c :located_in :varrow] [?c :located_in :ashford])]",
    )
    .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Unsupported);
    assert!(e.to_string().contains("or-join"), "{e}");
}

#[test]
fn empty_or_and_empty_and_are_errors() {
    let w = world();
    for text in [
        "[:find ?c :where (or)]",
        "[:find ?c :where (or (and) [?c :name \"Greg\"])]",
    ] {
        assert!(prepare(&w, text).is_err(), "{text}");
    }
}

#[test]
fn branch_and_depth_limits_are_bounded() {
    let w = world();
    let branches = "[?c :located_in :varrow] ".repeat(kala_datalog::MAX_CLAUSES + 1);
    let e = prepare(&w, &format!("[:find ?c :where (or {branches})]")).unwrap_err();
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
    let q = format!(
        "[:find ?c :where {}[?c :name \"Greg\"]{}]",
        "(or ".repeat(kala_datalog::MAX_DEPTH + 1),
        ")".repeat(kala_datalog::MAX_DEPTH + 1)
    );
    assert_eq!(prepare(&w, &q).unwrap_err().kind, ErrorKind::Limit);
}

#[test]
fn nested_negation_before_disjunction_respects_expanded_plan_limit() {
    let w = world();
    let mut body = String::from("[:greg :name \"Greg\"]");
    for _ in 0..6 {
        body = format!(
            "(not {body}) (or [:greg :name \"x\"] [:greg :name \"y\"]) (or [:greg :name \"x\"] [:greg :name \"y\"])"
        );
    }
    let query = format!("[:find ?e :where [?e :name \"Greg\"] {body}]");
    let e = match prepare(&w, &query) {
        Err(e) => e,
        Ok(_) => panic!("nested negations cloned into every branch must be included in plan cost"),
    };
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
}

#[test]
fn variables_inside_not_do_not_count_as_disjunction_bindings() {
    let w = world();
    let query =
        "[:find ?e :where [?e :name \"Greg\"] (or [?x :name \"a\"] (not [?x :name \"b\"]))]";
    let e = prepare(&w, query).expect_err(
        "a positive binding and an existential inside not are different branch binding sets",
    );
    assert!(
        e.message.contains("variable") || e.message.contains("bind"),
        "{e}"
    );
}

#[test]
fn repeated_independent_disjunctions_cannot_expand_exponentially() {
    let w = world();
    let branches = "(or [?c :name \"Greg\"] [?c :name \"Greg\"]) ".repeat(14);
    let query = format!("[:find ?c :where {branches}]");
    let e = match prepare(&w, &query) {
        Err(e) => e,
        Ok(_) => panic!("many individually small disjunctions need a combined plan limit"),
    };
    assert_eq!(e.kind, ErrorKind::Limit, "{e}");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]
    #[test]
    fn random_locations_match_multi_body_rule(mask in prop::collection::vec((any::<bool>(), any::<bool>()), 1..12)) {
        let mut yaml = "schema:\n  node_types: [CHARACTER, PLACE]\n  attributes:\n    - { name: located_in, type: ref, cardinality: many }\nentities:\n  PLACE: [varrow, ashford]\n  CHARACTER: [".to_string();
        yaml.push_str(&(0..mask.len()).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", "));
        yaml.push_str("]\ntransactions:\n  - tick: -215\n    ops:\n");
        // Keep a valid operation even when every generated relation is absent.
        yaml.push_str("      - retract: [c0, located_in, varrow]\n");
        for (i, (varrow, ashford)) in mask.iter().enumerate() {
            if *varrow { yaml.push_str(&format!("      - assert: [c{i}, located_in, varrow]\n")); }
            if *ashford { yaml.push_str(&format!("      - assert: [c{i}, located_in, ashford]\n")); }
        }
        let w = load_fixture(&yaml).unwrap();
        let union = rows(&w, "[:find ?c :at -215 :where (or [?c :located_in :varrow] [?c :located_in :ashford])]");
        let rules = rows(&w, "[:find ?c :at -215 :rules [[(there ?c) [?c :located_in :varrow]] [(there ?c) [?c :located_in :ashford]]] :where (there ?c)]");
        prop_assert_eq!(&union, &rules);
        prop_assert_eq!(union.len(), mask.iter().filter(|(a,b)| *a || *b).count());
    }
}
