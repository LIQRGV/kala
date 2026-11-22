//! Slice 6, item 2: the executor (docs/slice-6-decisions.md, "Item 2:
//! executor"). Plans are built by hand; results are compared with the native
//! kala-core API.
//!
//! Expected public paths (re-exported at the crate root):
//! `kala_query::{execute, run, Params, Limits, ResultSet, QueryError}`
//!
//! ```text
//! pub type Params = BTreeMap<String, Value>;
//! pub struct Limits { pub max_rows: usize, pub max_steps: u64 }   // + Default
//! pub struct ResultSet { pub columns: Vec<Var>, pub rows: Vec<Vec<Value>>,
//!                        pub tick: Tick, pub canon: TxId }
//! pub fn execute(&View, &Plan, &Params, &Limits) -> Result<ResultSet, QueryError>
//! pub fn run(&World, &Plan, &Params, &Limits) -> Result<ResultSet, QueryError>
//! ```
//! Also relies on `Value: Ord` and `View::attr_scan` (kala-core item 0).

mod common;

use std::collections::BTreeMap;

use common::*;
use kala_core::{
    AttrDef, Cardinality, Direction, EntityId, Kind, Schema, Tick, TxId, Value, ValueType, World,
};
use kala_query::{
    Access, AttrRef, CanonSource, CmpOp, Limits, Op, Params, Plan, QueryError, ResultSet, Term,
    TickSource, execute, run,
};

fn no_params() -> Params {
    Params::new()
}

fn exec_latest(w: &World, p: &Plan) -> Result<ResultSet, QueryError> {
    execute(
        &w.at_canon(w.latest_tx()),
        p,
        &no_params(),
        &Limits::default(),
    )
}

fn ok_rows(w: &World, p: &Plan) -> Vec<Vec<Value>> {
    value_rows(&exec_latest(w, p).expect("query runs"))
}

fn eid(w: &World, handle: &str) -> EntityId {
    w.entity_by_handle(handle).expect("handle exists")
}

/// A small world: ages (i64), names, trusts (directed, with a self pair),
/// parent_of/child_of (inverse), ally_of (bidirectional), marks (i64).
fn small() -> (World, EntityId, EntityId, EntityId) {
    let schema = Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "mark",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "trusts",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .inverse("child_of"),
        )
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        );
    let mut w = World::in_memory(schema).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let bb = w.create_entity("CHARACTER", Some("b")).unwrap();
    let c = w.create_entity("CHARACTER", Some("c")).unwrap();
    let t = Some(Tick(0));
    for (e, n, name) in [(a, 10, "a"), (bb, 20, "b"), (c, 30, "c")] {
        w.assert(e, "age", Value::I64(n), t).unwrap();
        w.assert(e, "name", Value::Str(name.into()), t).unwrap();
    }
    w.assert(a, "trusts", Value::Ref(a), t).unwrap();
    w.assert(a, "trusts", Value::Ref(bb), t).unwrap();
    w.assert(a, "parent_of", Value::Ref(bb), t).unwrap();
    w.assert(a, "ally_of", Value::Ref(bb), t).unwrap();
    w.assert(c, "ally_of", Value::Ref(a), t).unwrap();
    (w, a, bb, c)
}

const TICKS: [i64; 14] = [
    -500, -310, -300, -280, -260, -240, -210, -208, -200, -190, -150, -60, 590, 700,
];

// ---------------------------------------------------------------- Edwin's allies

#[test]
fn edwins_allies_all_accesses_equal_native_at_many_ticks() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    for t in TICKS {
        let expected: Vec<EntityId> = w
            .neighbors(edwin, "ally_of", Direction::Both, Tick(t))
            .unwrap()
            .into_iter()
            .map(|(e, _)| e)
            .collect();
        let expected = ref_rows(expected);
        // E (entity bound), R (value-side read of the same pattern flipped:
        // [?a ally_of edwin]), Scan (nothing bound but a constraint)
        let via_e = one_col("a", expand(tr(edwin), "ally_of", tv("a"), at(t), Access::E));
        let via_r = one_col("a", expand(tv("a"), "ally_of", tr(edwin), at(t), Access::R));
        let via_scan = one_col(
            "a",
            expand(tr(edwin), "ally_of", tv("a"), at(t), Access::Scan),
        );
        let via_scan_flipped = one_col(
            "a",
            expand(tv("a"), "ally_of", tr(edwin), at(t), Access::Scan),
        );
        for (name, p) in [
            ("E", via_e),
            ("R", via_r),
            ("Scan", via_scan),
            ("Scan flipped", via_scan_flipped),
        ] {
            let rs = exec_latest(&w, &p).unwrap();
            assert_eq!(col0(&rs), expected, "access {name} at tick {t}");
            assert_eq!(rs.columns, vec![v("a")]);
        }
    }
    // sanity: the answer is not trivially empty everywhere
    assert!(
        !w.neighbors(edwin, "ally_of", Direction::Both, Tick(-210))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn edwins_allies_at_every_canon_via_execute() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    for tx in 0..=w.latest_tx().0 + 2 {
        let view = w.at_canon(TxId(tx));
        for t in [-300, -210, -100, 600] {
            let expected: Vec<EntityId> = view
                .neighbors(edwin, "ally_of", Direction::Both, Tick(t))
                .map(|v| v.into_iter().map(|(e, _)| e).collect())
                .unwrap_or_default();
            let p = one_col("a", expand(tr(edwin), "ally_of", tv("a"), at(t), Access::E));
            let rs = execute(&view, &p, &no_params(), &Limits::default());
            if let Ok(rs) = rs {
                assert_eq!(col0(&rs), ref_rows(expected), "canon {tx} tick {t}");
                assert_eq!(rs.canon, view.canon());
            }
        }
    }
}

