//! Slice 6, item 2: executor edge cases decided by the tech lead. Each test
//! name records the decision it pins. Plans are hand-built; answers are
//! compared with the native kala-core API where there is one.

mod common;

use common::*;
use kala_core::{
    AttrDef, Cardinality, Correction, Direction, EntityId, Kind, Schema, Tick, TxId, Value,
    ValueType, World,
};
use kala_query::{
    Access, CanonSource, CmpOp, Limits, Op, Params, Plan, QueryError, TickSource, execute,
};
use proptest::prelude::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "score",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "knows",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
}

/// a, b, c: CHARACTER; p: PLACE. Everything at tick 0.
fn world() -> (World, EntityId, EntityId, EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let c = w.create_entity("CHARACTER", Some("c")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    let t = Some(Tick(0));
    for (e, n) in [(a, 1), (b, 2), (c, 3)] {
        w.assert(e, "age", Value::I64(n), t).unwrap();
        w.assert(e, "score", Value::F64(n as f64), t).unwrap();
        w.assert(e, "name", Value::Str(format!("n{n}")), t).unwrap();
    }
    w.assert(a, "bio", Value::Str("long".into()), t).unwrap();
    w.assert(a, "knows", Value::Ref(b), t).unwrap();
    (w, a, b, c, p)
}

fn run_with(
    w: &World,
    p: &Plan,
    params: &Params,
    lim: &Limits,
) -> Result<kala_query::ResultSet, QueryError> {
    execute(&w.at_canon(w.latest_tx()), p, params, lim)
}

fn run_p(w: &World, p: &Plan) -> Result<kala_query::ResultSet, QueryError> {
    run_with(w, p, &Params::new(), &Limits::default())
}

fn is_plan_err<T: std::fmt::Debug>(r: Result<T, QueryError>) -> bool {
    matches!(r, Err(QueryError::Plan(_)))
}

// ---------------------------------------------------------------- params

#[test]
fn every_declared_param_must_be_supplied_even_if_unused() {
    let (w, ..) = world();
    let mut p = one_col("x", node_scan(Op::Unit, "x", "CHARACTER"));
    p.params = vec!["unused".to_string()];
    assert!(matches!(run_p(&w, &p), Err(QueryError::MissingParam(n)) if n == "unused"));
    let mut given = Params::new();
    given.insert("unused".into(), Value::I64(1));
    assert_eq!(
        run_with(&w, &p, &given, &Limits::default())
            .unwrap()
            .rows
            .len(),
        3
    );
}

// ---------------------------------------------------------------- infeasible access / malformed plans

#[test]
fn handle_access_with_neither_side_bound_is_a_plan_error() {
    let (w, ..) = world();
    let p = plan(
        &["e", "h"],
        project(handle_expand(tv("e"), tv("h"), Access::Handle), &["e", "h"]),
    );
    assert!(is_plan_err(run_p(&w, &p)));
}

#[test]
fn v_access_on_an_unindexed_non_ref_attribute_is_a_plan_error() {
    let (w, ..) = world();
    let p = one_col(
        "e",
        expand(tv("e"), "bio", ts("long"), TickSource::Default, Access::V),
    );
    assert!(is_plan_err(run_p(&w, &p)));
}

#[test]
fn union_of_inputs_binding_different_variables_is_a_plan_error() {
    let (w, ..) = world();
    let u = Op::Union {
        inputs: vec![
            node_scan(Op::Unit, "x", "CHARACTER"),
            node_scan(Op::Unit, "y", "CHARACTER"),
        ],
    };
    assert!(is_plan_err(run_p(&w, &plan(&["x"], project(u, &["x"])))));
}

#[test]
fn filter_operand_any_or_unbound_var_is_a_plan_error() {
    let (w, ..) = world();
    let base = || node_scan(Op::Unit, "x", "CHARACTER");
    for (l, r) in [
        (Term::Any, ti(1)),
        (tv("x"), Term::Any),
        (tv("nope"), ti(1)),
        (tv("x"), tv("nope")),
    ] {
        let p = one_col("x", filter(base(), CmpOp::Eq, l.clone(), r.clone()));
        assert!(is_plan_err(run_p(&w, &p)), "{l:?} = {r:?}");
    }
}

#[test]
fn project_or_find_var_the_input_does_not_bind_is_a_plan_error() {
    let (w, ..) = world();
    let p = plan(
        &["x"],
        project(node_scan(Op::Unit, "x", "CHARACTER"), &["x", "ghost"]),
    );
    assert!(is_plan_err(run_p(&w, &p)));
    // find var missing from the (otherwise fine) projection
    let p = plan(
        &["ghost"],
        project(node_scan(Op::Unit, "x", "CHARACTER"), &["x"]),
    );
    assert!(is_plan_err(run_p(&w, &p)));
    // find var with no Project at all and not bound
    let p = plan(&["ghost"], node_scan(Op::Unit, "x", "CHARACTER"));
    assert!(is_plan_err(run_p(&w, &p)));
}

use kala_query::Term;

// ---------------------------------------------------------------- comparisons across types

#[test]
fn ordering_across_types_is_type_mismatch_but_eq_is_false_and_ne_true() {
    let (w, ..) = world();
    for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
        // constants
        let p = plan(&[], project(filter(Op::Unit, op, ti(1), ts("a")), &[]));
        assert!(
            matches!(run_p(&w, &p), Err(QueryError::TypeMismatch(_))),
            "const {op:?}"
        );
        // a bound i64 var against a string constant
        let p = one_col(
            "n",
            filter(
                expand(
                    tr(w.entity_by_handle("a").unwrap()),
                    "age",
                    tv("n"),
                    TickSource::Default,
                    Access::E,
                ),
                op,
                tv("n"),
                ts("a"),
            ),
        );
        assert!(
            matches!(run_p(&w, &p), Err(QueryError::TypeMismatch(_))),
            "var {op:?}"
        );
    }
    let age = || expand(tv("e"), "age", tv("n"), TickSource::Default, Access::Scan);
    let eq = one_col("n", filter(age(), CmpOp::Eq, tv("n"), ts("1")));
    assert_eq!(run_p(&w, &eq).unwrap().rows.len(), 0);
    let ne = one_col("n", filter(age(), CmpOp::Ne, tv("n"), ts("1")));
    assert_eq!(run_p(&w, &ne).unwrap().rows.len(), 3);
    // constants: Eq false -> no row; Ne true -> the one empty row
    let p = plan(
        &[],
        project(filter(Op::Unit, CmpOp::Eq, ti(1), ts("a")), &[]),
    );
    assert_eq!(run_p(&w, &p).unwrap().rows.len(), 0);
    let p = plan(
        &[],
        project(filter(Op::Unit, CmpOp::Ne, ti(1), ts("a")), &[]),
    );
    assert_eq!(run_p(&w, &p).unwrap().rows.len(), 1);
}

