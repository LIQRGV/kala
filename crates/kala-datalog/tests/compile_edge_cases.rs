//! Slice 6 items 4-6 CONFIRM: edge cases pinning accepted implementer choices.
use kala_core::*;
use kala_datalog::{ErrorKind, ParamKind, prepare};
use kala_query::{Limits, Op, Params, Plan, TickSource, run};
use std::collections::BTreeSet;

const YAML: &str = r#"
schema:
  node_types: [CHARACTER, PLACE]
  attributes:
    - { name: name,    type: string }
    - { name: bio,     type: string, indexed: false }
    - { name: age,     type: i64 }
    - { name: seen_at, type: i64 }
    - { name: likes,   type: ref, cardinality: many }
entities:
  CHARACTER: [ann, bob]
  PLACE: [home]
transactions:
  - tick: 0
    ops:
      - { assert: [ann, name, "Ann"] }
      - { assert: [bob, name, "Bob"] }
      - { assert: [ann, bio, "tall"] }
      - { assert: [bob, bio, "short"] }
      - { assert: [ann, age, 30] }
      - { assert: [ann, seen_at, 25] }
      - { assert: [bob, seen_at, 5] }
      - { assert: [ann, likes, bob], tick: 3 }
  - tick: 1
    ops:
      - { create: [CHARACTER, late] }
      - { assert: [late, name, "Late"] }
"#;

fn world() -> &'static World {
    thread_local!(static W: &'static World =
        Box::leak(Box::new(load_fixture(YAML).expect("fixture"))));
    W.with(|w| *w)
}

fn rows_p(w: &World, text: &str, params: &[(&str, Value)]) -> BTreeSet<Vec<Value>> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare {text}: {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, &Limits::default())
        .unwrap_or_else(|e| panic!("run {text}: {e}"))
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|d| d.as_value().unwrap().clone())
                .collect()
        })
        .collect()
}
fn rows(w: &World, text: &str) -> BTreeSet<Vec<Value>> {
    rows_p(w, text, &[])
}
fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(p) => panic!("expected error for {text}, got\n{}", p.plan.explain(w)),
        Err(e) => e.kind,
    }
}
fn id(w: &World, h: &str) -> Value {
    Value::Ref(w.entity_by_handle(h).unwrap())
}

#[test]
fn duplicate_in_name_is_syntax() {
    let w = world();
    assert_eq!(
        kind(w, "[:find ?e :in $ ?a ?a :where [?e :name ?n]]"),
        ErrorKind::Syntax
    );
}

#[test]
fn underscore_as_predicate_operand_is_syntax() {
    let w = world();
    assert_eq!(
        kind(w, "[:find ?a :where [?e :age ?a] [(= _ ?a)]]"),
        ErrorKind::Syntax
    );
    assert_eq!(
        kind(w, "[:find ?a :where [?e :age ?a] [(< ?a _)]]"),
        ErrorKind::Syntax
    );
}

#[test]
fn find_var_that_is_a_param_is_unbound() {
    let w = world();
    assert_eq!(
        kind(w, "[:find ?w :in $ ?w :where [?e :name ?n]]"),
        ErrorKind::UnboundVariable
    );
}

#[test]
fn two_untyped_params_only_compared_are_type_mismatch() {
    let w = world();
    assert_eq!(
        kind(w, "[:find ?e :in $ ?a ?b :where [?e :name ?n] [(= ?a ?b)]]"),
        ErrorKind::TypeMismatch
    );
}

#[test]
fn param_used_as_tick_and_as_i64_value_does_not_panic() {
    let w = world();
    let t = "[:find ?e :in $ ?x :at ?x :where [?e :age ?x]]";
    match prepare(w, t) {
        Ok(p) => {
            eprintln!("PIN param tick+i64 value: Ok, kinds {:?}", p.params);
            let ps: Params = [("x".to_string(), Value::I64(30))].into();
            let r = run(w, &p.plan, &ps, &Limits::default());
            eprintln!("PIN run -> {:?}", r.as_ref().map(|r| r.rows.clone()));
        }
        Err(e) => {
            eprintln!("PIN param tick+i64 value: Err {:?} {e}", e.kind);
            assert_eq!(e.kind, ErrorKind::TypeMismatch);
        }
    }
}

fn expand_ticks(op: &Op, out: &mut Vec<TickSource>) {
    match op {
        Op::Unit => {}
        Op::NodeScan { input, .. }
        | Op::NodeSeek { input, .. }
        | Op::Filter { input, .. }
        | Op::Project { input, .. } => expand_ticks(input, out),
        Op::Expand { input, tick, .. } => {
            out.push(tick.clone());
            expand_ticks(input, out);
        }
        Op::Join { left, right } => {
            expand_ticks(left, out);
            expand_ticks(right, out);
        }
        Op::Union { inputs } => inputs.iter().for_each(|i| expand_ticks(i, out)),
        _ => {}
    }
}
fn ticks_of(p: &Plan) -> Vec<TickSource> {
    let mut v = vec![];
    expand_ticks(&p.root, &mut v);
    v
}

