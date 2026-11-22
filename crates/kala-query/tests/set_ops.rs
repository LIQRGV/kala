//! Slice 7, item 4 (RED): the executor runs `Not`, `Difference` and
//! `Intersect` (today it refuses them with `QueryError::Plan`).
//!
//! Contract:
//! - `Not { input, body }`: for each input row, run `body` starting from that
//!   row (its `Unit` leaf is the row); keep the row iff the body gives no rows.
//!   Variables only the body binds are local to it.
//! - `Difference { left, right }`: independent subplans; the left rows with no
//!   right row agreeing on every shared variable. With no shared variable: all
//!   left rows if `right` is empty, else none.
//! - `Intersect { left, right }`: the left rows with at least one agreeing
//!   right row; binds the left's variables only, each row once. With no shared
//!   variable: all left rows if `right` is non-empty, else none.
//! - Results are sets; the row, step and byte limits apply inside both sides.
//!
//! World: n0..n5 (CHARACTER). parent_of @0: A = {(0,1),(0,2),(1,3),(2,3),(3,4)};
//! at @5 the edge (0,2) is retracted and (4,5) added: B = {(0,1),(1,3),(2,3),
//! (3,4),(4,5)}. Nobody has a child at tick -1, and n5 has none at @0.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Limits, Op, Params, QueryError, run};
use proptest::prelude::*;

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "parent_of",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    ))
}

fn world() -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let n: Vec<EntityId> = (0..6)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let t = |x| Some(Tick(x));
    for (a, c) in [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)] {
        w.assert(n[a], "parent_of", Value::Ref(n[c]), t(0)).unwrap();
    }
    w.retract(n[0], "parent_of", Value::Ref(n[2]), t(5))
        .unwrap();
    w.assert(n[4], "parent_of", Value::Ref(n[5]), t(5)).unwrap();
    (w, n)
}

/// A long chain: `size` entities, each the parent of the next, @0.
fn big_world(size: usize) -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let n: Vec<EntityId> = (0..size)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    for p in n.windows(2) {
        w.assert(p[0], "parent_of", Value::Ref(p[1]), Some(Tick(0)))
            .unwrap();
    }
    (w, n)
}

/// `parent_of` pairs at `tick` bound to the variables `x` and `y`.
fn scan(tick: i64, x: &str, y: &str) -> Op {
    expand(tv(x), "parent_of", tv(y), at(tick), Access::Scan)
}

fn diff(l: Op, r: Op) -> Op {
    Op::Difference {
        left: b(l),
        right: b(r),
    }
}
fn inter(l: Op, r: Op) -> Op {
    Op::Intersect {
        left: b(l),
        right: b(r),
    }
}
fn not(input: Op, body: Op) -> Op {
    Op::Not {
        input: b(input),
        body: b(body),
    }
}

fn exec_with(
    w: &World,
    find: &[&str],
    root: Op,
    lim: &Limits,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let p = plan(find, project(root, find));
    run(w, &p, &Params::new(), lim).map(|rs| value_rows(&rs))
}

fn exec(w: &World, find: &[&str], root: Op) -> Vec<Vec<Value>> {
    exec_with(w, find, root, &Limits::default()).unwrap_or_else(|e| panic!("{e}"))
}

fn pairs(n: &[EntityId], ps: &[(usize, usize)]) -> Vec<Vec<Value>> {
    sorted_rows(
        ps.iter()
            .map(|&(a, c)| vec![Value::Ref(n[a]), Value::Ref(n[c])])
            .collect(),
    )
}

fn singles(n: &[EntityId], xs: &[usize]) -> Vec<Vec<Value>> {
    sorted_rows(xs.iter().map(|&a| vec![Value::Ref(n[a])]).collect())
}

const A_ALL: [(usize, usize); 5] = [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)];

// ------------------------------------------------------------------- Not

#[test]
fn not_keeps_the_rows_the_body_cannot_satisfy() {
    let (w, n) = world();
    // body: does (x, parent_of, y) still hold @5? Only (0,2) was retracted.
    let body = expand_on(Op::Unit, tv("x"), "parent_of", tv("y"), at(5), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], not(scan(0, "x", "y"), body)),
        pairs(&n, &[(0, 2)])
    );
}