// ---------------------------------------------------------------- NaN

#[test]
fn nan_constant_in_value_position_matches_nothing_via_every_access() {
    let (w, a, ..) = world();
    let nan = Term::Const(Value::F64(f64::NAN));
    for (e, access) in [
        (tr(a), Access::E),
        (tv("e"), Access::V),
        (tv("e"), Access::Scan),
        (tr(a), Access::Scan),
    ] {
        let p = one_col(
            "e",
            expand_on(
                node_scan(Op::Unit, "e", "CHARACTER"),
                e.clone(),
                "score",
                nan.clone(),
                TickSource::Default,
                access,
            ),
        );
        let r = run_p(&w, &p).unwrap_or_else(|x| panic!("{access:?}: {x}"));
        assert!(r.rows.is_empty(), "{access:?} {e:?}");
    }
    // unbound entity, no NodeScan
    let p = one_col(
        "e",
        expand(
            tv("e"),
            "score",
            nan.clone(),
            TickSource::Default,
            Access::V,
        ),
    );
    assert!(run_p(&w, &p).unwrap().rows.is_empty());
    let p = one_col(
        "e",
        expand(tv("e"), "score", nan, TickSource::Default, Access::Scan),
    );
    assert!(run_p(&w, &p).unwrap().rows.is_empty());
}

// ---------------------------------------------------------------- NodeScan / NodeSeek on bound vars

#[test]
fn node_scan_on_a_bound_var_is_a_constraint() {
    let (w, a, _b, _c, p) = world();
    // x bound to a CHARACTER: kept
    let keep = one_col(
        "x",
        node_scan(node_seek(Op::Unit, "x", tr(a)), "x", "CHARACTER"),
    );
    assert_eq!(col0(&run_p(&w, &keep).unwrap()), vec![Value::Ref(a)]);
    // x bound to a PLACE: dropped by a CHARACTER scan, kept by a PLACE scan
    let drop = one_col(
        "x",
        node_scan(node_seek(Op::Unit, "x", tr(p)), "x", "CHARACTER"),
    );
    assert!(run_p(&w, &drop).unwrap().rows.is_empty());
    let keep = one_col(
        "x",
        node_scan(node_seek(Op::Unit, "x", tr(p)), "x", "PLACE"),
    );
    assert_eq!(col0(&run_p(&w, &keep).unwrap()), vec![Value::Ref(p)]);
}

