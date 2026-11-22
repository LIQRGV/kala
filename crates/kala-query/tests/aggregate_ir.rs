//! Slice 7, item 7 (RED): the executor runs `Op::Aggregate` (today it refuses
//! it with `QueryError::Plan`).
//!
//! Contract:
//! - `Aggregate { input, group, aggs }` groups the input's rows (the SET of
//!   distinct bindings of every variable `input` binds) by `group` and binds,
//!   per group, each `Agg { func, var, out }`. Its rows bind `group` plus every
//!   `out`. A group variable or an aggregated `var` that `input` doesn't bind
//!   is `QueryError::Plan`.
//! - `Count`: the number of bindings in the group; `CountDistinct`: the number
//!   of distinct `var` values; `Sum`: `var` summed over the bindings (a bag),
//!   i64 checked (`QueryError::Overflow(String)`) or f64; `Min`/`Max`: i64,
//!   f64 or string. Count results are `Value::I64`. Other types are
//!   `QueryError::TypeMismatch`.
//! - Empty input: no rows, with or without group variables.
//! - The row, step and byte limits apply.
//!
//! World: n0..n3. score (i64, many) and bonus (i64, many) at @0:
//!   n0 score {1, 2}  bonus {10, 20, 30}
//!   n1 score {2, 3}                       bonus {10}
//!   n2 score {5}                          bonus {}
//!   n3 (nothing)
//! name (string) n0 "b", n1 "a", n2 "c"; weight (f64) n0 0.5, n1 1.25;
//! parent_of (ref, many) n0->n1, n0->n2, n1->n2 at @0.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Agg, AggFn, Limits, Op, Params, QueryError, RelDef, ResultSet, run};
use proptest::prelude::*;