#[test]
fn not_with_a_local_variable_in_the_body() {
    let (w, n) = world();
    // body: does y have any child @0 (z is local)? only n4 has none.
    let body = expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(0), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], not(scan(0, "x", "y"), body)),
        pairs(&n, &[(3, 4)])
    );
    // z is local: it is not an output of the Not
    let p = plan(
        &["z"],
        project(
            not(
                scan(0, "x", "y"),
                expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(0), Access::E),
            ),
            &["z"],
        ),
    );
    assert!(matches!(
        run(&w, &p, &Params::new(), &Limits::default()),
        Err(QueryError::Plan(_))
    ));
}

#[test]
fn a_nested_not_is_double_negation() {
    let (w, n) = world();
    let has_child = expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(0), Access::E);
    let inner = not(Op::Unit, has_child);
    assert_eq!(
        exec(&w, &["x", "y"], not(scan(0, "x", "y"), inner)),
        pairs(&n, &[(0, 1), (0, 2), (1, 3), (2, 3)])
    );
}

#[test]
fn a_body_independent_of_the_row_is_a_global_condition() {
    let (w, n) = world();
    // n0 -> n1 holds @0: the body has rows, so every row is dropped
    let yes = expand_on(Op::Unit, tr(n[0]), "parent_of", tr(n[1]), at(0), Access::E);
    assert!(exec(&w, &["x", "y"], not(scan(0, "x", "y"), yes)).is_empty());
    // n5 has no child @0: the body is empty, every row stays
    let no = expand_on(Op::Unit, tr(n[5]), "parent_of", tv("c"), at(0), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], not(scan(0, "x", "y"), no)),
        pairs(&n, &A_ALL)
    );
}

#[test]
fn not_over_an_empty_input_is_empty() {
    let (w, n) = world();
    let input = expand_on(Op::Unit, tr(n[5]), "parent_of", tv("y"), at(0), Access::E);
    let body = expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(0), Access::E);
    assert!(exec(&w, &["y"], not(input, body)).is_empty());
}

#[test]
fn not_reads_each_body_pattern_at_its_own_tick() {
    let (w, n) = world();
    // rows A read @0; the body reads @-1 where nothing holds: nothing dropped
    let body = expand_on(Op::Unit, tv("x"), "parent_of", tv("y"), at(-1), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], not(scan(0, "x", "y"), body)),
        pairs(&n, &A_ALL)
    );
}

// ------------------------------------------------------------ Difference

#[test]
fn difference_on_all_shared_variables() {
    let (w, n) = world();
    assert_eq!(
        exec(&w, &["x", "y"], diff(scan(0, "x", "y"), scan(5, "x", "y"))),
        pairs(&n, &[(0, 2)])
    );
    // the other way: B - A = {(4,5)}
    assert_eq!(
        exec(&w, &["x", "y"], diff(scan(5, "x", "y"), scan(0, "x", "y"))),
        pairs(&n, &[(4, 5)])
    );
    // a set minus itself, and minus an empty set
    assert!(exec(&w, &["x", "y"], diff(scan(0, "x", "y"), scan(0, "x", "y"))).is_empty());
    assert_eq!(
        exec(&w, &["x", "y"], diff(scan(0, "x", "y"), scan(-1, "x", "y"))),
        pairs(&n, &A_ALL)
    );
}

#[test]
fn difference_on_some_shared_variables() {
    let (w, n) = world();
    // right binds only x = n3: drops every left row with x = n3
    let right = node_seek(Op::Unit, "x", tr(n[3]));
    assert_eq!(
        exec(&w, &["x", "y"], diff(scan(0, "x", "y"), right)),
        pairs(&n, &[(0, 1), (0, 2), (1, 3), (2, 3)])
    );
    // right (x, z): shares only x. Everyone with a child @5 is x in 0..=4: all gone
    assert!(exec(&w, &["x", "y"], diff(scan(0, "x", "y"), scan(5, "x", "z"))).is_empty());
    // the left binds fewer variables than the right
    let left = project(scan(0, "x", "y"), &["x"]);
    assert_eq!(
        exec(&w, &["x"], diff(left, node_seek(Op::Unit, "x", tr(n[0])))),
        singles(&n, &[1, 2, 3])
    );
}

