//! Slice 7, item 7 (confirm): tech-lead decisions on aggregates, pinned.
//! Compiler-level cases here; the IR-level ones are in
//! `kala-query/tests/aggregate_ir.rs`.
//!
//! World: edwin, mara, greg (CHARACTER, `gold` 10 / 20 / 30, `weight` f64
//! 1.5 / 2.5 / 4.0, `located_in` varrow, varrow, ashford), PLACE varrow and
//! ashford, all at tick 0.

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_query::{Datum, Limits, Params, ResultSet, run};
use std::collections::BTreeSet;

fn world() -> (World, Vec<EntityId>) {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(at("gold", ValueType::I64, Kind::State, Cardinality::One))
        .attribute(at("weight", ValueType::F64, Kind::State, Cardinality::One));
    let mut w = World::in_memory(schema).unwrap();
    let mut ids = vec![];
    for h in ["edwin", "mara", "greg"] {
        ids.push(w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    for h in ["varrow", "ashford"] {
        ids.push(w.create_entity("PLACE", Some(h)).unwrap());
    }
    let t = Some(Tick(0));
    for (k, (g, x, p)) in [(10, 1.5, 3), (20, 2.5, 3), (30, 4.0, 4)]
        .into_iter()
        .enumerate()
    {
        w.assert(ids[k], "gold", Value::I64(g), t).unwrap();
        w.assert(ids[k], "weight", Value::F64(x), t).unwrap();
        w.assert(ids[k], "located_in", Value::Ref(ids[p]), t)
            .unwrap();
    }
    (w, ids)
}

fn run_q(w: &World, text: &str) -> ResultSet {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    run(w, &p.plan, &Params::new(), &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}

fn rows(w: &World, text: &str) -> BTreeSet<Vec<Datum>> {
    run_q(w, text).rows.into_iter().collect()
}

fn i(n: i64) -> Datum {
    Datum::Value(Value::I64(n))
}
fn fl(x: f64) -> Datum {
    Datum::Value(Value::F64(x))
}
fn r(e: EntityId) -> Datum {
    Datum::Value(Value::Ref(e))
}

fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.kind,
    }
}

#[test]
fn a_variable_typed_only_by_a_rule_head_can_be_summed() {
    let (w, _) = world();
    // ?g is bound only by the call `(rich ?c ?g)`; its type (i64) comes from
    // the rule's body through the head
    let q = "[:find (sum ?g) (max ?g) (min ?g) :at 0 \
             :rules [[(rich ?c ?g) [?c :gold ?g]]] \
             :where (rich ?c ?g)]";
    assert_eq!(rows(&w, q), BTreeSet::from([vec![i(60), i(30), i(10)]]));
    // an f64 attribute through a rule head sums as f64
    let q = "[:find (sum ?x) :at 0 \
             :rules [[(heavy ?c ?x) [?c :weight ?x]]] \
             :where (heavy ?c ?x)]";
    assert_eq!(rows(&w, q), BTreeSet::from([vec![fl(8.0)]]));
}

#[test]
fn a_rule_head_typed_ref_variable_still_refuses_sum_min_max() {
    let (w, _) = world();
    for agg in ["sum", "min", "max"] {
        let q = format!(
            "[:find ({agg} ?p) :at 0 \
             :rules [[(where-is ?c ?p) [?c :located_in ?p]]] \
             :where (where-is ?c ?p)]"
        );
        assert_eq!(kind(&w, &q), ErrorKind::TypeMismatch, "{q}");
    }
    // count and count-distinct take any type
    let q = "[:find (count-distinct ?p) :at 0 \
             :rules [[(where-is ?c ?p) [?c :located_in ?p]]] \
             :where (where-is ?c ?p)]";
    assert_eq!(rows(&w, q), BTreeSet::from([vec![i(2)]]));
}

#[test]
fn a_variable_both_grouped_and_aggregated() {
    let (w, ids) = world();
    // group by ?c; each ?c has one binding, so count is 1 and count-distinct 1
    let q = "[:find ?c (count ?c) (count-distinct ?c) :at 0 :where [?c :located_in ?p]]";
    assert_eq!(
        rows(&w, q),
        BTreeSet::from([
            vec![r(ids[0]), i(1), i(1)],
            vec![r(ids[1]), i(1), i(1)],
            vec![r(ids[2]), i(1), i(1)],
        ])
    );
    // grouped on ?g and summed over ?g: each group's sum is the value itself
    let q = "[:find ?g (sum ?g) (min ?g) (max ?g) :at 0 :where [?c :gold ?g]]";
    assert_eq!(
        rows(&w, q),
        BTreeSet::from([
            vec![i(10), i(10), i(10), i(10)],
            vec![i(20), i(20), i(20), i(20)],
            vec![i(30), i(30), i(30), i(30)],
        ])
    );
}

#[test]
fn explain_prints_aggregate_columns_with_a_question_mark_prefix() {
    // accepted wart: the column name is `(count ?c)`, and explain prefixes
    // every column with `?`, so it reads `?(count ?c)`
    let (w, _) = world();
    let p = prepare(&w, "[:find ?p (count ?c) :at 0 :where [?c :located_in ?p]]").unwrap();
    let text = p.plan.explain(&w);
    assert!(text.contains("?(count ?c)"), "{text}");
    assert!(text.contains("?p"), "{text}");
}

#[test]
fn an_f64_sum_per_group_through_the_compiler() {
    let (w, ids) = world();
    // varrow: 1.5 + 2.5 = 4.0; ashford: 4.0
    let q = "[:find ?p (sum ?x) :at 0 :where [?c :located_in ?p] [?c :weight ?x]]";
    assert_eq!(
        rows(&w, q),
        BTreeSet::from([vec![r(ids[3]), fl(4.0)], vec![r(ids[4]), fl(4.0)]])
    );
}
