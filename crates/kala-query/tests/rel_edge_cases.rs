//! Slice 7, item 2 (confirm): hand-built IR edge cases for `Fixpoint`/`Rel`
//! (non-recursive): `Rel` inside a `Union` input and on the right of a
//! `Join`, the same variable twice in one `Rel`, limits hit *inside* a
//! relation while the final result is small, and malformed `RelDef`s.
//!
//! World: n0..n4 (CHARACTER); parent_of: n0->n1 @0, n1->n2 @0 (retracted @5),
//! n2->n3 @7, n4->n4 @0 (a self-loop). So `par` read @0 is
//! {(0,1),(1,2),(4,4)} and read @10 is {(0,1),(2,3),(4,4)}.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Limits, Op, Params, QueryError, RelDef, Term, run};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(
            AttrDef::new("parent_of", ValueType::Ref, Kind::State, Cardinality::Many).directed(),
        )
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
}

fn world() -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let n: Vec<EntityId> = (0..5)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let t = |x: i64| Some(Tick(x));
    let r = |i: usize| Value::Ref(n[i]);
    w.assert(n[0], "parent_of", r(1), t(0)).unwrap();
    w.assert(n[1], "parent_of", r(2), t(0)).unwrap();
    w.retract(n[1], "parent_of", r(2), t(5)).unwrap();
    w.assert(n[2], "parent_of", r(3), t(7)).unwrap();
    w.assert(n[4], "parent_of", r(4), t(0)).unwrap();
    (w, n)
}

/// `par(x, y)`: every parent_of pair at `tick`.
fn par(tick: i64) -> RelDef {
    RelDef {
        name: "par".into(),
        vars: vec![v("x"), v("y")],
        rules: vec![project(
            expand(tv("x"), "parent_of", tv("y"), at(tick), Access::Scan),
            &["x", "y"],
        )],
    }
}

fn rel(input: Op, name: &str, args: Vec<Term>) -> Op {
    Op::Rel {
        input: b(input),
        name: name.into(),
        args,
    }
}

fn fix(rels: Vec<RelDef>, body: Op) -> Op {
    Op::Fixpoint {
        rels,
        body: b(body),
    }
}

fn exec(w: &World, find: &[&str], root: Op, lim: &Limits) -> Result<Vec<Vec<Value>>, QueryError> {
    let p = plan(find, project(root, find));
    run(w, &p, &Params::new(), lim).map(|rs| value_rows(&rs))
}

fn pairs(n: &[EntityId], ps: &[(usize, usize)]) -> Vec<Vec<Value>> {
    sorted_rows(
        ps.iter()
            .map(|&(a, b)| vec![Value::Ref(n[a]), Value::Ref(n[b])])
            .collect(),
    )
}