#[test]
fn run_with_const_canon_matches_at_canon_and_past_latest_is_latest() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let latest = w.latest_tx();
    let mk = |canon: CanonSource| {
        let mut p = one_col(
            "a",
            expand(tr(edwin), "ally_of", tv("a"), at(-60), Access::E),
        );
        p.canon = canon;
        p
    };
    for tx in 1..=latest.0 {
        let expected = ref_rows(
            w.at_canon(TxId(tx))
                .neighbors(edwin, "ally_of", Direction::Both, Tick(-60))
                .unwrap()
                .into_iter()
                .map(|(e, _)| e)
                .collect(),
        );
        let rs = run(
            &w,
            &mk(CanonSource::Const(tx)),
            &no_params(),
            &Limits::default(),
        )
        .unwrap();
        assert_eq!(col0(&rs), expected, "canon {tx}");
        assert_eq!(rs.canon, TxId(tx));
    }
    // the corrected world differs at an older canon (the retcon is tx 11)
    let old = run(
        &w,
        &mk(CanonSource::Const(10)),
        &no_params(),
        &Limits::default(),
    )
    .unwrap();
    let new = run(
        &w,
        &mk(CanonSource::Latest),
        &no_params(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(new.canon, latest);
    // past latest = latest
    let past = run(
        &w,
        &mk(CanonSource::Const(latest.0 + 1000)),
        &no_params(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(past.canon, latest);
    assert_eq!(past.rows, new.rows);
    let _ = old;
}

#[test]
fn run_with_param_canon() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let mut p = one_col(
        "a",
        expand(tr(edwin), "ally_of", tv("a"), at(-60), Access::E),
    );
    p.params = vec!["c".into()];
    p.canon = CanonSource::Param("c".into());
    let mut params = Params::new();
    params.insert("c".into(), Value::I64(3));
    let rs = run(&w, &p, &params, &Limits::default()).unwrap();
    assert_eq!(rs.canon, TxId(3));
    let expected = ref_rows(
        w.at_canon(TxId(3))
            .neighbors(edwin, "ally_of", Direction::Both, Tick(-60))
            .unwrap()
            .into_iter()
            .map(|(e, _)| e)
            .collect(),
    );
    assert_eq!(col0(&rs), expected);
    // missing and mistyped canon params
    assert!(matches!(
        run(&w, &p, &no_params(), &Limits::default()),
        Err(QueryError::MissingParam(n)) if n == "c"
    ));
    params.insert("c".into(), Value::Str("x".into()));
    assert!(matches!(
        run(&w, &p, &params, &Limits::default()),
        Err(QueryError::ParamType { .. })
    ));
    params.insert("c".into(), Value::I64(-1));
    assert!(matches!(
        run(&w, &p, &params, &Limits::default()),
        Err(QueryError::ParamType { .. } | QueryError::TypeMismatch(_))
    ));
}

#[test]
fn execute_ignores_plan_canon() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let mut p = one_col(
        "a",
        expand(tr(edwin), "ally_of", tv("a"), at(-60), Access::E),
    );
    p.canon = CanonSource::Const(1);
    let rs = execute(
        &w.at_canon(w.latest_tx()),
        &p,
        &no_params(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(rs.canon, w.latest_tx());
    assert_eq!(
        col0(&rs),
        ref_rows(
            w.neighbors(edwin, "ally_of", Direction::Both, Tick(-60))
                .unwrap()
                .into_iter()
                .map(|(e, _)| e)
                .collect()
        )
    );
}

// ---------------------------------------------------------------- who is in Varrow

#[test]
fn who_is_in_varrow_via_r_and_scan_equals_native() {
    let w = varrow();
    let varrow_e = eid(&w, "varrow");
    for t in TICKS
        .iter()
        .copied()
        .chain([-400, -395, -270, -230, -95, 0])
    {
        let expected = ref_rows(
            w.neighbors(varrow_e, "located_in", Direction::In, Tick(t))
                .unwrap()
                .into_iter()
                .map(|(e, _)| e)
                .collect(),
        );
        let via_r = one_col(
            "p",
            expand(tv("p"), "located_in", tr(varrow_e), at(t), Access::R),
        );
        let via_scan = one_col(
            "p",
            expand(tv("p"), "located_in", tr(varrow_e), at(t), Access::Scan),
        );
        assert_eq!(
            col0(&exec_latest(&w, &via_r).unwrap()),
            expected,
            "R at {t}"
        );
        assert_eq!(
            col0(&exec_latest(&w, &via_scan).unwrap()),
            expected,
            "Scan at {t}"
        );
    }
    // not trivially empty
    assert!(
        !w.neighbors(varrow_e, "located_in", Direction::In, Tick(-300))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn where_is_greg_overlap_reports_both_places() {
    // Greg is in Varrow and Ashford at some canon/tick of the fixture
    // (cardinality one is a label): the executor reports both, like the native API.
    let w = varrow();
    let greg = eid(&w, "greg");
    for tx in 1..=w.latest_tx().0 {
        let view = w.at_canon(TxId(tx));
        for t in [250, 300, 601, 700] {
            let expected = ref_rows(
                view.as_of(greg, "located_in", Tick(t))
                    .unwrap()
                    .into_iter()
                    .filter_map(|x| if let Value::Ref(e) = x { Some(e) } else { None })
                    .collect(),
            );
            let p = one_col(
                "p",
                expand(tr(greg), "located_in", tv("p"), at(t), Access::E),
            );
            let rs = execute(&view, &p, &no_params(), &Limits::default()).unwrap();
            assert_eq!(col0(&rs), expected, "canon {tx} tick {t}");
        }
    }
}

// ---------------------------------------------------------------- two moments, tick sources

#[test]
fn join_of_two_moments_is_allies_at_both() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    for (t1, t2) in [(-210, -200), (-280, -260), (-300, -60), (-208, -208)] {
        let allies = |t: i64| -> Vec<EntityId> {
            w.neighbors(edwin, "ally_of", Direction::Both, Tick(t))
                .unwrap()
                .into_iter()
                .map(|(e, _)| e)
                .collect()
        };
        let (a1, a2) = (allies(t1), allies(t2));
        let both: Vec<EntityId> = a1.iter().copied().filter(|x| a2.contains(x)).collect();
        let p = one_col(
            "a",
            join(
                expand(tr(edwin), "ally_of", tv("a"), at(t1), Access::E),
                expand(tr(edwin), "ally_of", tv("a"), at(t2), Access::E),
            ),
        );
        assert_eq!(
            col0(&exec_latest(&w, &p).unwrap()),
            ref_rows(both),
            "{t1} & {t2}"
        );
    }
    // allies at -210 who are not allies at -200 needs Difference: not in the IR.
    // (-210) alone is non-empty, so the join test above is not vacuous:
    assert!(
        !w.neighbors(edwin, "ally_of", Direction::Both, Tick(-210))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn tick_param_matches_const_tick() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let mut p = one_col(
        "a",
        expand(
            tr(edwin),
            "ally_of",
            tv("a"),
            TickSource::Param("t".into()),
            Access::E,
        ),
    );
    p.params = vec!["t".into()];
    for t in TICKS {
        let mut params = Params::new();
        params.insert("t".into(), Value::I64(t));
        let rs = execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()).unwrap();
        let c = one_col("a", expand(tr(edwin), "ally_of", tv("a"), at(t), Access::E));
        assert_eq!(rs.rows, exec_latest(&w, &c).unwrap().rows, "tick {t}");
    }
}

#[test]
fn entity_param_and_param_in_value_position() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let mut p = one_col(
        "a",
        expand(tp("who"), "ally_of", tv("a"), at(-210), Access::E),
    );
    p.params = vec!["who".into()];
    let mut params = Params::new();
    params.insert("who".into(), Value::Ref(edwin));
    let rs = execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()).unwrap();
    let c = one_col(
        "a",
        expand(tr(edwin), "ally_of", tv("a"), at(-210), Access::E),
    );
    assert_eq!(rs.rows, exec_latest(&w, &c).unwrap().rows);
    // value position, R access
    let mut p = one_col(
        "a",
        expand(tv("a"), "ally_of", tp("who"), at(-210), Access::R),
    );
    p.params = vec!["who".into()];
    let rs = execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()).unwrap();
    assert_eq!(rs.rows, exec_latest(&w, &c).unwrap().rows);
}

#[test]
fn tick_var_bound_by_an_earlier_expand() {
    let (mut w, a, bb, c) = small();
    // a has marks -300 and -208; a's allies are asserted at -300 (b) and -208 (c)
    w.assert(a, "mark", Value::I64(-300), Some(Tick(0)))
        .unwrap();
    w.assert(a, "mark", Value::I64(-208), Some(Tick(0)))
        .unwrap();
    // fresh allies of `a` at distinct ticks
    let d = w.create_entity("CHARACTER", Some("d")).unwrap();
    let e = w.create_entity("CHARACTER", Some("e")).unwrap();
    w.assert(a, "ally_of", Value::Ref(d), Some(Tick(-300)))
        .unwrap();
    w.assert(a, "ally_of", Value::Ref(e), Some(Tick(-208)))
        .unwrap();
    let p = plan(
        &["t", "x"],
        project(
            expand_on(
                expand(tr(a), "mark", tv("t"), TickSource::Default, Access::E),
                tr(a),
                "ally_of",
                tv("x"),
                TickSource::Var(v("t")),
                Access::E,
            ),
            &["t", "x"],
        ),
    );
    let rows = ok_rows(&w, &p);
    // at -300: allies {d}; at -208: {d, e} (alliances persist); b and c arrived at tick 0
    let expected = sorted_rows(vec![
        vec![Value::I64(-300), Value::Ref(d)],
        vec![Value::I64(-208), Value::Ref(d)],
        vec![Value::I64(-208), Value::Ref(e)],
    ]);
    assert_eq!(rows, expected);
    let _ = (bb, c);
}

#[test]
fn tick_var_errors() {
    let (w, a, _b, _c) = small();
    // never bound
    let p = one_col(
        "x",
        expand(tr(a), "trusts", tv("x"), TickSource::Var(v("t")), Access::E),
    );
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::TickNotBound(n)) if n == "t"));
    // bound to a string
    let p = one_col(
        "x",
        expand_on(
            expand(tr(a), "name", tv("t"), TickSource::Default, Access::E),
            tr(a),
            "trusts",
            tv("x"),
            TickSource::Var(v("t")),
            Access::E,
        ),
    );
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::TypeMismatch(_))
    ));
}