#[test]
fn node_scan_on_a_var_bound_to_an_edge_id_matches_nothing() {
    let (mut w, a, b, ..) = world();
    let edge = w.edge_id(a, "knows", b).unwrap();
    w.assert(EntityId::from(edge), "rank", Value::I64(1), Some(Tick(0)))
        .unwrap();
    // ?x bound to the edge entity by a Scan of the edge property
    let src = || {
        expand(
            tv("x"),
            "rank",
            Term::Any,
            TickSource::Default,
            Access::Scan,
        )
    };
    let r = run_p(&w, &one_col("x", src())).unwrap();
    assert_eq!(col0(&r), vec![Value::Ref(EntityId::from(edge))]);
    for ty in ["CHARACTER", "PLACE"] {
        let p = one_col("x", node_scan(src(), "x", ty));
        assert!(run_p(&w, &p).unwrap().rows.is_empty(), "{ty}");
    }
}

#[test]
fn node_seek_on_a_bound_var_is_a_constraint() {
    let (w, a, b, ..) = world();
    let same = one_col("x", node_seek(node_seek(Op::Unit, "x", tr(a)), "x", tr(a)));
    assert_eq!(col0(&run_p(&w, &same).unwrap()), vec![Value::Ref(a)]);
    let other = one_col("x", node_seek(node_seek(Op::Unit, "x", tr(a)), "x", tr(b)));
    assert!(run_p(&w, &other).unwrap().rows.is_empty());
}

// ---------------------------------------------------------------- step budget

/// The least `max_steps` under which `p` still runs (linear search).
fn min_steps(w: &World, p: &Plan) -> u64 {
    (1..10_000u64)
        .find(|&s| {
            let l = Limits {
                max_steps: s,
                ..Limits::default()
            };
            run_with(w, p, &Params::new(), &l).is_ok()
        })
        .expect("plan runs under some budget")
}

#[test]
fn step_budget_counts_cached_scans_inside_a_nested_loop() {
    let (w, ..) = world();
    // 3 characters. The inner Scan (age = 99, matches nobody) is the same
    // read for every outer row, so a cache may serve it, but each repetition
    // still examines the 3 stored age pairs: that work must be charged every
    // time, not once. No inner row is produced, so the extra cost over the
    // outer-only plan is pure read cost: >= 3 repetitions x 3 pairs.
    let outer_only = plan(&["x"], node_scan(Op::Unit, "x", "CHARACTER"));
    let nested = plan(
        &["x"],
        expand_on(
            node_scan(Op::Unit, "x", "CHARACTER"),
            tv("y"),
            "age",
            ti(99),
            at(0),
            Access::Scan,
        ),
    );
    assert!(run_p(&w, &nested).unwrap().rows.is_empty());
    let base = min_steps(&w, &outer_only);
    let with_inner = min_steps(&w, &nested);
    assert!(
        with_inner >= base + 9,
        "3 repetitions of a 3-pair scan must cost >= 9 steps even if cached: outer-only needs {base}, nested {with_inner}"
    );
    // and the budget trips as an error at exactly one below
    let tight = Limits {
        max_steps: with_inner - 1,
        ..Limits::default()
    };
    assert!(matches!(
        run_with(&w, &nested, &Params::new(), &tight),
        Err(QueryError::StepBudget { .. })
    ));
}

// ---------------------------------------------------------------- edge properties

#[test]
fn expand_on_an_edge_entity_via_e_and_scan_matches_native() {
    let (mut w, a, b, ..) = world();
    let edge = w.edge_id(a, "knows", b).unwrap();
    let ee = EntityId::from(edge);
    w.assert(ee, "rank", Value::I64(1), Some(Tick(-5))).unwrap();
    w.retract(ee, "rank", Value::I64(1), Some(Tick(3))).unwrap();
    w.assert(ee, "rank", Value::I64(2), Some(Tick(3))).unwrap();
    let view = w.at_canon(w.latest_tx());
    for t in -8..8 {
        let native = view.as_of(ee, "rank", Tick(t)).unwrap();
        let via_e = run_p(
            &w,
            &one_col("v", expand(tr(ee), "rank", tv("v"), at(t), Access::E)),
        )
        .unwrap();
        assert_eq!(col0(&via_e), native, "E @{t}");
        let via_scan = run_p(
            &w,
            &plan(
                &["e", "v"],
                project(
                    expand(tv("e"), "rank", tv("v"), at(t), Access::Scan),
                    &["e", "v"],
                ),
            ),
        )
        .unwrap();
        let exp: Vec<Vec<Value>> = native
            .iter()
            .map(|v| vec![Value::Ref(ee), v.clone()])
            .collect();
        assert_eq!(via_scan.rows, exp, "Scan @{t}");
        // V on the i64 property too
        for v in &native {
            let r = run_p(
                &w,
                &one_col(
                    "e",
                    expand(tv("e"), "rank", Term::Const(v.clone()), at(t), Access::V),
                ),
            )
            .unwrap();
            assert_eq!(col0(&r), vec![Value::Ref(ee)], "V @{t}");
        }
    }
}

