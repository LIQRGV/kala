//! Slice 7 security finding H1 (CONFIRM): edge cases of the interned,
//! step-charged value order in the executor.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{Datum, Limits, Params, QueryError, ResultSet, run};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn schema() -> Schema {
    let many = |n: &str, t: ValueType| AttrDef::new(n, t, Kind::State, Cardinality::Many);
    Schema::new()
        .node_type("CHARACTER")
        .attribute(many("bio", ValueType::Str).unindexed())
        .attribute(many("bio2", ValueType::Str).unindexed())
        .attribute(many("p", ValueType::Ref))
        .attribute(many("q", ValueType::I64))
}

fn exec(w: &World, q: &str, lim: &Limits) -> Result<ResultSet, QueryError> {
    let p = prepare(w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    run(w, &p.plan, &Params::new(), lim)
}

fn ok(w: &World, q: &str) -> ResultSet {
    exec(w, q, &Limits::default()).expect("Ok under default limits")
}

fn s(v: &str) -> Datum {
    Datum::Value(Value::Str(v.to_string()))
}

/// Entity `a` (handle) with the given bio strings at tick 0.
fn bio_world(strs: &[String]) -> World {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let mut t = w.begin();
    for x in strs {
        t.assert(a, "bio", Value::Str(x.clone()), Some(Tick(0)))
            .unwrap();
    }
    t.commit().unwrap();
    w
}

fn long(len: usize, suffix: &str) -> String {
    format!("{}{suffix}", "p".repeat(len))
}

#[test]
fn lt_filter_on_long_shared_prefix_strings_is_charged_and_correct() {
    let (x, y, z) = (long(20_000, "a"), long(20_000, "b"), long(20_000, "c"));
    let mut w = bio_world(&[x.clone(), y.clone(), z.clone()]);
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(b, "bio2", Value::Str(y.clone()), Some(Tick(0)))
        .unwrap();
    let q = "[:find ?s :where [:a :bio ?s] [:b :bio2 ?u] [(< ?s ?u)]]";
    let rs = ok(&w, q);
    assert_eq!(rs.rows, vec![vec![s(&x)]]);
    // One comparison of ~20 KB costs ~300 steps; the rest of the query is a
    // handful of steps, so a limit of 100 must trip in the filter.
    let lim = Limits {
        max_steps: 100,
        ..Limits::default()
    };
    assert!(
        matches!(
            exec(&w, q, &lim),
            Err(QueryError::StepBudget { limit: 100 })
        ),
        "filter comparisons must be charged"
    );
}

#[test]
fn min_max_over_long_shared_prefix_strings_are_right_and_charged() {
    let strs: Vec<String> = (0..40)
        .map(|i| long(20_000, &format!("{:03}", (i * 7) % 40)))
        .collect();
    let w = bio_world(&strs);
    let q = "[:find (min ?s) (max ?s) :where [:a :bio ?s]]";
    let rs = ok(&w, q);
    let want = vec![vec![
        s(strs.iter().min().unwrap()),
        s(strs.iter().max().unwrap()),
    ]];
    assert_eq!(rs.rows, want);
    // 40 rows scanned (~200 steps without comparisons); 78 comparisons of
    // ~313 steps each is far above 2_000.
    let lim = Limits {
        max_steps: 2_000,
        ..Limits::default()
    };
    assert!(
        matches!(exec(&w, q, &lim), Err(QueryError::StepBudget { .. })),
        "min/max comparisons must be charged"
    );
}

#[test]
fn equal_interval_sets_from_different_during_reads_dedupe() {
    let mut w = World::in_memory(schema()).unwrap();
    let t = w.create_entity("CHARACTER", Some("t")).unwrap();
    let mut ents = vec![];
    for h in ["e1", "e2", "e3"] {
        ents.push(w.create_entity("CHARACTER", Some(h)).unwrap());
    }
    let mut tx = w.begin();
    for e in &ents {
        for (i, tick) in [-10i64, 0, 5, 9].iter().enumerate() {
            if i % 2 == 0 {
                tx.assert(*e, "p", Value::Ref(t), Some(Tick(*tick)))
                    .unwrap();
            } else {
                tx.retract(*e, "p", Value::Ref(t), Some(Tick(*tick)))
                    .unwrap();
            }
        }
    }
    tx.commit().unwrap();
    let rs = ok(&w, "[:find ?s :where (during [:e1 :p :t] ?s)]");
    assert_eq!(rs.rows.len(), 1);
    let rs3 = ok(
        &w,
        "[:find ?s :where (during [:e1 :p :t] ?s) (during [:e2 :p :t] ?u) (during [:e3 :p :t] ?v) [(= ?s ?u)] [(= ?u ?v)]]",
    );
    assert_eq!(rs3.rows, rs.rows);
    let rs_each = ok(
        &w,
        "[:find ?e ?s :at -5 :where [?e :p :t] (during [?e :p :t] ?s)]",
    );
    assert_eq!(rs_each.rows.len(), 3);
    let rs_only = ok(
        &w,
        "[:find ?s :at -5 :where [?e :p :t] (during [?e :p :t] ?s)]",
    );
    assert_eq!(
        rs_only.rows.len(),
        1,
        "identical sets from different entities are one row"
    );
}

#[test]
fn string_constant_equal_to_a_world_string_changes_nothing() {
    let big = "z".repeat(30_000);
    let w = bio_world(&[big.clone(), "small".into()]);
    let plain = ok(&w, "[:find ?s :where [:a :bio ?s]]");
    let with_const = ok(
        &w,
        &format!("[:find ?s :where [:a :bio ?s] [(= ?s \"{big}\")]]"),
    );
    assert_eq!(with_const.rows, vec![vec![s(&big)]]);
    let c2 = ok(
        &w,
        &format!("[:find ?s :where [:a :bio ?s] [:a :bio \"{big}\"]]"),
    );
    assert_eq!(c2.rows, plain.rows);
    // Bytes: the data once (30 KB) plus slack; the constant must not be
    // counted again as a second copy per use.
    let lim = Limits {
        max_bytes: 400_000,
        ..Limits::default()
    };
    let q = format!("[:find ?s :where [:a :bio ?s] [:a :bio \"{big}\"]]");
    let r = exec(&w, &q, &lim);
    assert!(
        r.is_ok(),
        "constant equal to a world string must not double the byte count: {:?}",
        r.map(|r| r.rows.len())
    );
    let q0 = "[:find ?s :where [:a :bio ?s]]";
    assert!(exec(&w, q0, &lim).is_ok());
}

#[test]
fn final_rows_sorted_by_datum_order_across_mixed_types() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let mut t = w.begin();
    for x in ["pear", "apple", "pear2"] {
        t.assert(a, "bio", Value::Str(x.into()), Some(Tick(0)))
            .unwrap();
    }
    t.assert(a, "q", Value::I64(1), Some(Tick(0))).unwrap();
    t.assert(a, "q", Value::I64(-5), Some(Tick(0))).unwrap();
    t.assert(a, "p", Value::Ref(b), Some(Tick(0))).unwrap();
    t.assert(a, "p", Value::Ref(a), Some(Tick(0))).unwrap();
    t.retract(a, "p", Value::Ref(a), Some(Tick(5))).unwrap();
    t.commit().unwrap();
    let plan_of = |q: &str| prepare(&w, q).unwrap().plan;
    let mut plan = plan_of("[:find ?x :at 0 :where [:a :bio ?x]]");
    let branches = [
        "[:find ?x :at 0 :where [:a :bio ?x]]",
        "[:find ?x :at 0 :where [:a :q ?x]]",
        "[:find ?x :at 0 :where [:a :p ?x]]",
        "[:find ?x :at 0 :where [:a :p _ _ ?x]]",
        "[:find ?x :at 0 :where (during [:a :p :a] ?x)]",
    ];
    plan.root = kala_query::Op::Union {
        inputs: branches.iter().map(|q| plan_of(q).root).collect(),
    };
    let rs = run(&w, &plan, &Params::new(), &Limits::default()).expect("union runs");
    let col: Vec<Datum> = rs.rows.iter().map(|r| r[0].clone()).collect();
    assert!(
        col.len() >= 7,
        "expected strings, i64s, refs, ops and an interval set: {col:?}"
    );
    assert!(col.iter().any(|d| matches!(d, Datum::Op(_))));
    assert!(col.iter().any(|d| matches!(d, Datum::Intervals(_))));
    assert!(col.iter().any(|d| matches!(d, Datum::Value(Value::Str(_)))));
    assert!(col.iter().any(|d| matches!(d, Datum::Value(Value::I64(_)))));
    assert!(col.iter().any(|d| matches!(d, Datum::Value(Value::Ref(_)))));
    let mut want = col.clone();
    want.sort();
    want.dedup();
    assert_eq!(
        col, want,
        "rows must be sorted by Datum order without duplicates"
    );
}