// ---------------------------------------------------------------- sets, columns

#[test]
fn union_of_same_plan_twice_is_once_and_rows_are_sorted() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let one = expand(tr(edwin), "ally_of", tv("a"), at(-210), Access::E);
    let single = one_col("a", one.clone());
    let twice = one_col(
        "a",
        Op::Union {
            inputs: vec![one.clone(), one],
        },
    );
    let s = exec_latest(&w, &single).unwrap();
    let t = exec_latest(&w, &twice).unwrap();
    assert_eq!(s.rows, t.rows);
    assert!(!s.rows.is_empty());
    let mut sorted = s.rows.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(s.rows, sorted);
}

#[test]
fn union_of_different_moments_is_set_union() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let allies = |t: i64| -> Vec<EntityId> {
        w.neighbors(edwin, "ally_of", Direction::Both, Tick(t))
            .unwrap()
            .into_iter()
            .map(|(e, _)| e)
            .collect()
    };
    let mut all = allies(-280);
    all.extend(allies(-208));
    let p = one_col(
        "a",
        Op::Union {
            inputs: vec![
                expand(tr(edwin), "ally_of", tv("a"), at(-280), Access::E),
                expand(tr(edwin), "ally_of", tv("a"), at(-208), Access::E),
            ],
        },
    );
    assert_eq!(col0(&exec_latest(&w, &p).unwrap()), ref_rows(all));
}