fn schema() -> Schema {
    let at = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .attribute(at("score", ValueType::I64, Kind::State, Cardinality::Many))
        .attribute(at("bonus", ValueType::I64, Kind::State, Cardinality::Many))
        .attribute(at("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(at("weight", ValueType::F64, Kind::State, Cardinality::One))
        .attribute(at("alive", ValueType::Bool, Kind::State, Cardinality::One))
        .attribute(at(
            "parent_of",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
}

fn world() -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let n: Vec<EntityId> = (0..4)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let t = Some(Tick(0));
    for (e, a, x) in [
        (0, "score", 1),
        (0, "score", 2),
        (1, "score", 2),
        (1, "score", 3),
        (2, "score", 5),
        (0, "bonus", 10),
        (0, "bonus", 20),
        (0, "bonus", 30),
        (1, "bonus", 10),
    ] {
        w.assert(n[e], a, Value::I64(x), t).unwrap();
    }
    for (e, s) in [(0, "b"), (1, "a"), (2, "c")] {
        w.assert(n[e], "name", Value::Str(s.into()), t).unwrap();
    }
    w.assert(n[0], "weight", Value::F64(0.5), t).unwrap();
    w.assert(n[1], "weight", Value::F64(1.25), t).unwrap();
    w.assert(n[0], "alive", Value::Bool(true), t).unwrap();
    for (a, c) in [(0, 1), (0, 2), (1, 2)] {
        w.assert(n[a], "parent_of", Value::Ref(n[c]), t).unwrap();
    }
    (w, n)
}

fn scan(attr: &str, e: &str, x: &str) -> Op {
    expand(tv(e), attr, tv(x), at(0), Access::Scan)
}

fn agg(input: Op, group: &[&str], aggs: &[(AggFn, &str, &str)]) -> Op {
    Op::Aggregate {
        input: b(input),
        group: group.iter().map(|g| v(g)).collect(),
        aggs: aggs
            .iter()
            .map(|(func, var, out)| Agg {
                func: *func,
                var: v(var),
                out: v(out),
            })
            .collect(),
    }
}

/// `Project` over the aggregate, in the shape the Datalog compiler emits.
fn exec(w: &World, cols: &[&str], root: Op) -> Result<ResultSet, QueryError> {
    run(
        w,
        &plan(cols, project(root, cols)),
        &Params::new(),
        &Limits::default(),
    )
}
fn rows_of(w: &World, cols: &[&str], root: Op) -> Vec<Vec<Value>> {
    value_rows(&exec(w, cols, root).unwrap_or_else(|e| panic!("run failed: {e}")))
}
fn i(n: i64) -> Value {
    Value::I64(n)
}
fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

#[test]
fn count_per_group() {
    let (w, n) = world();
    // score pairs (e, x): n0 has 2, n1 has 2, n2 has 1
    let got = rows_of(
        &w,
        &["e", "c"],
        agg(scan("score", "e", "x"), &["e"], &[(AggFn::Count, "x", "c")]),
    );
    assert_eq!(
        got,
        vec![
            vec![r(n[0]), i(2)],
            vec![r(n[1]), i(2)],
            vec![r(n[2]), i(1)]
        ]
    );
    // group by the value: 2 is held by n0 and n1
    let got = rows_of(
        &w,
        &["x", "c"],
        agg(scan("score", "e", "x"), &["x"], &[(AggFn::Count, "e", "c")]),
    );
    assert_eq!(
        got,
        vec![
            vec![i(1), i(1)],
            vec![i(2), i(2)],
            vec![i(3), i(1)],
            vec![i(5), i(1)]
        ]
    );
}

#[test]
fn no_group_variables_is_one_group_over_everything() {
    let (w, _) = world();
    let got = rows_of(
        &w,
        &["c", "d", "s", "lo", "hi"],
        agg(
            scan("score", "e", "x"),
            &[],
            &[
                (AggFn::Count, "x", "c"),
                (AggFn::CountDistinct, "x", "d"),
                (AggFn::Sum, "x", "s"),
                (AggFn::Min, "x", "lo"),
                (AggFn::Max, "x", "hi"),
            ],
        ),
    );
    // bindings (e,x): (0,1)(0,2)(1,2)(1,3)(2,5): 5 of them; distinct x {1,2,3,5};
    // the bag sum 1+2+2+3+5 = 13
    assert_eq!(got, vec![vec![i(5), i(4), i(13), i(1), i(5)]]);
}

#[test]
fn count_counts_bindings_and_count_distinct_counts_values() {
    let (w, n) = world();
    // input bindings (e, x, y) = score x bonus per e: n0 2x3 = 6, n1 2x1 = 2, n2 none
    let input = || join(scan("score", "e", "x"), scan("bonus", "e", "y"));
    let got = rows_of(
        &w,
        &["e", "c", "dx", "dy"],
        agg(
            input(),
            &["e"],
            &[
                (AggFn::Count, "x", "c"),
                (AggFn::CountDistinct, "x", "dx"),
                (AggFn::CountDistinct, "y", "dy"),
            ],
        ),
    );
    assert_eq!(
        got,
        vec![
            vec![r(n[0]), i(6), i(2), i(3)],
            vec![r(n[1]), i(2), i(2), i(1)],
        ]
    );
}

#[test]
fn sum_is_a_bag_over_the_bindings() {
    let (w, n) = world();
    // n0: x in {1,2} each paired with 3 bonuses: (1+2) * 3 = 9; n1: (2+3) * 1 = 5.
    // The sum of the DISTINCT x per e would be 3 and 5.
    let got = rows_of(
        &w,
        &["e", "s"],
        agg(
            join(scan("score", "e", "x"), scan("bonus", "e", "y")),
            &["e"],
            &[(AggFn::Sum, "x", "s")],
        ),
    );
    assert_eq!(got, vec![vec![r(n[0]), i(9)], vec![r(n[1]), i(5)]]);
    // across groups the equal x = 2 of n0 and n1 are two bindings: 2 + 2
    let got = rows_of(
        &w,
        &["x", "s"],
        agg(scan("score", "e", "x"), &["x"], &[(AggFn::Sum, "x", "s")]),
    );
    assert_eq!(
        got,
        vec![
            vec![i(1), i(1)],
            vec![i(2), i(4)],
            vec![i(3), i(3)],
            vec![i(5), i(5)]
        ]
    );
}

#[test]
fn sum_min_max_of_f64_and_min_max_of_strings() {
    let (w, _) = world();
    let got = rows_of(
        &w,
        &["s", "lo", "hi"],
        agg(
            scan("weight", "e", "x"),
            &[],
            &[
                (AggFn::Sum, "x", "s"),
                (AggFn::Min, "x", "lo"),
                (AggFn::Max, "x", "hi"),
            ],
        ),
    );
    assert_eq!(
        got,
        vec![vec![Value::F64(1.75), Value::F64(0.5), Value::F64(1.25)]]
    );
    let got = rows_of(
        &w,
        &["lo", "hi"],
        agg(
            scan("name", "e", "x"),
            &[],
            &[(AggFn::Min, "x", "lo"), (AggFn::Max, "x", "hi")],
        ),
    );
    assert_eq!(
        got,
        vec![vec![Value::Str("a".into()), Value::Str("c".into())]]
    );
    // count and count-distinct take any type: refs, strings, bools
    let got = rows_of(
        &w,
        &["c"],
        agg(
            scan("parent_of", "e", "x"),
            &[],
            &[(AggFn::Count, "x", "c")],
        ),
    );
    assert_eq!(got, vec![vec![i(3)]]);
    let got = rows_of(
        &w,
        &["d"],
        agg(
            scan("parent_of", "e", "x"),
            &[],
            &[(AggFn::CountDistinct, "x", "d")],
        ),
    );
    assert_eq!(got, vec![vec![i(2)]]); // n1, n2
    let got = rows_of(
        &w,
        &["c", "d"],
        agg(
            scan("alive", "e", "x"),
            &[],
            &[(AggFn::Count, "x", "c"), (AggFn::CountDistinct, "e", "d")],
        ),
    );
    assert_eq!(got, vec![vec![i(1), i(1)]]);
}

#[test]
fn empty_input_gives_no_rows() {
    let (w, _) = world();
    // nothing exists at tick -1
    let empty = || expand(tv("e"), "score", tv("x"), at(-1), Access::Scan);
    for group in [&["e"][..], &[][..]] {
        let cols: Vec<&str> = group.iter().copied().chain(["c"]).collect();
        let rs = exec(&w, &cols, agg(empty(), group, &[(AggFn::Count, "x", "c")])).unwrap();
        assert!(rs.rows.is_empty(), "group {group:?}");
    }
    let rs = exec(&w, &["s"], agg(empty(), &[], &[(AggFn::Sum, "x", "s")])).unwrap();
    assert!(rs.rows.is_empty());
}

#[test]
fn sum_overflow_is_an_overflow_error_and_extremes_that_fit_are_fine() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let c = w.create_entity("CHARACTER", None).unwrap();
    let t = Some(Tick(0));
    w.assert(a, "score", Value::I64(i64::MAX), t).unwrap();
    w.assert(c, "score", Value::I64(i64::MIN), t).unwrap();
    let sum = || agg(scan("score", "e", "x"), &[], &[(AggFn::Sum, "x", "s")]);
    assert_eq!(rows_of(&w, &["s"], sum()), vec![vec![i(-1)]]);
    let mut w2 = World::in_memory(schema()).unwrap();
    for x in [i64::MAX, 1] {
        let e = w2.create_entity("CHARACTER", None).unwrap();
        w2.assert(e, "score", Value::I64(x), t).unwrap();
    }
    let r = exec(&w2, &["s"], sum());
    assert!(matches!(r, Err(QueryError::Overflow(_))), "{r:?}");
    // a negative overflow too
    let mut w3 = World::in_memory(schema()).unwrap();
    for x in [i64::MIN, -1] {
        let e = w3.create_entity("CHARACTER", None).unwrap();
        w3.assert(e, "score", Value::I64(x), t).unwrap();
    }
    let r = exec(&w3, &["s"], sum());
    assert!(matches!(r, Err(QueryError::Overflow(_))), "{r:?}");
}

#[test]
fn type_errors_and_unbound_variables() {
    let (w, _) = world();
    for (what, input, func, var) in [
        ("min of refs", scan("parent_of", "e", "x"), AggFn::Min, "x"),
        ("max of refs", scan("parent_of", "e", "x"), AggFn::Max, "x"),
        ("sum of refs", scan("parent_of", "e", "x"), AggFn::Sum, "x"),
        ("sum of strings", scan("name", "e", "x"), AggFn::Sum, "x"),
        ("min of bools", scan("alive", "e", "x"), AggFn::Min, "x"),
        ("sum of bools", scan("alive", "e", "x"), AggFn::Sum, "x"),
    ] {
        let r = exec(&w, &["o"], agg(input, &[], &[(func, var, "o")]));
        assert!(
            matches!(r, Err(QueryError::TypeMismatch(_))),
            "{what}: {r:?}"
        );
    }
    let r = exec(
        &w,
        &["o"],
        agg(scan("score", "e", "x"), &[], &[(AggFn::Count, "nope", "o")]),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
    let r = exec(
        &w,
        &["g", "o"],
        agg(scan("score", "e", "x"), &["g"], &[(AggFn::Count, "x", "o")]),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
}

#[test]
fn limits_apply_to_the_aggregate() {
    let (w, _) = world();
    let root = || {
        project(
            agg(scan("score", "e", "x"), &["e"], &[(AggFn::Count, "x", "c")]),
            &["e", "c"],
        )
    };
    let p = plan(&["e", "c"], root());
    let tight = Limits {
        max_rows: 2,
        ..Limits::default()
    };
    // three groups
    let r = run(&w, &p, &Params::new(), &tight);
    assert!(matches!(r, Err(QueryError::RowLimit { .. })), "{r:?}");
    let tight = Limits {
        max_steps: 3,
        ..Limits::default()
    };
    let r = run(&w, &p, &Params::new(), &tight);
    assert!(matches!(r, Err(QueryError::StepBudget { .. })), "{r:?}");
}

// ---------------------------------------------------------------- property

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Aggregate over `score x bonus` joined on the entity, against a naive
    /// grouping of the `attr_scan` rows read straight from the world.
    #[test]
    fn aggregate_matches_a_naive_grouping(
        facts in proptest::collection::vec((0usize..4, any::<bool>(), -3i64..4, 0i64..6, any::<bool>()), 0..40),
        tick in -1i64..7,
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let n: Vec<EntityId> = (0..4).map(|_| w.create_entity("CHARACTER", None).unwrap()).collect();
        for (e, is_score, x, t, is_assert) in facts {
            let attr = if is_score { "score" } else { "bonus" };
            // same-tick conflicts are rejected by the store: skip them
            let _ = if is_assert {
                w.assert(n[e], attr, Value::I64(x), Some(Tick(t)))
            } else {
                w.retract(n[e], attr, Value::I64(x), Some(Tick(t)))
            };
        }
        // the naive model: read each entity's values at the tick
        let view = w.at_canon(w.latest_tx());
        let holds = |e: EntityId, attr: &str| -> Vec<i64> {
            let mut vs: Vec<i64> = view
                .attr_scan(attr, Tick(tick))
                .unwrap()
                .into_iter()
                .filter(|(who, _)| *who == e)
                .map(|(_, v)| match v { Value::I64(x) => x, other => panic!("{other:?}") })
                .collect();
            vs.sort();
            vs.dedup();
            vs
        };
        let mut want: Vec<Vec<Value>> = vec![];
        for &e in &n {
            let (xs, ys) = (holds(e, "score"), holds(e, "bonus"));
            if xs.is_empty() || ys.is_empty() {
                continue;
            }
            want.push(vec![
                Value::Ref(e),
                i((xs.len() * ys.len()) as i64),                 // count x
                i(xs.len() as i64),                               // count-distinct x
                i(xs.iter().sum::<i64>() * ys.len() as i64),      // bag sum x
                i(*xs.iter().min().unwrap()),
                i(*xs.iter().max().unwrap()),
            ]);
        }
        want.sort();
        let input = join(
            expand(tv("e"), "score", tv("x"), at(tick), Access::Scan),
            expand(tv("e"), "bonus", tv("y"), at(tick), Access::Scan),
        );
        let got = rows_of(
            &w,
            &["e", "c", "d", "s", "lo", "hi"],
            agg(input, &["e"], &[
                (AggFn::Count, "x", "c"),
                (AggFn::CountDistinct, "x", "d"),
                (AggFn::Sum, "x", "s"),
                (AggFn::Min, "x", "lo"),
                (AggFn::Max, "x", "hi"),
            ]),
        );
        prop_assert_eq!(got, want);
    }
}

// ------------------------------------------------- tech-lead decisions (edge cases)

fn f64_schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "weight",
        ValueType::F64,
        Kind::State,
        Cardinality::Many,
    ))
}