#[test]
fn plain_patterns_carry_default_tick_source_under_at() {
    let w = world();
    let p = prepare(w, "[:find ?n :at 7 :where [?e :name ?n] [?e :age ?a]]").unwrap();
    assert_eq!(p.plan.default_tick, TickSource::Const(Tick(7)));
    let ts = ticks_of(&p.plan);
    assert_eq!(ts.len(), 2);
    assert!(ts.iter().all(|t| *t == TickSource::Default), "{ts:?}");
}

#[test]
fn at_var_bound_only_by_its_own_pattern_is_unbound() {
    let w = world();
    assert_eq!(
        kind(w, "[:find ?e :where (at ?t [?e :seen_at ?t])]"),
        ErrorKind::UnboundVariable
    );
}

#[test]
fn handle_pattern_whose_entity_is_bound_only_by_a_later_at_pattern_works() {
    let w = world();
    // ann seen_at 25 -> nobody likes ann; bob seen_at 5 -> ann likes bob at 5.
    let got = rows(
        w,
        r#"[:find ?h :where [?e :kala_handle ?h] [?x :seen_at ?t] (at ?t [?e :likes ?x])]"#,
    );
    assert_eq!(got, BTreeSet::from([vec![Value::Str("ann".into())]]));
}

#[test]
fn id_constant_defined_at_latest_but_read_under_older_canon_is_zero_rows() {
    let w = world();
    let late = w.entity_by_handle("late").unwrap();
    let label = w.format_id(late);
    let latest = w.latest_tx().0;
    assert!(latest >= 2);
    let q = |c: u64| format!("[:find ?n :canon {c} :where [{label} :name ?n]]");
    assert_eq!(
        rows(w, &q(latest)),
        BTreeSet::from([vec![Value::Str("Late".into())]])
    );
    assert!(rows(w, &q(latest - 1)).is_empty());
    assert!(rows(w, &q(0)).is_empty());
}

#[test]
fn string_constant_of_257_bytes_on_indexed_attribute_never_panics() {
    let w = world();
    let big = "a".repeat(257);
    let t = format!("[:find ?e :where [?e :name \"{big}\"]]");
    // Pinned: compiles, runs, 0 rows (no error).
    let p = prepare(w, &t).expect("257-byte constant compiles");
    let rs = run(w, &p.plan, &Params::new(), &Limits::default()).expect("runs");
    assert!(rs.rows.is_empty());
}

#[test]
fn unindexed_string_with_bound_value_scans_and_equals_native() {
    let w = world();
    let p = prepare(w, r#"[:find ?e :where [?e :bio "tall"]]"#).unwrap();
    let ex = p.plan.explain(w);
    assert!(!ex.contains("via V"), "{ex}");
    assert!(ex.contains("via scan"), "{ex}");
    let got = rows(w, r#"[:find ?e :where [?e :bio "tall"]]"#);
    let want: BTreeSet<Vec<Value>> = w
        .attr_scan("bio", Tick(0))
        .unwrap()
        .into_iter()
        .filter(|(_, v)| *v == Value::Str("tall".into()))
        .map(|(e, _)| vec![Value::Ref(e)])
        .collect();
    assert_eq!(got, want);
    assert_eq!(got, BTreeSet::from([vec![id(w, "ann")]]));
}

#[test]
fn cartesian_chain_of_three_unrelated_patterns_is_the_product() {
    let w = world();
    // names: 3 (ann bob late), bios: 2, ages: 1 -> 6
    let got = rows(
        w,
        "[:find ?a ?b ?c :where [?a :name ?x] [?b :bio ?y] [?c :age ?z]]",
    );
    assert_eq!(got.len(), 3 * 2);
}

fn same_var_text(n: usize) -> String {
    let mut t = String::from("[:find ?n :where");
    for _ in 0..n {
        t.push_str(" [?e :name ?n]");
    }
    t.push(']');
    t
}
fn chain_text(n: usize) -> String {
    let mut t = String::from("[:find ?v0 :where");
    for i in 0..n {
        t.push_str(&format!(" [?v{i} :likes ?v{}]", i + 1));
    }
    t.push(']');
    t
}

#[test]
fn query_with_256_patterns_compiles_and_plans() {
    let w = world();
    for t in [same_var_text(256), chain_text(256)] {
        let p = prepare(w, &t).expect("256 patterns is the limit, accepted");
        assert!(!p.plan.explain(w).is_empty());
    }
    let _ = ParamKind::Tick;
}

/// RED if the executor recurses once per pattern: a query the compiler accepts
/// (<= MAX_PATTERNS) must run, not abort the process. Runs on the default 2 MiB
/// test thread stack.
#[test]
fn query_with_256_patterns_runs() {
    let w = world();
    let p = prepare(w, &same_var_text(256)).unwrap();
    let got = run(w, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(got.rows.len(), 3);
    let p = prepare(w, &chain_text(256)).unwrap();
    let rs = run(w, &p.plan, &Params::new(), &Limits::default()).unwrap();
    assert!(rs.rows.is_empty());
}