#[test]
fn project_drops_vars_and_dedups_and_columns_equal_find() {
    let (w, a, _b, _c) = small();
    // (?x trusts ?y) has rows (a,a) and (a,b); projecting ?x leaves one row
    let both = plan(
        &["x", "y"],
        project(
            expand(
                tv("x"),
                "trusts",
                tv("y"),
                TickSource::Default,
                Access::Scan,
            ),
            &["x", "y"],
        ),
    );
    let rs = exec_latest(&w, &both).unwrap();
    assert_eq!(rs.columns, vec![v("x"), v("y")]);
    assert_eq!(rs.rows.len(), 2);
    let only_x = plan(
        &["x"],
        project(
            expand(
                tv("x"),
                "trusts",
                tv("y"),
                TickSource::Default,
                Access::Scan,
            ),
            &["x"],
        ),
    );
    let rs = exec_latest(&w, &only_x).unwrap();
    assert_eq!(rs.columns, vec![v("x")]);
    assert_eq!(rs.rows, vec![vec![Value::Ref(a)]]);
    // column order follows find, not binding order
    let swapped = plan(
        &["y", "x"],
        project(
            expand(
                tv("x"),
                "trusts",
                tv("y"),
                TickSource::Default,
                Access::Scan,
            ),
            &["y", "x"],
        ),
    );
    let rs = exec_latest(&w, &swapped).unwrap();
    assert_eq!(rs.columns, vec![v("y"), v("x")]);
    assert!(rs.rows.iter().all(|r| r[1] == Value::Ref(a)));
}

// ---------------------------------------------------------------- Filter, Join, NodeScan/Seek

#[test]
fn filter_every_cmp_op_on_i64() {
    let (w, a, bb, c) = small();
    let by_age = |op: CmpOp, rhs: Term| -> Vec<EntityId> {
        let p = one_col(
            "e",
            filter(
                expand(tv("e"), "age", tv("n"), TickSource::Default, Access::Scan),
                op,
                tv("n"),
                rhs,
            ),
        );
        let mut es: Vec<EntityId> = col0(&exec_latest(&w, &p).unwrap())
            .into_iter()
            .map(|v| match v {
                Value::Ref(e) => e,
                o => panic!("not a ref: {o:?}"),
            })
            .collect();
        es.sort();
        es
    };
    let mut abc = vec![a, bb, c];
    abc.sort();
    assert_eq!(by_age(CmpOp::Eq, ti(20)), vec![bb]);
    assert_eq!(by_age(CmpOp::Ne, ti(20)), {
        let mut x = vec![a, c];
        x.sort();
        x
    });
    assert_eq!(by_age(CmpOp::Lt, ti(20)), vec![a]);
    assert_eq!(by_age(CmpOp::Le, ti(20)), {
        let mut x = vec![a, bb];
        x.sort();
        x
    });
    assert_eq!(by_age(CmpOp::Gt, ti(20)), vec![c]);
    assert_eq!(by_age(CmpOp::Ge, ti(20)), {
        let mut x = vec![bb, c];
        x.sort();
        x
    });
    assert_eq!(by_age(CmpOp::Ge, ti(10)), abc);
    assert!(by_age(CmpOp::Gt, ti(30)).is_empty());
    assert!(by_age(CmpOp::Lt, ti(i64::MIN)).is_empty());
}

#[test]
fn filter_with_param_var_and_strings() {
    let (w, a, bb, c) = small();
    // param operand
    let mut p = one_col(
        "e",
        filter(
            expand(tv("e"), "age", tv("n"), TickSource::Default, Access::Scan),
            CmpOp::Gt,
            tv("n"),
            tp("min"),
        ),
    );
    p.params = vec!["min".into()];
    let mut params = Params::new();
    params.insert("min".into(), Value::I64(10));
    let rs = execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()).unwrap();
    assert_eq!(col0(&rs), ref_rows(vec![bb, c]));
    // var vs var: pairs (older, younger) with n < m
    let p = plan(
        &["e", "f"],
        project(
            filter(
                join(
                    expand(tv("e"), "age", tv("n"), TickSource::Default, Access::Scan),
                    expand(tv("f"), "age", tv("m"), TickSource::Default, Access::Scan),
                ),
                CmpOp::Lt,
                tv("n"),
                tv("m"),
            ),
            &["e", "f"],
        ),
    );
    let expected = sorted_rows(vec![
        vec![Value::Ref(a), Value::Ref(bb)],
        vec![Value::Ref(a), Value::Ref(c)],
        vec![Value::Ref(bb), Value::Ref(c)],
    ]);
    assert_eq!(ok_rows(&w, &p), expected);
    // strings compare bytewise
    let p = one_col(
        "e",
        filter(
            expand(tv("e"), "name", tv("s"), TickSource::Default, Access::Scan),
            CmpOp::Lt,
            tv("s"),
            ts("b"),
        ),
    );
    assert_eq!(col0(&exec_latest(&w, &p).unwrap()), ref_rows(vec![a]));
    // constant predicate on an empty-var plan: true keeps the row, false drops it
    let t = plan(&[], project(filter(Op::Unit, CmpOp::Lt, ti(1), ti(2)), &[]));
    assert_eq!(ok_rows(&w, &t), vec![Vec::<Value>::new()]);
    let f = plan(&[], project(filter(Op::Unit, CmpOp::Gt, ti(1), ti(2)), &[]));
    assert!(ok_rows(&w, &f).is_empty());
}