#[test]
fn baseline_rel_over_unit_returns_the_relation() {
    let (w, n) = world();
    let got = exec(
        &w,
        &["a", "b"],
        fix(vec![par(0)], rel(Op::Unit, "par", vec![tv("a"), tv("b")])),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(got, pairs(&n, &[(0, 1), (1, 2), (4, 4)]));
}

#[test]
fn rel_inside_a_union_input() {
    let (w, n) = world();
    // input 1: par @0; input 2: a direct scan @10
    let body = Op::Union {
        inputs: vec![
            rel(Op::Unit, "par", vec![tv("a"), tv("b")]),
            expand(tv("a"), "parent_of", tv("b"), at(10), Access::Scan),
        ],
    };
    let got = exec(&w, &["a", "b"], fix(vec![par(0)], body), &Limits::default()).unwrap();
    assert_eq!(got, pairs(&n, &[(0, 1), (1, 2), (2, 3), (4, 4)]));
}

#[test]
fn rel_in_both_union_inputs() {
    let (w, n) = world();
    let body = Op::Union {
        inputs: vec![
            rel(Op::Unit, "par", vec![tv("a"), tv("b")]),
            rel(Op::Unit, "par", vec![tv("a"), tv("b")]),
        ],
    };
    let got = exec(&w, &["a", "b"], fix(vec![par(0)], body), &Limits::default()).unwrap();
    assert_eq!(got, pairs(&n, &[(0, 1), (1, 2), (4, 4)]));
}

#[test]
fn rel_on_the_right_of_a_join() {
    let (w, n) = world();
    // left @10: (0,1) (2,3) (4,4); right par@0 on (b, c): (0,1) (1,2) (4,4)
    let body = join(
        expand(tv("a"), "parent_of", tv("b"), at(10), Access::Scan),
        rel(Op::Unit, "par", vec![tv("b"), tv("c")]),
    );
    let got = exec(
        &w,
        &["a", "b", "c"],
        fix(vec![par(0)], body),
        &Limits::default(),
    )
    .unwrap();
    let t =
        |a: usize, b: usize, c: usize| vec![Value::Ref(n[a]), Value::Ref(n[b]), Value::Ref(n[c])];
    assert_eq!(got, sorted_rows(vec![t(0, 1, 2), t(4, 4, 4)]));
}

#[test]
fn rel_on_the_left_of_a_join_gives_the_same_rows() {
    let (w, _) = world();
    let l = join(
        expand(tv("a"), "parent_of", tv("b"), at(10), Access::Scan),
        rel(Op::Unit, "par", vec![tv("b"), tv("c")]),
    );
    let r = join(
        rel(Op::Unit, "par", vec![tv("b"), tv("c")]),
        expand(tv("a"), "parent_of", tv("b"), at(10), Access::Scan),
    );
    let f = ["a", "b", "c"];
    assert_eq!(
        exec(&w, &f, fix(vec![par(0)], l), &Limits::default()).unwrap(),
        exec(&w, &f, fix(vec![par(0)], r), &Limits::default()).unwrap()
    );
}

#[test]
fn same_variable_twice_when_already_bound_is_an_equality_filter() {
    let (w, n) = world();
    // z bound by a node scan; par(z, z) keeps only the self-loop
    let body = rel(
        node_scan(Op::Unit, "z", "CHARACTER"),
        "par",
        vec![tv("z"), tv("z")],
    );
    let got = exec(&w, &["z"], fix(vec![par(0)], body), &Limits::default()).unwrap();
    assert_eq!(got, vec![vec![Value::Ref(n[4])]]);
}

#[test]
fn same_variable_twice_when_free_is_an_equality_filter() {
    let (w, n) = world();
    let body = rel(Op::Unit, "par", vec![tv("z"), tv("z")]);
    let got = exec(&w, &["z"], fix(vec![par(0)], body), &Limits::default()).unwrap();
    assert_eq!(got, vec![vec![Value::Ref(n[4])]]);
    // @10 has the same self-loop; reading n2->n3 never matches
    let got = exec(
        &w,
        &["z"],
        fix(vec![par(10)], rel(Op::Unit, "par", vec![tv("z"), tv("z")])),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(got, vec![vec![Value::Ref(n[4])]]);
}

#[test]
fn same_variable_twice_with_a_value_that_never_matches_is_empty() {
    let (w, n) = world();
    // bind z to n0 first: par(n0, n0) doesn't exist
    let body = rel(
        node_seek(Op::Unit, "z", tr(n[0])),
        "par",
        vec![tv("z"), tv("z")],
    );
    let got = exec(&w, &["z"], fix(vec![par(0)], body), &Limits::default()).unwrap();
    assert!(got.is_empty(), "{got:?}");
}

// ------------------------------------------------- limits inside a relation

/// 60 characters, each with a distinct ~1 KB bio.
fn big_world() -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let mut t = w.begin();
    let mut ids = vec![];
    for i in 0..60 {
        let e = t.create_entity("CHARACTER", None).unwrap();
        t.assert(
            e,
            "bio",
            Value::Str(format!("{i:03}{}", "x".repeat(1000))),
            Some(Tick(0)),
        )
        .unwrap();
        ids.push(e);
    }
    t.commit().unwrap();
    (w, ids)
}

#[test]
fn row_limit_inside_a_relation_even_when_the_final_result_is_small() {
    let (w, n) = big_world();
    // pair(x, y): 60 * 60 = 3,600 tuples; the body keeps exactly one.
    let pair = RelDef {
        name: "pair".into(),
        vars: vec![v("x"), v("y")],
        rules: vec![node_scan(
            node_scan(Op::Unit, "x", "CHARACTER"),
            "y",
            "CHARACTER",
        )],
    };
    let small = || {
        fix(
            vec![pair.clone()],
            rel(Op::Unit, "pair", vec![tr(n[0]), tr(n[1])]),
        )
    };
    let ok = exec(&w, &[], small(), &Limits::default()).unwrap();
    assert_eq!(ok.len(), 1);
    let lim = Limits {
        max_rows: 1_000,
        ..Limits::default()
    };
    match exec(&w, &[], small(), &lim) {
        Err(QueryError::RowLimit { limit: 1_000 }) => {}
        other => panic!("expected RowLimit inside the relation, got {other:?}"),
    }
}

#[test]
fn memory_budget_inside_a_relation_even_when_the_final_result_is_small() {
    let (w, n) = big_world();
    // blurb(x, s): 60 distinct 1 KB strings (~60 KB) held by the relation
    let blurb = RelDef {
        name: "blurb".into(),
        vars: vec![v("x"), v("s")],
        rules: vec![project(
            expand(tv("x"), "bio", tv("s"), at(0), Access::Scan),
            &["x", "s"],
        )],
    };
    let root = || {
        fix(
            vec![blurb.clone()],
            rel(Op::Unit, "blurb", vec![tr(n[0]), tv("s")]),
        )
    };
    let ok = exec(&w, &["s"], root(), &Limits::default()).unwrap();
    assert_eq!(ok.len(), 1); // a ~1 KB final result
    let lim = Limits {
        max_bytes: 30 * 1024,
        ..Limits::default()
    };
    match exec(&w, &["s"], root(), &lim) {
        Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 30 * 1024),
        other => panic!("expected MemoryBudget inside the relation, got {other:?}"),
    }
}

