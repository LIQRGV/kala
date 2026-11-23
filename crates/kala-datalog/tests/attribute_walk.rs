//! `Query::attributes()` (AST) and `Plan::attribute_names()` (plan) must find
//! every attribute a query names, however deeply nested, so a caller can
//! refuse a query by attribute before running it.

use kala_core::{World, load_fixture};
use kala_datalog::{parse, prepare};
use std::collections::BTreeSet;

const VARROW: &str = include_str!("../../kala-core/tests/fixtures/varrow.yaml");

fn world() -> World {
    load_fixture(VARROW).expect("fixture loads")
}

fn ast_attrs(text: &str) -> BTreeSet<String> {
    parse(text)
        .unwrap_or_else(|e| panic!("{text}: {e}"))
        .attributes()
        .into_iter()
        .map(|a| a.name.clone())
        .collect()
}

fn plan_attrs(text: &str) -> BTreeSet<String> {
    let w = world();
    let p = prepare(&w, text).unwrap_or_else(|e| panic!("{text}: {e}"));
    p.plan
        .attribute_names()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn ast_finds_attribute_in_not_inside_or_inside_not_inside_rule_body() {
    let q = "[:find ?e :rules [[(deep ?e) [?e :name ?n] \
             (not (or (and [?e :alias ?a]) \
                      (and (not (or (and [?e :born_in ?b]) (and (during [?e :present ?x] ?s)))))))]] \
             :where [?e :located_in ?p] (deep ?e)]";
    assert_eq!(
        ast_attrs(q),
        set(&["located_in", "name", "alias", "born_in", "present"])
    );
}

#[test]
fn ast_finds_attributes_in_at_history_and_unused_rules() {
    let q = "[:find ?e :rules [[(unused ?e) [?e :ruled_by ?r]]] \
             :where (at -100 [?e :ally_of ?m]) [?e :name ?n ?t ?op]]";
    assert_eq!(ast_attrs(q), set(&["ruled_by", "ally_of", "name"]));
}

#[test]
fn ast_nesting_at_parser_depth_does_not_overflow_the_walk() {
    let mut inner = "[?e :alias ?a]".to_string();
    for _ in 0..20 {
        inner = format!("(not {inner})");
    }
    let q = format!("[:find ?e :where [?e :name ?n] {inner}]");
    // Either the parser refuses the depth, or the walk finds the leaf.
    if let Ok(parsed) = parse(&q) {
        let names: BTreeSet<_> = parsed
            .attributes()
            .into_iter()
            .map(|a| a.name.clone())
            .collect();
        assert_eq!(names, set(&["name", "alias"]));
    }
}

#[test]
fn plan_names_attributes_through_not_and_or() {
    let q = "[:find ?e :where [?e :name ?n] \
             (not [?e :alias ?a]) \
             (or (and [?e :located_in ?p]) (and [?e :born_in ?p]))]";
    assert_eq!(
        plan_attrs(q),
        set(&["name", "alias", "located_in", "born_in"])
    );
}

#[test]
fn plan_names_attributes_inside_fixpoint_base_and_recursive_rules() {
    let q = "[:find ?a ?b :rules [[(reach ?a ?b) [?a :located_in ?b]] \
             [(reach ?a ?c) [?a :ally_of ?b] (reach ?b ?c)]] \
             :where (reach ?a ?b)]";
    assert_eq!(plan_attrs(q), set(&["located_in", "ally_of"]));
}

#[test]
fn plan_names_attributes_under_a_not_inside_a_fixpoint_rule() {
    let q = "[:find ?a ?b :rules [[(reach ?a ?b) [?a :located_in ?b] (not [?a :alias \"x\"])] \
             [(reach ?a ?c) [?a :ally_of ?b] (reach ?b ?c)]] \
             :where (reach ?a ?b)]";
    assert_eq!(plan_attrs(q), set(&["located_in", "ally_of", "alias"]));
}

#[test]
fn plan_names_history_and_during_attributes() {
    let q = "[:find ?s :where (during [?e :present ?x] ?s) [?e :name ?n ?t ?op]]";
    assert_eq!(plan_attrs(q), set(&["present", "name"]));
}

#[test]
fn plan_leaves_the_handle_pseudo_attribute_out() {
    let q = r#"[:find ?p :where [?p :handle "varrow"]]"#;
    assert!(plan_attrs(q).is_empty(), "{:?}", plan_attrs(q));
    assert!(ast_attrs(q).contains("handle"), "AST still names :handle");
}

#[test]
fn plan_attributes_are_a_subset_of_ast_attributes() {
    let q = "[:find ?a :rules [[(r ?a) [?a :located_in ?b] (not [?a :alias ?x])]] \
             :where (r ?a) (or (and [?a :born_in ?b]) (and [?a :present ?b]))]";
    assert!(plan_attrs(q).is_subset(&ast_attrs(q)));
}