#[test]
fn join_on_shared_vars_and_cartesian() {
    let (w, a, bb, c) = small();
    let p = plan(
        &["e", "n", "s"],
        project(
            join(
                expand(tv("e"), "age", tv("n"), TickSource::Default, Access::Scan),
                expand(tv("e"), "name", tv("s"), TickSource::Default, Access::Scan),
            ),
            &["e", "n", "s"],
        ),
    );
    let expected = sorted_rows(vec![
        vec![Value::Ref(a), Value::I64(10), Value::Str("a".into())],
        vec![Value::Ref(bb), Value::I64(20), Value::Str("b".into())],
        vec![Value::Ref(c), Value::I64(30), Value::Str("c".into())],
    ]);
    assert_eq!(ok_rows(&w, &p), expected);
    // Cartesian: 3 characters x 0 places = 0; 3 x 3 = 9
    let p = plan(
        &["x", "y"],
        project(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            &["x", "y"],
        ),
    );
    assert_eq!(ok_rows(&w, &p).len(), 9);
    let p = plan(
        &["x", "y"],
        project(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "PLACE"),
            ),
            &["x", "y"],
        ),
    );
    assert!(ok_rows(&w, &p).is_empty());
    // Join with Unit is the identity
    let p = one_col("x", join(Op::Unit, node_scan(Op::Unit, "x", "CHARACTER")));
    assert_eq!(
        col0(&exec_latest(&w, &p).unwrap()),
        ref_rows(vec![a, bb, c])
    );
}

#[test]
fn node_scan_equals_entities_of_type_at_every_canon() {
    let w = varrow();
    for ty in ["CHARACTER", "PLACE", "ITEM", "EVENT"] {
        for tx in 0..=w.latest_tx().0 {
            let view = w.at_canon(TxId(tx));
            let expected: Vec<EntityId> = view.entities_of_type(ty).unwrap().collect();
            let p = one_col("e", node_scan(Op::Unit, "e", ty));
            let rs = execute(&view, &p, &no_params(), &Limits::default()).unwrap();
            assert_eq!(col0(&rs), ref_rows(expected), "{ty} at canon {tx}");
        }
    }
    // unknown type: an error, never a panic
    let p = one_col("e", node_scan(Op::Unit, "e", "NOPE"));
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::Core(_) | QueryError::Plan(_))
    ));
}

#[test]
fn node_seek_const_param_var_and_undefined_at_canon() {
    let mut w = World::in_memory(Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "name",
        ValueType::Str,
        Kind::State,
        Cardinality::One,
    )))
    .unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap(); // tx 1
    let b2 = w.create_entity("CHARACTER", Some("b")).unwrap(); // tx 2
    let seek = |e: Term| one_col("x", node_seek(Op::Unit, "x", e));
    let at1 = w.at_canon(TxId(1));
    let run1 = |p: &Plan| {
        execute(&at1, p, &no_params(), &Limits::default())
            .unwrap()
            .rows
    };
    assert_eq!(run1(&seek(tr(a))), vec![vec![Value::Ref(a)]]);
    assert!(
        run1(&seek(tr(b2))).is_empty(),
        "b is defined at tx 2, not at canon 1"
    );
    assert_eq!(ok_rows(&w, &seek(tr(b2))), vec![vec![Value::Ref(b2)]]);
    // never defined at all
    let ghost = EntityId::new(a.node_type(), 999).unwrap();
    assert!(ok_rows(&w, &seek(tr(ghost))).is_empty());
    // param
    let mut p = seek(tp("who"));
    p.params = vec!["who".into()];
    let mut params = Params::new();
    params.insert("who".into(), Value::Ref(a));
    let rs = execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()).unwrap();
    assert_eq!(rs.rows, vec![vec![Value::Ref(a)]]);
    // bound var: NodeScan ?y then NodeSeek ?x = ?y gives x == y
    let p = plan(
        &["x", "y"],
        project(
            node_seek(node_scan(Op::Unit, "y", "CHARACTER"), "x", tv("y")),
            &["x", "y"],
        ),
    );
    assert_eq!(
        ok_rows(&w, &p),
        vec![
            vec![Value::Ref(a), Value::Ref(a)],
            vec![Value::Ref(b2), Value::Ref(b2)]
        ]
    );
}

// ---------------------------------------------------------------- handles

