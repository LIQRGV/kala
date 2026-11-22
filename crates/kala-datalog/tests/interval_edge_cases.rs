//! Slice 7, item 6 (CONFIRM): edge cases of interval sets in queries.
//!
//! Accepted implementation choice: `(during [:e :a ?v] ?s)` with only the
//! entity bound reads that entity's history (one row per value that ever
//! held), not a world scan.

use kala_core::*;
use kala_datalog::{ErrorKind, prepare};
use kala_query::{Datum, Limits, Params, QueryError, run};
use std::collections::{BTreeMap, BTreeSet};

type Rows = BTreeSet<Vec<Datum>>;

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(at("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(at("present", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(at(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(at("weight", ValueType::F64, Kind::State, Cardinality::Many))
        .attribute(
            at("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
}

struct W {
    w: World,
    ids: BTreeMap<&'static str, EntityId>,
}

fn fresh() -> W {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ids = BTreeMap::new();
    for h in ["edwin", "mara", "lisa", "tomas"] {
        ids.insert(h, w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    for h in ["varrow", "carnath"] {
        ids.insert(h, w.create_entity("PLACE", Some(h)).unwrap());
    }
    W { w, ids }
}

fn t(n: i64) -> Option<Tick> {
    Some(Tick(n))
}
fn iv(from: i64, until: Option<i64>) -> Interval {
    Interval {
        from: Tick(from),
        until: until.map(Tick),
    }
}
fn set(ivs: &[(i64, Option<i64>)]) -> Datum {
    Datum::Intervals(IntervalSet::from_intervals(
        ivs.iter().map(|&(a, b)| iv(a, b)),
    ))
}
fn rows(rs: &[&[Datum]]) -> Rows {
    rs.iter().map(|r| r.to_vec()).collect()
}
fn val(v: Value) -> Datum {
    Datum::Value(v)
}

fn try_run(
    w: &World,
    text: &str,
    params: &[(&str, Value)],
    lim: &Limits,
) -> Result<Rows, QueryError> {
    let p = prepare(w, text).unwrap_or_else(|e| panic!("prepare failed for {text}\n  {e}"));
    let ps: Params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    run(w, &p.plan, &ps, lim).map(|rs| rs.rows.into_iter().collect())
}
fn go(w: &World, text: &str) -> Rows {
    try_run(w, text, &[], &Limits::default())
        .unwrap_or_else(|e| panic!("run failed for {text}\n  {e}"))
}
fn kind(w: &World, text: &str) -> ErrorKind {
    match prepare(w, text) {
        Ok(_) => panic!("expected an error for {text}"),
        Err(e) => e.kind,
    }
}

/// Edwin in Varrow: A-300 R-250 A-200 R-150.
fn edwin_in_varrow(f: &mut W) {
    let (e, v) = (f.ids["edwin"], Value::Ref(f.ids["varrow"]));
    for (tick, assert) in [(-300, true), (-250, false), (-200, true), (-150, false)] {
        if assert {
            f.w.assert(e, "located_in", v.clone(), t(tick)).unwrap();
        } else {
            f.w.retract(e, "located_in", v.clone(), t(tick)).unwrap();
        }
    }
}

// ------------------------------------------------------------ edge entities

#[test]
fn during_on_an_edge_entity_reads_the_edge_property() {
    let mut f = fresh();
    let (e, c) = (f.ids["edwin"], f.ids["carnath"]);
    f.w.assert(e, "present", Value::Ref(c), t(-250)).unwrap();
    let edge = f.w.edge_id(e, "present", c).expect("edge interned");
    let eid = EntityId::from(edge);
    // disguised: true [-250,-220), then false from -220 on
    f.w.assert(eid, "disguised", Value::Bool(true), t(-250))
        .unwrap();
    f.w.retract(eid, "disguised", Value::Bool(true), t(-220))
        .unwrap();
    f.w.assert(eid, "disguised", Value::Bool(false), t(-220))
        .unwrap();
    let name = f.w.format_id(eid);
    assert!(name.starts_with("edge#"), "{name}");
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?s :where (during [{name} :disguised true] ?s)]")
        ),
        rows(&[&[set(&[(-250, Some(-220))])]])
    );
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?s :where (during [{name} :disguised false] ?s)]")
        ),
        rows(&[&[set(&[(-220, None)])]])
    );
    // only the entity bound: one row per value that ever held
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?v ?s :where (during [{name} :disguised ?v] ?s)]")
        ),
        rows(&[
            &[val(Value::Bool(true)), set(&[(-250, Some(-220))])],
            &[val(Value::Bool(false)), set(&[(-220, None)])],
        ])
    );
}

// ------------------------------------------------------------------- floats