fn f64_world(xs: &[f64]) -> World {
    let mut w = World::in_memory(f64_schema()).unwrap();
    for &x in xs {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "weight", Value::F64(x), Some(Tick(0))).unwrap();
    }
    w
}

fn weight_sum(w: &World) -> Result<ResultSet, QueryError> {
    exec(
        w,
        &["s"],
        agg(
            expand(tv("e"), "weight", tv("x"), at(0), Access::Scan),
            &[],
            &[(AggFn::Sum, "x", "s")],
        ),
    )
}

#[test]
fn an_output_variable_that_is_already_bound_is_a_plan_error() {
    let (w, _) = world();
    // out = "x" is bound by the input
    let r = exec(
        &w,
        &["e", "x"],
        agg(scan("score", "e", "x"), &["e"], &[(AggFn::Count, "e", "x")]),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
    // out collides with a group variable
    let r = exec(
        &w,
        &["e"],
        agg(scan("score", "e", "x"), &["e"], &[(AggFn::Count, "x", "e")]),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
    // two aggregates with the same out
    let r = exec(
        &w,
        &["o"],
        agg(
            scan("score", "e", "x"),
            &[],
            &[(AggFn::Count, "x", "o"), (AggFn::Sum, "x", "o")],
        ),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
}

#[test]
fn an_aggregate_inside_a_fixpoint_rule_is_a_plan_error() {
    let (w, _) = world();
    // c(e, k) = count of scores per e: an aggregate in a relation's rule
    let rule = project(
        agg(scan("score", "e", "x"), &["e"], &[(AggFn::Count, "x", "k")]),
        &["e", "k"],
    );
    let def = RelDef {
        name: "c".into(),
        vars: vec![v("e"), v("k")],
        rules: vec![rule],
    };
    let call = Op::Rel {
        input: b(Op::Unit),
        name: "c".into(),
        args: vec![tv("e"), tv("k")],
    };
    let root = Op::Fixpoint {
        rels: vec![def],
        body: b(call),
    };
    let r = run(
        &w,
        &plan(&["e", "k"], project(root, &["e", "k"])),
        &Params::new(),
        &Limits::default(),
    );
    assert!(matches!(r, Err(QueryError::Plan(_))), "{r:?}");
}

#[test]
fn an_f64_sum_reaching_infinity_is_a_normal_result() {
    let w = f64_world(&[f64::MAX, f64::MAX]);
    let rs = weight_sum(&w).unwrap();
    assert_eq!(value_rows(&rs), vec![vec![Value::F64(f64::INFINITY)]]);
    let w = f64_world(&[-f64::MAX, -f64::MAX]);
    let rs = weight_sum(&w).unwrap();
    assert_eq!(value_rows(&rs), vec![vec![Value::F64(f64::NEG_INFINITY)]]);
    // an infinite input alone sums to itself
    let w = f64_world(&[f64::INFINITY, 1.0]);
    let rs = weight_sum(&w).unwrap();
    assert_eq!(value_rows(&rs), vec![vec![Value::F64(f64::INFINITY)]]);
}

#[test]
fn an_f64_sum_that_is_nan_is_an_overflow_error() {
    // +inf + -inf = NaN, which no value may be
    let w = f64_world(&[f64::INFINITY, f64::NEG_INFINITY]);
    let r = weight_sum(&w);
    assert!(matches!(r, Err(QueryError::Overflow(_))), "{r:?}");
}

#[test]
fn an_i64_overflow_in_a_later_group_fails_the_whole_query() {
    let mut w = World::in_memory(schema()).unwrap();
    let t = Some(Tick(0));
    // groups in entity order: two fine ones, then the one that overflows
    let fine: Vec<EntityId> = (0..2)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    for (k, e) in fine.iter().enumerate() {
        w.assert(*e, "score", Value::I64(k as i64 + 1), t).unwrap();
    }
    let bad = w.create_entity("CHARACTER", None).unwrap();
    for x in [i64::MAX, 5] {
        w.assert(bad, "score", Value::I64(x), t).unwrap();
    }
    let r = exec(
        &w,
        &["e", "s"],
        agg(scan("score", "e", "x"), &["e"], &[(AggFn::Sum, "x", "s")]),
    );
    assert!(matches!(r, Err(QueryError::Overflow(_))), "{r:?}");
}