#[test]
fn handle_access_both_directions() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    // handle -> entity
    let p = one_col("e", handle_expand(tv("e"), ts("edwin"), Access::Handle));
    assert_eq!(ok_rows(&w, &p), vec![vec![Value::Ref(edwin)]]);
    // entity -> handle string
    let p = one_col("h", handle_expand(tr(edwin), tv("h"), Access::Handle));
    assert_eq!(ok_rows(&w, &p), vec![vec![Value::Str("edwin".into())]]);
    // both bound: a check
    let p = plan(
        &[],
        project(handle_expand(tr(edwin), ts("edwin"), Access::Handle), &[]),
    );
    assert_eq!(ok_rows(&w, &p).len(), 1);
    let p = plan(
        &[],
        project(handle_expand(tr(edwin), ts("greg"), Access::Handle), &[]),
    );
    assert!(ok_rows(&w, &p).is_empty());
    // unknown handle: 0 rows, not an error
    let p = one_col("e", handle_expand(tv("e"), ts("nobody"), Access::Handle));
    assert!(ok_rows(&w, &p).is_empty());
    // entity without a handle: 0 rows
    let (mut w2, ..) = small();
    let anon = w2.create_entity("CHARACTER", None).unwrap();
    let p = one_col("h", handle_expand(tr(anon), tv("h"), Access::Handle));
    assert!(ok_rows(&w2, &p).is_empty());
    // the handle table respects the canon: no handle for an entity not yet defined
    let (w3, ..) = small();
    let view0 = w3.at_canon(TxId(0));
    let a = eid(&w3, "a");
    let p = one_col("h", handle_expand(tr(a), tv("h"), Access::Handle));
    assert!(
        execute(&view0, &p, &no_params(), &Limits::default())
            .unwrap()
            .rows
            .is_empty()
    );
    let p = one_col("e", handle_expand(tv("e"), ts("a"), Access::Handle));
    assert!(
        execute(&view0, &p, &no_params(), &Limits::default())
            .unwrap()
            .rows
            .is_empty()
    );
}

#[test]
fn handle_access_mismatches_are_plan_errors() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    // Handle access on a named attribute
    let p = one_col(
        "x",
        expand(
            tr(edwin),
            "ally_of",
            tv("x"),
            TickSource::Default,
            Access::Handle,
        ),
    );
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    // :handle read through an index access
    for access in [Access::E, Access::R, Access::V, Access::Scan] {
        let p = one_col("h", handle_expand(tr(edwin), tv("h"), access));
        assert!(
            matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))),
            "{access:?}"
        );
    }
}

// ---------------------------------------------------------------- pattern shapes

#[test]
fn same_var_twice_in_one_pattern_only_self_pairs() {
    let (w, a, _b, _c) = small();
    let scan = one_col(
        "x",
        expand(
            tv("x"),
            "trusts",
            tv("x"),
            TickSource::Default,
            Access::Scan,
        ),
    );
    assert_eq!(ok_rows(&w, &scan), vec![vec![Value::Ref(a)]]);
    // E access with ?x bound first: same answer
    let e = one_col(
        "x",
        expand_on(
            node_scan(Op::Unit, "x", "CHARACTER"),
            tv("x"),
            "trusts",
            tv("x"),
            TickSource::Default,
            Access::E,
        ),
    );
    assert_eq!(ok_rows(&w, &e), vec![vec![Value::Ref(a)]]);
    // a bound var in value position is a constraint, not rebound
    let both = plan(
        &["x", "y"],
        project(
            expand_on(
                node_scan(Op::Unit, "y", "CHARACTER"),
                tv("x"),
                "trusts",
                tv("y"),
                TickSource::Default,
                Access::Scan,
            ),
            &["x", "y"],
        ),
    );
    assert_eq!(ok_rows(&w, &both).len(), 2);
}

#[test]
fn any_matches_anything_and_binds_nothing() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    // Edwin has allies at -210: one empty row; none at -400: no rows
    let has = |t: i64| {
        plan(
            &[],
            project(
                expand(tr(edwin), "ally_of", Term::Any, at(t), Access::E),
                &[],
            ),
        )
    };
    assert_eq!(ok_rows(&w, &has(-210)), vec![Vec::<Value>::new()]);
    assert!(ok_rows(&w, &has(-400)).is_empty());
    // [?p located_in _] = everyone with a location at t, no duplicates
    let t = -300;
    let p = one_col(
        "p",
        expand(tv("p"), "located_in", Term::Any, at(t), Access::Scan),
    );
    let view = w.at_canon(w.latest_tx());
    let expected: Vec<EntityId> = view
        .entities_of_type("CHARACTER")
        .unwrap()
        .filter(|&c| !view.as_of(c, "located_in", Tick(t)).unwrap().is_empty())
        .collect();
    assert_eq!(col0(&exec_latest(&w, &p).unwrap()), ref_rows(expected));
    // two Any positions: [_ located_in _]
    let p = plan(
        &[],
        project(
            expand(Term::Any, "located_in", Term::Any, at(t), Access::Scan),
            &[],
        ),
    );
    assert_eq!(ok_rows(&w, &p), vec![Vec::<Value>::new()]);
}