#[test]
fn difference_with_no_shared_variable() {
    let (w, n) = world();
    // right = {w: n1, n2}: non-empty, shares nothing: every left row goes
    let right = expand_on(Op::Unit, tr(n[0]), "parent_of", tv("w"), at(0), Access::E);
    assert!(exec(&w, &["x", "y"], diff(scan(0, "x", "y"), right)).is_empty());
    // empty right: every left row stays
    let empty = expand_on(Op::Unit, tr(n[5]), "parent_of", tv("w"), at(0), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], diff(scan(0, "x", "y"), empty)),
        pairs(&n, &A_ALL)
    );
    // empty left stays empty either way
    let none = expand_on(Op::Unit, tr(n[5]), "parent_of", tv("y"), at(0), Access::E);
    let right = expand_on(Op::Unit, tr(n[0]), "parent_of", tv("w"), at(0), Access::E);
    assert!(exec(&w, &["y"], diff(none, right)).is_empty());
}

// ------------------------------------------------------------- Intersect

#[test]
fn intersect_on_all_shared_variables() {
    let (w, n) = world();
    assert_eq!(
        exec(&w, &["x", "y"], inter(scan(0, "x", "y"), scan(5, "x", "y"))),
        pairs(&n, &[(0, 1), (1, 3), (2, 3), (3, 4)])
    );
    assert_eq!(
        exec(&w, &["x", "y"], inter(scan(0, "x", "y"), scan(0, "x", "y"))),
        pairs(&n, &A_ALL)
    );
    assert!(
        exec(
            &w,
            &["x", "y"],
            inter(scan(0, "x", "y"), scan(-1, "x", "y"))
        )
        .is_empty()
    );
}

#[test]
fn intersect_on_some_shared_variables_binds_the_left_variables_only() {
    let (w, n) = world();
    let right = node_seek(Op::Unit, "x", tr(n[3]));
    assert_eq!(
        exec(&w, &["x", "y"], inter(scan(0, "x", "y"), right)),
        pairs(&n, &[(3, 4)])
    );
    // the right's own variable z is not available above the Intersect
    let p = plan(
        &["z"],
        project(inter(scan(0, "x", "y"), scan(5, "x", "z")), &["z"]),
    );
    assert!(matches!(
        run(&w, &p, &Params::new(), &Limits::default()),
        Err(QueryError::Plan(_))
    ));
    // but the left's variables are, with all of A's x having a child @5
    assert_eq!(
        exec(&w, &["x", "y"], inter(scan(0, "x", "y"), scan(5, "x", "z"))),
        pairs(&n, &A_ALL)
    );
}

#[test]
fn intersect_collapses_duplicates_from_several_matching_right_rows() {
    let (w, n) = world();
    // n0 has two right rows (n0,n1) and (n0,n2) @0, but its left row appears once
    let left = project(scan(0, "x", "y"), &["x"]);
    let got = exec(&w, &["x"], inter(left, scan(0, "x", "z")));
    assert_eq!(got, singles(&n, &[0, 1, 2, 3]));
    assert_eq!(got.len(), 4);
    // the full set-op result (no projection above) has no duplicate rows either
    let p = plan(&["x", "y"], inter(scan(0, "x", "y"), scan(0, "x", "z")));
    let rs = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    assert_eq!(value_rows(&rs), pairs(&n, &A_ALL));
}

#[test]
fn intersect_with_no_shared_variable() {
    let (w, n) = world();
    let right = expand_on(Op::Unit, tr(n[0]), "parent_of", tv("w"), at(0), Access::E);
    assert_eq!(
        exec(&w, &["x", "y"], inter(scan(0, "x", "y"), right)),
        pairs(&n, &A_ALL)
    );
    let empty = expand_on(Op::Unit, tr(n[5]), "parent_of", tv("w"), at(0), Access::E);
    assert!(exec(&w, &["x", "y"], inter(scan(0, "x", "y"), empty)).is_empty());
}

// ---------------------------------------------------------------- limits

