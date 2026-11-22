//! Edge cases for security fix H1 (grouped per-tick scans for `(at ?t ..)`
//! scan patterns): error precedence, cache reuse, extreme ticks, several
//! scan patterns, and limits inside the grouped path.

use kala_core::*;
use kala_datalog::prepare;
use kala_query::{
    Access, AttrRef, CanonSource, Limits, Op, Params, Plan, QueryError, ResultSet, Term,
    TickSource, Var, run,
};

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "year",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "label",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "score",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "rank",
            ValueType::I64,
            Kind::State,
            Cardinality::Many,
        ))
}

fn exec(w: &World, q: &str, lim: &Limits) -> Result<ResultSet, QueryError> {
    let p = prepare(w, q).unwrap_or_else(|e| panic!("prepare failed: {e}"));
    run(w, &p.plan, &Params::new(), lim)
}

/// Characters with the given years, each with `score` and `rank` facts that
/// change around the years and at extreme ticks.
fn world(years: &[i64]) -> (World, Vec<EntityId>) {
    let mut w = World::in_memory(schema()).unwrap();
    let mut ents = Vec::new();
    for (i, y) in years.iter().enumerate() {
        let e = w.create_entity("CHARACTER", None).unwrap();
        w.assert(e, "year", Value::I64(*y), Some(Tick(0))).unwrap();
        w.assert(e, "label", Value::Str(format!("c{i}")), Some(Tick(0)))
            .unwrap();
        let i = i as i64;
        w.assert(e, "score", Value::I64(i), Some(Tick(i64::MIN)))
            .unwrap();
        w.assert(e, "score", Value::I64(100 + i), Some(Tick(0)))
            .unwrap();
        w.retract(e, "score", Value::I64(i), Some(Tick(5))).unwrap();
        w.assert(e, "score", Value::I64(200 + i), Some(Tick(i64::MAX)))
            .unwrap();
        w.assert(e, "rank", Value::I64(i % 2), Some(Tick(-1)))
            .unwrap();
        w.retract(e, "rank", Value::I64(i % 2), Some(Tick(7)))
            .unwrap();
        w.assert(e, "rank", Value::I64(9), Some(Tick(7))).unwrap();
        ents.push(e);
    }
    (w, ents)
}

/// The Datalog front-end rejects a string-typed `?t` statically, so the plan
/// is built by hand at the IR level (as another front-end could).
fn string_tick_plan() -> Plan {
    let v = |n: &str| Var(n.to_string());
    let label = Op::Expand {
        input: Box::new(Op::Unit),
        entity: Term::Var(v("a")),
        attr: AttrRef::Named("label".into()),
        value: Term::Var(v("t")),
        tick: TickSource::Const(Tick(0)),
        access: Access::Scan,
    };
    let scan = Op::Expand {
        input: Box::new(label),
        entity: Term::Var(v("x")),
        attr: AttrRef::Named("score".into()),
        value: Term::Var(v("y")),
        tick: TickSource::Var(v("t")),
        access: Access::Scan,
    };
    Plan {
        find: vec![v("x")],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: scan,
    }
}

#[test]
fn non_i64_tick_with_tight_budget_reports_which_error_wins() {
    let (w, _) = world(&[1, 2, 3]);
    let plan = string_tick_plan();
    let go = |max_steps| {
        run(
            &w,
            &plan,
            &Params::new(),
            &Limits {
                max_rows: 100,
                max_steps,
                ..Limits::default()
            },
        )
    };
    // The smallest budget that gets past the first operator (the `label`
    // read): below it the budget wins, at and above it the type error wins,
    // because grouping reads every row's tick before any scan is charged.
    let first_ok = (0..1_000u64)
        .find(|s| !matches!(go(*s), Err(QueryError::StepBudget { .. })))
        .expect("some budget gets past the first operator");
    assert!(first_ok > 0);
    for s in [first_ok, first_ok + 1, first_ok + 50, 10_000_000] {
        let r = go(s);
        assert!(
            matches!(r, Err(QueryError::TypeMismatch(_))),
            "steps {s}: got {r:?}"
        );
    }
    let r = go(first_ok - 1);
    assert!(matches!(r, Err(QueryError::StepBudget { .. })), "got {r:?}");
}

#[test]
fn same_attribute_at_constant_and_variable_equal_tick_matches_native() {
    let years = [5, 0, 5, -3, 7, 0];
    let (w, ents) = world(&years);
    let q =
        "[:find ?a ?t ?x ?y ?z :where [?a :year ?t] (at ?t [?x :score ?y]) (at 5 [?x :score ?z])]";
    let rs = exec(&w, q, &Limits::default()).unwrap();
    let at5 = w.attr_scan("score", Tick(5)).unwrap();
    let mut expected = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        for (x, vy) in w.attr_scan("score", Tick(y)).unwrap() {
            for (x2, vz) in &at5 {
                if *x2 == x {
                    expected.push(vec![
                        Value::Ref(*e),
                        Value::I64(y),
                        Value::Ref(x),
                        vy.clone(),
                        vz.clone(),
                    ]);
                }
            }
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);

    // Same variable name for the value: joined, so only equal values survive.
    let q = "[:find ?a ?x ?y :where [?a :year ?t] (at ?t [?x :score ?y]) (at 5 [?x :score ?y])]";
    let rs = exec(&w, q, &Limits::default()).unwrap();
    let mut expected = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        for (x, v) in w.attr_scan("score", Tick(y)).unwrap() {
            if at5.contains(&(x, v.clone())) {
                expected.push(vec![Value::Ref(*e), Value::Ref(x), v]);
            }
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);
}