#[test]
fn value_index_and_inverse_names() {
    let (w, a, bb, _c) = small();
    // [?e :name "b"] via V, Scan equal; with the entity bound via E
    let by = |access: Access| -> Vec<Vec<Value>> {
        ok_rows(
            &w,
            &one_col(
                "e",
                expand(tv("e"), "name", ts("b"), TickSource::Default, access),
            ),
        )
    };
    assert_eq!(by(Access::V), vec![vec![Value::Ref(bb)]]);
    assert_eq!(by(Access::Scan), by(Access::V));
    let e_bound = plan(
        &["n"],
        project(
            expand(tr(bb), "name", tv("n"), TickSource::Default, Access::E),
            &["n"],
        ),
    );
    assert_eq!(ok_rows(&w, &e_bound), vec![vec![Value::Str("b".into())]]);
    // i64 through V
    let p = one_col(
        "e",
        expand(tv("e"), "age", ti(30), TickSource::Default, Access::V),
    );
    assert_eq!(ok_rows(&w, &p).len(), 1);
    // inverse name: pairs come back (child, parent)
    let parents_of_b = one_col(
        "p",
        expand(tr(bb), "child_of", tv("p"), TickSource::Default, Access::E),
    );
    assert_eq!(ok_rows(&w, &parents_of_b), vec![vec![Value::Ref(a)]]);
    let kids_via_r = one_col(
        "k",
        expand(tv("k"), "child_of", tr(a), TickSource::Default, Access::R),
    );
    assert_eq!(ok_rows(&w, &kids_via_r), vec![vec![Value::Ref(bb)]]);
    let kids_scan = one_col(
        "k",
        expand(
            tv("k"),
            "child_of",
            tr(a),
            TickSource::Default,
            Access::Scan,
        ),
    );
    assert_eq!(ok_rows(&w, &kids_scan), vec![vec![Value::Ref(bb)]]);
    // bidirectional: either orientation matches, from either side
    for access in [Access::E, Access::Scan] {
        let p = one_col(
            "x",
            expand(tr(bb), "ally_of", tv("x"), TickSource::Default, access),
        );
        assert_eq!(ok_rows(&w, &p), vec![vec![Value::Ref(a)]], "{access:?}");
    }
    let p = one_col(
        "x",
        expand(tv("x"), "ally_of", tr(a), TickSource::Default, Access::R),
    );
    let mut want = ref_rows(vec![bb, eid(&w, "c")]);
    want.sort();
    assert_eq!(col0(&exec_latest(&w, &p).unwrap()), want);
}

#[test]
fn unindexed_attribute_values_via_scan_and_e() {
    let (mut w, a, ..) = small();
    let long = "x".repeat(1000);
    w.assert(a, "bio", Value::Str(long.clone()), Some(Tick(0)))
        .unwrap();
    for access in [Access::E, Access::Scan] {
        let p = one_col(
            "s",
            expand(tr(a), "bio", tv("s"), TickSource::Default, access),
        );
        assert_eq!(
            ok_rows(&w, &p),
            vec![vec![Value::Str(long.clone())]],
            "{access:?}"
        );
    }
    let p = one_col(
        "e",
        expand(tv("e"), "bio", ts("x"), TickSource::Default, Access::V),
    );
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::Plan(_) | QueryError::Core(_))
    ));
}

// ---------------------------------------------------------------- errors

#[test]
fn missing_param_and_param_type() {
    let w = varrow();
    let mut p = one_col("a", expand(tp("who"), "ally_of", tv("a"), at(0), Access::E));
    p.params = vec!["who".into()];
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::MissingParam(n)) if n == "who"
    ));
    let mut p = one_col(
        "a",
        expand(
            tr(eid(&w, "edwin")),
            "ally_of",
            tv("a"),
            TickSource::Param("t".into()),
            Access::E,
        ),
    );
    p.params = vec!["t".into()];
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::MissingParam(n)) if n == "t"
    ));
    let mut params = Params::new();
    params.insert("t".into(), Value::Str("soon".into()));
    assert!(matches!(
        execute(&w.at_canon(w.latest_tx()), &p, &params, &Limits::default()),
        Err(QueryError::ParamType { name, .. }) if name == "t"
    ));
    // default tick as a param, same rules
    let mut p = one_col(
        "a",
        expand(
            tr(eid(&w, "edwin")),
            "ally_of",
            tv("a"),
            TickSource::Default,
            Access::E,
        ),
    );
    p.params = vec!["t".into()];
    p.default_tick = TickSource::Param("t".into());
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::MissingParam(_))
    ));
}

#[test]
fn infeasible_accesses_are_plan_errors_never_panics() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    // E needs the entity bound
    let p = one_col("a", expand(tv("e"), "ally_of", tv("a"), at(0), Access::E));
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    let p = one_col("a", expand(Term::Any, "ally_of", tv("a"), at(0), Access::E));
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    // R needs a bound ref value
    let p = one_col("a", expand(tv("a"), "ally_of", tv("b"), at(0), Access::R));
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    let p = one_col("a", expand(tv("a"), "ally_of", Term::Any, at(0), Access::R));
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    // V needs a bound value
    let p = one_col("a", expand(tv("a"), "name", tv("n"), at(0), Access::V));
    assert!(matches!(exec_latest(&w, &p), Err(QueryError::Plan(_))));
    let _ = edwin;
}

#[test]
fn unknown_attribute_is_an_error() {
    let w = varrow();
    let p = one_col(
        "a",
        expand(
            tr(eid(&w, "edwin")),
            "no_such_attr",
            tv("a"),
            at(0),
            Access::E,
        ),
    );
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::Core(kala_core::Error::UnknownAttribute(_)) | QueryError::Plan(_))
    ));
    let p = one_col(
        "a",
        expand(tv("e"), "no_such_attr", tv("a"), at(0), Access::Scan),
    );
    assert!(matches!(
        exec_latest(&w, &p),
        Err(QueryError::Core(kala_core::Error::UnknownAttribute(_)) | QueryError::Plan(_))
    ));
}

