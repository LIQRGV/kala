//! Hash tables must count even when the join/negation produces no rows.
mod common;
use common::*;
use kala_core::{Schema, World};
use kala_query::{Limits, Op, Params, QueryError, RelDef, Term, run};

fn world(n: usize) -> World {
    let mut w = World::in_memory(Schema::new().node_type("A").node_type("B")).unwrap();
    let mut tx = w.begin();
    for _ in 0..n {
        tx.create_entity("A", None).unwrap();
        tx.create_entity("B", None).unwrap();
    }
    tx.commit().unwrap();
    w
}

fn disjoint_join() -> Op {
    join(node_scan(Op::Unit, "x", "A"), node_scan(Op::Unit, "x", "B"))
}

fn negation() -> Op {
    Op::Not {
        input: b(node_scan(Op::Unit, "x", "A")),
        body: b(Op::Unit),
    }
}

fn recursive_rel() -> Op {
    // The second rule is recursive but contributes no new tuples. The
    // body's four keyed positions expose the formerly constant-size
    // charge for a Rel index whose keys grow with arity.
    let vars: Vec<_> = ["a", "b", "c", "d"].into_iter().map(v).collect();
    let seed = node_scan(
        node_scan(node_scan(node_scan(Op::Unit, "a", "A"), "b", "A"), "c", "A"),
        "d",
        "A",
    );
    let probe = node_scan(
        node_scan(node_scan(node_scan(Op::Unit, "a", "B"), "b", "B"), "c", "B"),
        "d",
        "B",
    );
    let recursive = Op::Rel {
        input: b(probe),
        name: "r".into(),
        args: vars.iter().cloned().map(Term::Var).collect(),
    };
    let relation = RelDef {
        name: "r".into(),
        vars,
        rules: vec![seed, recursive],
    };
    Op::Fixpoint {
        rels: vec![relation],
        body: b(Op::Rel {
            input: b(Op::Unit),
            name: "r".into(),
            args: vec![ti(-1), ti(-1), ti(-1), ti(-1)],
        }),
    }
}

#[test]
fn join_table_alone_exceeds_remaining_memory() {
    let w = world(200);
    let p = plan(&["x"], disjoint_join());
    let ok = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    assert!(ok.rows.is_empty());
    let result = run(
        &w,
        &p,
        &Params::new(),
        &Limits {
            max_bytes: 32_000,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::MemoryBudget { .. })),
        "join table escaped memory budget: {result:?}"
    );
}

#[test]
fn negation_table_alone_exceeds_remaining_memory() {
    let w = world(200);
    let p = plan(&["x"], negation());
    let ok = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    assert!(ok.rows.is_empty());
    let result = run(
        &w,
        &p,
        &Params::new(),
        &Limits {
            max_bytes: 32_000,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::MemoryBudget { .. })),
        "negation table escaped memory budget: {result:?}"
    );
}

#[test]
fn recursive_rel_keys_count_toward_memory() {
    let w = world(4);
    let p = plan(&[], recursive_rel());
    let ok = run(&w, &p, &Params::new(), &Limits::default()).unwrap();
    assert!(ok.rows.is_empty());
    let result = run(
        &w,
        &p,
        &Params::new(),
        &Limits {
            max_bytes: 120_000,
            ..Limits::default()
        },
    );
    assert!(
        matches!(result, Err(QueryError::MemoryBudget { .. })),
        "recursive Rel index escaped memory budget: {result:?}"
    );
}

#[test]
fn smaller_tables_succeed_under_the_same_limits() {
    let w = world(4);
    for root in [disjoint_join(), negation()] {
        let p = plan(&["x"], root);
        let result = run(
            &w,
            &p,
            &Params::new(),
            &Limits {
                max_bytes: 32_000,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(result.rows.is_empty());
    }
    let w = world(2);
    let result = run(
        &w,
        &plan(&[], recursive_rel()),
        &Params::new(),
        &Limits {
            max_bytes: 120_000,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(result.rows.is_empty());
}