#[test]
fn extreme_tick_variables_match_native_attr_scan() {
    let years = [i64::MIN, i64::MAX, 0, i64::MIN, 0, i64::MAX, -1, 1];
    let (w, ents) = world(&years);
    let q = "[:find ?a ?t ?x ?y :where [?a :year ?t] (at ?t [?x :score ?y])]";
    let rs = exec(&w, q, &Limits::default()).unwrap();
    let mut expected = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        for (x, v) in w.attr_scan("score", Tick(y)).unwrap() {
            expected.push(vec![Value::Ref(*e), Value::I64(y), Value::Ref(x), v]);
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);
}

#[test]
fn two_variable_tick_scans_same_and_different_attributes_match_native() {
    let years = [7, -2, 7, 0, 100, 5, -2];
    let (w, ents) = world(&years);

    // Different attributes, same ?t.
    let q =
        "[:find ?a ?t ?x ?y ?r :where [?a :year ?t] (at ?t [?x :score ?y]) (at ?t [?x :rank ?r])]";
    let rs = exec(&w, q, &Limits::default()).unwrap();
    let mut expected = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        let ranks = w.attr_scan("rank", Tick(y)).unwrap();
        for (x, vy) in w.attr_scan("score", Tick(y)).unwrap() {
            for (x2, r) in &ranks {
                if *x2 == x {
                    expected.push(vec![
                        Value::Ref(*e),
                        Value::I64(y),
                        Value::Ref(x),
                        vy.clone(),
                        r.clone(),
                    ]);
                }
            }
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);

    // Same attribute at two different tick variables (from two year rows).
    let q = "[:find ?a ?b ?x ?y ?z :where [?a :year ?t] [?b :year ?u] (at ?t [?x :score ?y]) (at ?u [?x :score ?z])]";
    let rs = exec(&w, q, &Limits::default()).unwrap();
    let mut expected = Vec::new();
    for (e, y) in ents.iter().zip(years) {
        for (f, u) in ents.iter().zip(years) {
            let right = w.attr_scan("score", Tick(u)).unwrap();
            for (x, vy) in w.attr_scan("score", Tick(y)).unwrap() {
                for (x2, vz) in &right {
                    if *x2 == x {
                        expected.push(vec![
                            Value::Ref(*e),
                            Value::Ref(*f),
                            Value::Ref(x),
                            vy.clone(),
                            vz.clone(),
                        ]);
                    }
                }
            }
        }
    }
    expected.sort();
    expected.dedup();
    assert!(!expected.is_empty());
    assert_eq!(rs.rows, expected);
}

#[test]
fn limits_inside_the_grouped_path_error_and_never_truncate() {
    let years = [1, 2, 3, 4, 5, 6];
    let (w, _) = world(&years);
    let q = "[:find ?a ?t ?x ?y :where [?a :year ?t] (at ?t [?x :score ?y])]";
    let full = exec(&w, q, &Limits::default()).unwrap();
    let n = full.rows.len();
    assert!(n > 10);

    // Row limit: the scan operator produces `n` rows (before dedup); a limit
    // below that must error, a limit at or above must return everything.
    let r = exec(
        &w,
        q,
        &Limits {
            max_rows: n - 1,
            max_steps: 10_000_000,
            ..Limits::default()
        },
    );
    assert!(matches!(r, Err(QueryError::RowLimit { .. })), "got {r:?}");
    let r = exec(
        &w,
        q,
        &Limits {
            max_rows: n,
            max_steps: 10_000_000,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(r.rows, full.rows);

    // Step budget: every smaller budget errors; none returns a partial set.
    for steps in [0u64, 1, 5, 10, 20] {
        match exec(
            &w,
            q,
            &Limits {
                max_rows: 100_000,
                max_steps: steps,
                ..Limits::default()
            },
        ) {
            Err(QueryError::StepBudget { .. }) => {}
            Ok(r) => panic!(
                "steps {steps}: expected StepBudget, got {} rows",
                r.rows.len()
            ),
            Err(e) => panic!("steps {steps}: unexpected {e:?}"),
        }
    }
    // A budget between "first scan fits" and "all fit" errors mid-grouping.
    let r = exec(
        &w,
        q,
        &Limits {
            max_rows: 100_000,
            max_steps: (n as u64) + 6,
            ..Limits::default()
        },
    );
    assert!(matches!(r, Err(QueryError::StepBudget { .. })), "got {r:?}");
}