#[test]
fn during_with_an_f64_value_and_negative_zero() {
    let mut f = fresh();
    let e = f.ids["edwin"];
    f.w.assert(e, "weight", Value::F64(1.5), t(0)).unwrap();
    f.w.retract(e, "weight", Value::F64(1.5), t(10)).unwrap();
    // -0.0 is normalized to 0.0 on write: one value, whichever way it is written
    f.w.assert(e, "weight", Value::F64(-0.0), t(5)).unwrap();
    f.w.retract(e, "weight", Value::F64(0.0), t(9)).unwrap();
    let want = rows(&[&[set(&[(5, Some(9))])]]);
    for lit in ["0.0", "-0.0"] {
        assert_eq!(
            go(
                &f.w,
                &format!("[:find ?s :where (during [:edwin :weight {lit}] ?s)]")
            ),
            want,
            "literal {lit}"
        );
    }
    assert_eq!(
        go(&f.w, "[:find ?s :where (during [:edwin :weight 1.5] ?s)]"),
        rows(&[&[set(&[(0, Some(10))])]])
    );
    // free value: two rows, and the zero is a positive zero
    let all = go(&f.w, "[:find ?v ?s :where (during [:edwin :weight ?v] ?s)]");
    assert_eq!(all.len(), 2);
    for r in &all {
        if let Datum::Value(Value::F64(x)) = &r[0] {
            assert!(
                x.to_bits() == 1.5f64.to_bits() || x.to_bits() == 0.0f64.to_bits(),
                "{x:?}"
            );
        } else {
            panic!("not an f64: {r:?}");
        }
    }
    // an integer literal in an f64 position is a float (slice-6 decisions)
    assert_eq!(
        go(&f.w, "[:find ?s :where (during [:edwin :weight 0] ?s)]"),
        want
    );
    assert_eq!(
        kind(&f.w, "[:find ?s :where (during [:edwin :weight \"0\"] ?s)]"),
        ErrorKind::TypeMismatch
    );
}

// ----------------------------------------------------------- inverse names

#[test]
fn an_inverse_name_with_only_the_entity_bound() {
    let mut f = fresh();
    let (e, l, m) = (f.ids["edwin"], f.ids["lisa"], f.ids["mara"]);
    f.w.assert(e, "parent_of", Value::Ref(m), t(-200)).unwrap();
    f.w.retract(e, "parent_of", Value::Ref(m), t(-100)).unwrap();
    f.w.assert(l, "parent_of", Value::Ref(m), t(-50)).unwrap();
    let want = rows(&[
        &[val(Value::Ref(e)), set(&[(-200, Some(-100))])],
        &[val(Value::Ref(l)), set(&[(-50, None)])],
    ]);
    assert_eq!(
        go(
            &f.w,
            "[:find ?p ?s :where (during [:mara :child_of ?p] ?s)]"
        ),
        want
    );
    // the same through the forward name
    assert_eq!(
        go(
            &f.w,
            "[:find ?p ?s :where (during [?p :parent_of :mara] ?s)]"
        ),
        want
    );
    // and the other direction: Edwin's children
    assert_eq!(
        go(
            &f.w,
            "[:find ?c ?s :where (during [:edwin :parent_of ?c] ?s)]"
        ),
        rows(&[&[val(Value::Ref(m)), set(&[(-200, Some(-100))])]])
    );
}

// ------------------------------------------------- rules with op variables

#[test]
fn a_rule_head_with_an_op_variable_called_with_assert() {
    let mut f = fresh();
    edwin_in_varrow(&mut f);
    let rule = "[(change ?e ?t ?op) [?e :located_in ?p ?t ?op]]";
    let q = |call: &str| format!("[:find ?t :rules [{rule}] :where (change :edwin ?t {call})]");
    let ticks = |ts: &[i64]| -> Rows { ts.iter().map(|n| vec![val(Value::I64(*n))]).collect() };
    assert_eq!(go(&f.w, &q(":assert")), ticks(&[-300, -200]));
    assert_eq!(go(&f.w, &q(":retract")), ticks(&[-250, -150]));
    assert_eq!(go(&f.w, &q("_")), ticks(&[-300, -250, -200, -150]));
    // the op comes back as an op datum
    assert_eq!(
        go(
            &f.w,
            &format!("[:find ?op :rules [{rule}] :where (change :edwin -300 ?op)]")
        ),
        rows(&[&[Datum::Op(kala_core::Op::Assert)]])
    );
}

// ------------------------------------------------------------------ params

#[test]
fn a_param_is_the_tick_of_contains() {
    let mut f = fresh();
    edwin_in_varrow(&mut f);
    let text =
        "[:find ?s :in ?tk :where (during [:edwin :located_in :varrow] ?s) [(contains ?s ?tk)]]";
    for (tick, hit) in [
        (-300, true),
        (-251, true),
        (-250, false),
        (-200, true),
        (-150, false),
    ] {
        let r = try_run(&f.w, text, &[("tk", Value::I64(tick))], &Limits::default()).unwrap();
        assert_eq!(r.len(), usize::from(hit), "tick {tick}");
    }
    // a non-integer param is a run-time parameter error, not a wrong answer
    let r = try_run(&f.w, text, &[("tk", Value::Bool(true))], &Limits::default());
    assert!(matches!(r, Err(QueryError::ParamType { .. })), "{r:?}");
}