/// One plan per operator and side: `big` is a scan of `parent_of` pairs,
/// `small` yields one row. Each plan has the big part on one side only.
fn limit_cases(first: EntityId) -> Vec<(&'static str, Op)> {
    let big = || scan(0, "x", "y");
    let small = || node_seek(Op::Unit, "x", tr(first));
    let free_body = || expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(0), Access::E);
    vec![
        ("difference, big right", diff(small(), big())),
        ("difference, big left", diff(big(), small())),
        ("intersect, big right", inter(small(), big())),
        ("intersect, big left", inter(big(), small())),
        // input: one row; body: an independent scan of every pair
        ("not, big body", not(small(), scan(0, "p", "q"))),
        ("not, big input", not(big(), free_body())),
    ]
}

#[test]
fn the_limit_cases_succeed_under_the_default_limits() {
    let (w, n) = world();
    for (name, op) in limit_cases(n[0]) {
        let find: &[&str] = &["x"];
        exec_with(&w, find, op, &Limits::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn a_row_limit_applies_inside_both_sides() {
    let (w, n) = world();
    // the scan has 5 rows
    let lim = Limits {
        max_rows: 4,
        ..Limits::default()
    };
    for (name, op) in limit_cases(n[0]) {
        match exec_with(&w, &["x"], op, &lim) {
            Err(QueryError::RowLimit { limit: 4 }) => {}
            other => panic!("{name}: expected RowLimit, got {other:?}"),
        }
    }
}

#[test]
fn a_step_budget_applies_inside_both_sides() {
    let (w, n) = world();
    let lim = Limits {
        max_steps: 3,
        ..Limits::default()
    };
    for (name, op) in limit_cases(n[0]) {
        match exec_with(&w, &["x"], op, &lim) {
            Err(QueryError::StepBudget { limit: 3 }) => {}
            other => panic!("{name}: expected StepBudget, got {other:?}"),
        }
    }
}

#[test]
fn a_byte_budget_applies_inside_both_sides() {
    let (w, n) = big_world(3_000);
    let lim = Limits {
        max_bytes: 16 * 1024,
        ..Limits::default()
    };
    for (name, op) in limit_cases(n[0]) {
        match exec_with(&w, &["x"], op, &lim) {
            Err(QueryError::MemoryBudget { limit }) => assert_eq!(limit, 16 * 1024, "{name}"),
            other => panic!("{name}: expected MemoryBudget, got {other:?}"),
        }
    }
    // the same plans fit the default budget
    for (name, op) in limit_cases(n[0]) {
        exec_with(&w, &["x"], op, &Limits::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

// ------------------------------------------------------ property: naive model

type Pair = (EntityId, Value);

fn naive_rows(set: &[(EntityId, EntityId)]) -> Vec<Vec<Value>> {
    sorted_rows(
        set.iter()
            .map(|&(a, c)| vec![Value::Ref(a), Value::Ref(c)])
            .collect(),
    )
}

fn scan_pairs(w: &World, t: i64) -> Vec<(EntityId, EntityId)> {
    let raw: Vec<Pair> = w.attr_scan("parent_of", Tick(t)).unwrap();
    raw.into_iter()
        .map(|(e, v)| match v {
            Value::Ref(r) => (e, r),
            other => panic!("not a ref: {other:?}"),
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn difference_and_intersect_match_a_naive_computation_over_attr_scan(
        facts in proptest::collection::vec((0usize..6, 0usize..6, 0i64..6, any::<bool>()), 0..30),
        t1 in -1i64..7,
        t2 in -1i64..7,
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let n: Vec<EntityId> = (0..6).map(|_| w.create_entity("CHARACTER", None).unwrap()).collect();
        for (a, c, t, is_assert) in facts {
            let v = Value::Ref(n[c]);
            // same-tick conflicts are rejected by the store: skip them
            let _ = if is_assert {
                w.assert(n[a], "parent_of", v, Some(Tick(t)))
            } else {
                w.retract(n[a], "parent_of", v, Some(Tick(t)))
            };
        }
        let l = scan_pairs(&w, t1);
        let r = scan_pairs(&w, t2);
        let rx: Vec<EntityId> = r.iter().map(|p| p.0).collect();
        let ry: Vec<EntityId> = r.iter().map(|p| p.1).collect();

        // (a) every variable shared
        let want_d: Vec<_> = l.iter().filter(|p| !r.contains(p)).copied().collect();
        let want_i: Vec<_> = l.iter().filter(|p| r.contains(p)).copied().collect();
        prop_assert_eq!(exec(&w, &["x", "y"], diff(scan(t1, "x", "y"), scan(t2, "x", "y"))), naive_rows(&want_d));
        prop_assert_eq!(exec(&w, &["x", "y"], inter(scan(t1, "x", "y"), scan(t2, "x", "y"))), naive_rows(&want_i));

        // (b) only x shared (right is (x, z))
        let want_d: Vec<_> = l.iter().filter(|p| !rx.contains(&p.0)).copied().collect();
        let want_i: Vec<_> = l.iter().filter(|p| rx.contains(&p.0)).copied().collect();
        prop_assert_eq!(exec(&w, &["x", "y"], diff(scan(t1, "x", "y"), scan(t2, "x", "z"))), naive_rows(&want_d));
        prop_assert_eq!(exec(&w, &["x", "y"], inter(scan(t1, "x", "y"), scan(t2, "x", "z"))), naive_rows(&want_i));

        // (c) only y shared (right is (z, y))
        let want_d: Vec<_> = l.iter().filter(|p| !ry.contains(&p.1)).copied().collect();
        let want_i: Vec<_> = l.iter().filter(|p| ry.contains(&p.1)).copied().collect();
        prop_assert_eq!(exec(&w, &["x", "y"], diff(scan(t1, "x", "y"), scan(t2, "z", "y"))), naive_rows(&want_d));
        prop_assert_eq!(exec(&w, &["x", "y"], inter(scan(t1, "x", "y"), scan(t2, "z", "y"))), naive_rows(&want_i));

        // (d) nothing shared (right is (p, q)): all or none of the left
        let (want_d, want_i) = if r.is_empty() { (l.clone(), vec![]) } else { (vec![], l.clone()) };
        prop_assert_eq!(exec(&w, &["x", "y"], diff(scan(t1, "x", "y"), scan(t2, "p", "q"))), naive_rows(&want_d));
        prop_assert_eq!(exec(&w, &["x", "y"], inter(scan(t1, "x", "y"), scan(t2, "p", "q"))), naive_rows(&want_i));

        // Not with the same condition as (a): "(x, y) does not hold at t2"
        let body = expand_on(Op::Unit, tv("x"), "parent_of", tv("y"), at(t2), Access::E);
        let want_n: Vec<_> = l.iter().filter(|p| !r.contains(p)).copied().collect();
        prop_assert_eq!(exec(&w, &["x", "y"], not(scan(t1, "x", "y"), body)), naive_rows(&want_n));
    }
}

// -------------------------------------------- Difference inside a Not body

#[test]
fn a_difference_inside_a_not_body() {
    let (w, n) = world();
    // For each row (x, y) of A@0 the body is "y's children @0 minus y's children
    // @5" (z shared by both sides). Children @0 of y: n1:{3} n2:{3} n3:{4} n4:{}.
    // @5: n1:{3} n2:{3} n3:{4} n4:{5}. @0 minus @5 is empty for every y (the
    // retracted (0,2) edge belongs to n0, never a y here), so every row stays.
    let kids = |t| expand_on(Op::Unit, tv("y"), "parent_of", tv("z"), at(t), Access::E);
    assert_eq!(
        exec(
            &w,
            &["x", "y"],
            not(scan(0, "x", "y"), diff(kids(0), kids(5)))
        ),
        pairs(&n, &A_ALL)
    );
    // Reversed: children @5 minus children @0 is {5} for y = n4 only, so the body
    // has a row for (3,4) and that row is dropped.
    assert_eq!(
        exec(
            &w,
            &["x", "y"],
            not(scan(0, "x", "y"), diff(kids(5), kids(0)))
        ),
        pairs(&n, &[(0, 1), (0, 2), (1, 3), (2, 3)])
    );
}