// ---------------------------------------------------------------- corrections

#[test]
fn correct_replace_moving_a_tick_matches_native_before_and_after() {
    let (mut w, a, _b, c, _p) = world();
    let r = w.assert(a, "knows", Value::Ref(c), Some(Tick(10))).unwrap();
    let before = w.latest_tx();
    w.correct(
        r.entries[0],
        Correction::Replace {
            value: None,
            tick: Some(Tick(-4)),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let after = w.latest_tx();
    assert!(after > before);
    for canon in [before, after] {
        let view = w.at_canon(canon);
        for t in -8..14 {
            let native: Vec<Value> = view
                .neighbors(a, "knows", Direction::Out, Tick(t))
                .unwrap()
                .into_iter()
                .map(|(n, _)| Value::Ref(n))
                .collect();
            for access in [Access::E, Access::Scan] {
                let p = one_col("v", expand(tr(a), "knows", tv("v"), at(t), access));
                let got = execute(&view, &p, &Params::new(), &Limits::default()).unwrap();
                let mut exp = native.clone();
                exp.sort();
                exp.dedup();
                assert_eq!(col0(&got), exp, "{access:?} canon {canon:?} @{t}");
            }
            // R access too, from c's side
            let rr = execute(
                &view,
                &one_col("s", expand(tv("s"), "knows", tr(c), at(t), Access::R)),
                &Params::new(),
                &Limits::default(),
            )
            .unwrap();
            assert_eq!(
                col0(&rr).contains(&Value::Ref(a)),
                native.contains(&Value::Ref(c)),
                "R canon {canon:?} @{t}"
            );
        }
    }
    // the move is visible: absent at 0 before, present after
    let at0 = |canon| {
        col0(
            &execute(
                &w.at_canon(canon),
                &one_col("v", expand(tr(a), "knows", tv("v"), at(0), Access::E)),
                &Params::new(),
                &Limits::default(),
            )
            .unwrap(),
        )
    };
    assert!(!at0(before).contains(&Value::Ref(c)));
    assert!(at0(after).contains(&Value::Ref(c)));
    let _ = TxId(0);
    let _ = CanonSource::Latest;
}

// ---------------------------------------------------------------- Union / Project property

#[derive(Clone, Debug)]
struct St {
    e: usize,
    v: usize,
    tick: i64,
    assert: bool,
}

fn st() -> impl Strategy<Value = St> {
    (0..4usize, 0..4usize, -4..5i64, any::<bool>()).prop_map(|(e, v, tick, assert)| St {
        e,
        v,
        tick,
        assert,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Union of two Expands (different ticks) == set union of the native
    /// results; Project drops the other columns and dedups.
    #[test]
    fn union_of_two_expands_is_the_set_union_of_native_results(
        steps in proptest::collection::vec(st(), 0..30),
        t1 in -6i64..7,
        t2 in -6i64..7,
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let ents: Vec<EntityId> = (0..4).map(|_| w.create_entity("CHARACTER", None).unwrap()).collect();
        for s in &steps {
            let v = Value::Ref(ents[s.v]);
            let _ = if s.assert { w.assert(ents[s.e], "knows", v, Some(Tick(s.tick))) }
                    else { w.retract(ents[s.e], "knows", v, Some(Tick(s.tick))) };
        }
        let view = w.at_canon(w.latest_tx());
        let native = |t: i64| -> Vec<Vec<Value>> {
            let mut out = Vec::new();
            for &e in &ents {
                for (n, _) in view.neighbors(e, "knows", Direction::Out, Tick(t)).unwrap() {
                    out.push(vec![Value::Ref(e), Value::Ref(n)]);
                }
            }
            out
        };
        let mut expected = native(t1);
        expected.extend(native(t2));
        let expected = sorted_rows(expected);
        let u = Op::Union { inputs: vec![
            expand(tv("e"), "knows", tv("v"), at(t1), Access::Scan),
            expand(tv("e"), "knows", tv("v"), at(t2), Access::Scan),
        ]};
        let got = execute(&view, &plan(&["e", "v"], project(u.clone(), &["e", "v"])), &Params::new(), &Limits::default()).unwrap();
        prop_assert_eq!(&got.rows, &expected);
        // Project to one column == the distinct first components
        let col = execute(&view, &one_col("e", u), &Params::new(), &Limits::default()).unwrap();
        let mut firsts: Vec<Value> = expected.iter().map(|r| r[0].clone()).collect();
        firsts.dedup();
        prop_assert_eq!(col0(&col), firsts);
    }
}