#[test]
fn a_param_cannot_be_an_interval_set() {
    let mut f = fresh();
    edwin_in_varrow(&mut f);
    let during = "(during [:edwin :located_in :varrow] ?ie)";
    for text in [
        format!("[:find ?ie :in ?p :where {during} [(intersect ?p ?ie) ?r]]"),
        format!("[:find ?ie :in ?p :where {during} [(union ?ie ?p) ?r]]"),
        "[:find ?e :in ?p :where [?e :located_in :varrow] [(not-empty ?p)]]".to_string(),
        "[:find ?e :in ?p :where [?e :located_in :varrow] [(contains ?p 0)]]".to_string(),
        format!("[:find ?ie :in ?p :where {during} [(= ?ie ?p)]]"),
    ] {
        assert_eq!(kind(&f.w, &text), ErrorKind::TypeMismatch, "{text}");
    }
}

// --------------------------------------------------------------- functions

#[test]
fn two_functions_feeding_each_others_arguments_are_unbound() {
    let mut f = fresh();
    edwin_in_varrow(&mut f);
    let text = "[:find ?a :where (during [:edwin :located_in :varrow] ?x) \
                [(union ?x ?b) ?a] [(union ?x ?a) ?b]]";
    assert_eq!(kind(&f.w, text), ErrorKind::UnboundVariable);
    // a function consuming its own output
    let text = "[:find ?a :where (during [:edwin :located_in :varrow] ?x) [(union ?x ?a) ?a]]";
    assert_eq!(kind(&f.w, text), ErrorKind::UnboundVariable);
}

// -------------------------------------------------------------- op ordering

#[test]
fn less_than_on_two_ops_is_allowed_assert_before_retract() {
    let mut f = fresh();
    edwin_in_varrow(&mut f); // A-300 R-250 A-200 R-150
    let h = "[:edwin :located_in :varrow ?t1 ?o1] [:edwin :located_in :varrow ?t2 ?o2]";
    let pairs = |ps: &[(i64, i64)]| -> Rows {
        ps.iter()
            .map(|(a, b)| vec![val(Value::I64(*a)), val(Value::I64(*b))])
            .collect()
    };
    let q = |op: &str| format!("[:find ?t1 ?t2 :where {h} [({op} ?o1 ?o2)]]");
    // o1 = assert at -300 or -200; o2 = retract at -250 or -150
    assert_eq!(
        go(&f.w, &q("<")),
        pairs(&[(-300, -250), (-300, -150), (-200, -250), (-200, -150)])
    );
    assert_eq!(
        go(&f.w, &q(">")),
        pairs(&[(-250, -300), (-250, -200), (-150, -300), (-150, -200)])
    );
    // <= adds the equal-op pairs: (A,A) and (R,R), four each
    assert_eq!(go(&f.w, &q("<=")).len(), 12);
    assert_eq!(go(&f.w, &q(">=")).len(), 12);
}

// ------------------------------------------------------------ memory budget

#[test]
fn many_per_row_function_results_hit_the_memory_budget() {
    let mut f = fresh();
    let (e, v) = (f.ids["edwin"], Value::Ref(f.ids["varrow"]));
    {
        let mut tx = f.w.begin();
        // Edwin: 400 intervals [2i, 2i+1)
        for i in 0..400 {
            tx.assert(e, "located_in", v.clone(), t(2 * i)).unwrap();
            tx.retract(e, "located_in", v.clone(), t(2 * i + 1))
                .unwrap();
        }
        // 300 others, in Carnath: each row computes its own copy of Edwin's big set
        for n in 0..300 {
            let c = tx.create_entity("CHARACTER", None).unwrap();
            tx.assert(c, "located_in", Value::Ref(f.ids["carnath"]), t(n))
                .unwrap();
        }
        tx.commit().unwrap();
    }
    let text = "[:find ?c ?u :where (during [:edwin :located_in :varrow] ?big) \
                [?c :located_in :carnath] [(union ?big ?big) ?u]]";
    // default budget: 300 rows, each with the 400-interval set
    let ok = try_run(&f.w, text, &[], &Limits::default()).expect("fits the default budget");
    assert_eq!(ok.len(), 300);
    let small = Limits {
        max_bytes: 300_000,
        ..Limits::default()
    };
    let r = try_run(&f.w, text, &[], &small);
    assert!(matches!(r, Err(QueryError::MemoryBudget { .. })), "{r:?}");
}