#[test]
fn query_error_is_display_and_std_error() {
    fn takes<E: std::error::Error + 'static>(_: &E) {}
    let e = QueryError::MissingParam("x".into());
    takes(&e);
    assert!(!e.to_string().is_empty());
    let all = [
        QueryError::ParamType {
            name: "t".into(),
            expected: "i64".into(),
        },
        QueryError::TickNotBound("t".into()),
        QueryError::TypeMismatch("m".into()),
        QueryError::Plan("p".into()),
        QueryError::RowLimit { limit: 3 },
        QueryError::StepBudget { limit: 9 },
        QueryError::Core(kala_core::Error::UnknownAttribute("a".into())),
    ];
    for e in &all {
        assert!(!e.to_string().is_empty());
        assert!(!format!("{e:?}").is_empty());
    }
}

// ---------------------------------------------------------------- tick and canon reporting

#[test]
fn result_tick_is_the_resolved_default_tick() {
    let w = varrow();
    let edwin = eid(&w, "edwin");
    let mk = |d: TickSource| {
        let mut p = one_col(
            "a",
            expand(
                tr(edwin),
                "ally_of",
                tv("a"),
                TickSource::Default,
                Access::E,
            ),
        );
        p.default_tick = d;
        p.params = vec!["t".into()];
        p
    };
    let mut params = Params::new();
    params.insert("t".into(), Value::I64(-77));
    let view = w.at_canon(w.latest_tx());
    let rs = execute(&view, &mk(TickSource::Default), &params, &Limits::default()).unwrap();
    assert_eq!(rs.tick, w.latest_tick());
    assert_eq!(rs.canon, w.latest_tx());
    let rs = execute(&view, &mk(at(-5)), &params, &Limits::default()).unwrap();
    assert_eq!(rs.tick, Tick(-5));
    let rs = execute(
        &view,
        &mk(TickSource::Param("t".into())),
        &params,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(rs.tick, Tick(-77));
    // the Default tick really is what operators read at, at the view's canon
    let old = w.at_canon(TxId(2));
    let rs = execute(&old, &mk(TickSource::Default), &params, &Limits::default()).unwrap();
    assert_eq!(rs.tick, old.latest_tick());
    assert_eq!(rs.canon, TxId(2));
    let expected = ref_rows(
        old.neighbors(edwin, "ally_of", Direction::Both, old.latest_tick())
            .unwrap_or_default()
            .into_iter()
            .map(|(e, _)| e)
            .collect(),
    );
    assert_eq!(col0(&rs), expected);
    // empty world: latest tick 0
    let empty = World::in_memory(Schema::new().node_type("CHARACTER")).unwrap();
    let p = plan(
        &["e"],
        project(node_scan(Op::Unit, "e", "CHARACTER"), &["e"]),
    );
    let rs = exec_latest(&empty, &p).unwrap();
    assert_eq!(rs.tick, Tick(0));
    assert!(rs.rows.is_empty());
    assert_eq!(rs.columns, vec![v("e")]);
}

#[test]
fn deterministic_across_runs() {
    let w = varrow();
    let p = plan(
        &["a", "b"],
        project(
            join(
                expand(tv("a"), "ally_of", tv("b"), at(-200), Access::Scan),
                node_scan(Op::Unit, "a", "CHARACTER"),
            ),
            &["a", "b"],
        ),
    );
    let r1 = exec_latest(&w, &p).unwrap();
    let r2 = exec_latest(&w, &p).unwrap();
    assert_eq!(r1.rows, r2.rows);
    assert!(!r1.rows.is_empty());
}

// ---------------------------------------------------------------- limits

#[test]
fn row_limit_and_step_budget_are_errors_not_truncation() {
    let w = varrow();
    let all_chars = one_col("x", node_scan(Op::Unit, "x", "CHARACTER"));
    let n = exec_latest(&w, &all_chars).unwrap().rows.len();
    assert!(n > 10);
    let view = w.at_canon(w.latest_tx());
    let lim = Limits {
        max_rows: 5,
        ..Limits::default()
    };
    assert!(matches!(
        execute(&view, &all_chars, &no_params(), &lim),
        Err(QueryError::RowLimit { limit: 5 })
    ));
    // a Cartesian product bigger than the limit trips it even though the
    // projected result would be small
    let cart = plan(
        &[],
        project(
            join(
                node_scan(Op::Unit, "x", "CHARACTER"),
                node_scan(Op::Unit, "y", "CHARACTER"),
            ),
            &[],
        ),
    );
    let lim = Limits {
        max_rows: n,
        ..Limits::default()
    };
    assert!(matches!(
        execute(&view, &cart, &no_params(), &lim),
        Err(QueryError::RowLimit { .. })
    ));
    let tiny = Limits {
        max_steps: 3,
        ..Limits::default()
    };
    assert!(matches!(
        execute(&view, &all_chars, &no_params(), &tiny),
        Err(QueryError::StepBudget { limit: 3 })
    ));
    // defaults from the contract
    let d = Limits::default();
    assert_eq!((d.max_rows, d.max_steps), (100_000, 10_000_000));
    // a generous limit gives the full answer
    let big = Limits {
        max_rows: 1_000_000,
        max_steps: 100_000_000,
        ..Limits::default()
    };
    assert_eq!(
        execute(&view, &all_chars, &no_params(), &big)
            .unwrap()
            .rows
            .len(),
        n
    );
}

#[test]
fn execute_unused_imports_compile_guard() {
    // keeps AttrRef / BTreeMap imports meaningful if helpers change
    let _: BTreeMap<String, Value> = Params::new();
    let _ = AttrRef::Handle;
}