// ------------------------------------------------------ malformed RelDefs

fn plan_err(w: &World, root: Op) -> String {
    match exec(w, &["a"], root, &Limits::default()) {
        Err(QueryError::Plan(m)) => m,
        other => panic!("expected a Plan error, got {other:?}"),
    }
}

#[test]
fn duplicate_relation_names_are_a_plan_error() {
    let (w, _) = world();
    let one = RelDef {
        name: "r".into(),
        vars: vec![v("a")],
        rules: vec![node_scan(Op::Unit, "a", "CHARACTER")],
    };
    let m = plan_err(
        &w,
        fix(vec![one.clone(), one], rel(Op::Unit, "r", vec![tv("a")])),
    );
    assert!(m.contains("`r`") && m.contains("twice"), "{m}");
}

#[test]
fn duplicate_column_names_are_a_plan_error() {
    let (w, _) = world();
    let bad = RelDef {
        name: "r".into(),
        vars: vec![v("a"), v("a")],
        rules: vec![node_scan(Op::Unit, "a", "CHARACTER")],
    };
    let m = plan_err(
        &w,
        fix(vec![bad], rel(Op::Unit, "r", vec![tv("a"), tv("b")])),
    );
    assert!(m.contains("`r`") && m.contains("twice"), "{m}");
}

#[test]
fn same_names_in_nested_fixpoints_shadow_the_outer_one() {
    let (w, n) = world();
    // inner `par` (@10) shadows outer `par` (@0) inside the inner body
    let inner = fix(vec![par(10)], rel(Op::Unit, "par", vec![tv("a"), tv("b")]));
    let got = exec(
        &w,
        &["a", "b"],
        fix(vec![par(0)], inner),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(got, pairs(&n, &[(0, 1), (2, 3), (4, 4)]));
}