fn word() -> impl Strategy<Value = String> {
    (
        prop::sample::select(vec![0usize, 1, 70, 130, 300]),
        prop::sample::select(vec!["", "a", "b", "ab", "zz"]),
    )
        .prop_map(|(n, suf)| long(n, suf))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn strings_match_naive_evaluation(
        xs in prop::collection::vec(word(), 0..8),
        ys in prop::collection::vec(word(), 0..8),
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let a = w.create_entity("CHARACTER", Some("a")).unwrap();
        let b = w.create_entity("CHARACTER", Some("b")).unwrap();
        let mut t = w.begin();
        for x in &xs { t.assert(a, "bio", Value::Str(x.clone()), Some(Tick(0))).unwrap(); }
        for y in &ys { t.assert(b, "bio2", Value::Str(y.clone()), Some(Tick(0))).unwrap(); }
        t.commit().unwrap();
        let xset: BTreeSet<&String> = xs.iter().collect();
        let yset: BTreeSet<&String> = ys.iter().collect();

        let rs = ok(&w, "[:find ?s :where [:a :bio ?s]]");
        let want: Vec<Vec<Datum>> = xset.iter().map(|x| vec![s(x)]).collect();
        prop_assert_eq!(rs.rows, want);

        for (op, f) in [("<", (|a: &str, b: &str| a < b) as fn(&str, &str) -> bool),
                        ("<=", |a, b| a <= b), (">", |a, b| a > b), (">=", |a, b| a >= b)] {
            let q = format!("[:find ?s ?u :where [:a :bio ?s] [:b :bio2 ?u] [({op} ?s ?u)]]");
            let rs = ok(&w, &q);
            let mut want: Vec<Vec<Datum>> = vec![];
            for x in &xset { for y in &yset { if f(x, y) { want.push(vec![s(x), s(y)]); } } }
            want.sort();
            prop_assert_eq!(rs.rows, want, "op {}", op);
        }

        if !xs.is_empty() {
            let rs = ok(&w, "[:find (min ?s) (max ?s) :where [:a :bio ?s]]");
            prop_assert_eq!(
                rs.rows,
                vec![vec![s(xset.iter().next().unwrap()), s(xset.iter().next_back().unwrap())]]
            );
        }
    }
}
