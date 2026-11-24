//! Parameters need a final rule-position type; recursive inference may defer it.

use kala_core::{Value, World};
use kala_datalog::{ErrorKind, Span, prepare};
use kala_query::{Limits, Params, run};
use std::collections::BTreeSet;

fn world() -> World {
    kala_fixture::load_fixture(include_str!("../../kala-core/tests/fixtures/varrow.yaml")).unwrap()
}

fn rows(world: &World, query: &str, params: &Params) -> BTreeSet<Vec<Value>> {
    let prepared = prepare(world, query).unwrap_or_else(|e| panic!("{query}\n{e}"));
    run(world, &prepared.plan, params, &Limits::default())
        .unwrap()
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect()
}

fn param(world: &World) -> Params {
    [(
        "p".into(),
        Value::Ref(world.entity_by_handle("edwin").unwrap()),
    )]
    .into()
}

fn assert_untyped(query: &str, rule: &str, argument: usize) {
    let error = prepare(&world(), query).unwrap_err();
    assert_eq!(error.kind, ErrorKind::TypeMismatch, "{error}");
    assert!(
        error
            .message
            .contains(&format!("argument {argument} of `{rule}`")),
        "{error}"
    );
    assert!(error.message.contains("?p"), "{error}");
    assert!(error.message.contains("type"), "{error}");
    assert!(!error.message.contains("is not used"), "{error}");
    let start = query.rfind("?p").unwrap();
    assert_eq!(error.span, Some(Span::new(start, start + 2)), "{error}");
    let prefix = &query[..start];
    let line = prefix.chars().filter(|&c| c == '\n').count() + 1;
    let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
    assert!(
        error
            .to_string()
            .starts_with(&format!("line {line}, column {column}:")),
        "{error}"
    );
}

#[test]
fn parameter_in_untyped_self_recursive_rule_names_call_and_cause() {
    assert_untyped(
        "[:find ?e :in ?p :rules [[(r ?x) (r ?x)]] :where [?e :ally_of _] (r ?p)]",
        "r",
        1,
    );
}

#[test]
fn parameter_in_untyped_mutually_recursive_rule_names_call_and_cause() {
    assert_untyped(
        "[:find ?e :in ?p\n :rules [[(a ?x) (b ?x)] [(b ?x) (a ?x)]]\n :where [?e :ally_of _]\n (a ?p)]",
        "a",
        1,
    );
}

#[test]
fn captured_parameter_in_untyped_recursive_body_names_call_and_cause() {
    assert_untyped(
        "[:find ?e :in ?p :rules [[(r ?x) (r ?x) (r ?p)]] :where [?e :ally_of _] (r ?e)]",
        "r",
        1,
    );
}

#[test]
fn parameter_in_second_untyped_rule_position_names_its_position() {
    assert_untyped(
        "[:find ?e :in ?p :rules [[(r ?x ?y) (r ?x ?y)]] :where [?e :ally_of _] (r ?e ?p)]",
        "r",
        2,
    );
}

#[test]
fn parameter_typed_elsewhere_still_rejects_an_untyped_rule_position() {
    assert_untyped(
        "[:find ?e :in ?p :rules [[(r ?x) (r ?x)]] :where [?p :ally_of ?e] (r ?p)]",
        "r",
        1,
    );
}

#[test]
fn untyped_rule_positions_still_accept_variables_wildcards_and_constants() {
    let world = world();
    for arg in ["?e", "_", ":edwin", "7", "\"untyped\"", "true", "1.5"] {
        for tick in [i64::MIN, -210, 0, i64::MAX] {
            let query = format!(
                "[:find ?e :at {tick} :rules [[(r ?x) (r ?x)]] :where [?e :ally_of _] (r {arg})]"
            );
            assert!(rows(&world, &query, &Params::new()).is_empty(), "{query}");
        }
    }
}

#[test]
fn parameter_in_typed_rule_position_returns_edwins_allies() {
    let world = world();
    let expected: BTreeSet<_> = ["greg", "tomas"]
        .into_iter()
        .map(|name| vec![Value::Ref(world.entity_by_handle(name).unwrap())])
        .collect();
    assert_eq!(
        rows(
            &world,
            "[:find ?a :in ?p :at -210 :rules [[(ally ?x ?y) [?x :ally_of ?y]]] :where (ally ?p ?a)]",
            &param(&world),
        ),
        expected,
    );
}

#[test]
fn parameter_waits_for_transitively_inferred_rule_position() {
    let world = world();
    let query = "[:find ?e :in ?p :at -210 :rules [[(a ?x) (b ?x)] [(b ?x) [?x :ally_of _]]] :where [?e :ally_of _] (a ?p)]";
    let expected = rows(
        &world,
        "[:find ?e :at -210 :where [?e :ally_of _]]",
        &Params::new(),
    );
    assert!(!expected.is_empty());
    assert_eq!(rows(&world, query, &param(&world)), expected);
}

#[test]
fn captured_parameter_waits_for_recursive_rule_type_seeds() {
    let world = world();
    // The first definition sees (b ?p) before b's fact pattern supplies its type.
    let query = "[:find ?e :in ?p :at -210 :rules [[(a ?x) (b ?x) (b ?p)] [(b ?x) (a ?x)] [(b ?x) [?x :ally_of _]]] :where (a ?e)]";
    let expected = rows(
        &world,
        "[:find ?e :at -210 :where [?e :ally_of _]]",
        &Params::new(),
    );
    assert!(!expected.is_empty());
    assert_eq!(rows(&world, query, &param(&world)), expected);
}

#[test]
fn genuinely_unused_parameter_keeps_its_error() {
    let query = "[:find ?e :in ?p :where [?e :ally_of _]]";
    let error = prepare(&world(), query).unwrap_err();
    assert_eq!(error.kind, ErrorKind::UnboundVariable);
    assert_eq!(error.message, "parameter ?p is not used by the query");
}
