//! Slice 7, item 3 (confirm): hand-built IR edge cases for recursive
//! `Fixpoint`s: one rule that is a `Union` of a base branch and a recursive
//! branch (in either order), and a `Fixpoint` nested in a rule that reads the
//! enclosing relation (refused with `QueryError::Plan`: pinned as the current
//! contract, since semi-naive evaluation is only sound for reads joined
//! straight into the rule's rows).
//!
//! World: n0..n4 (CHARACTER); parent_of n0->n1, n1->n2, n2->n3, n3->n1 (a
//! cycle), n4 alone. Closure `anc`: from n0 {n1,n2,n3}; from each of n1, n2,
//! n3 {n1,n2,n3} (itself included, through the cycle); from n4 nothing.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, Limits, Op, Params, QueryError, RelDef, Term, run};

fn world() -> (World, Vec<EntityId>) {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "parent_of",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let n: Vec<EntityId> = (0..5)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    for (a, b) in [(0, 1), (1, 2), (2, 3), (3, 1)] {
        w.assert(n[a], "parent_of", Value::Ref(n[b]), Some(Tick(0)))
            .unwrap();
    }
    (w, n)
}

fn rel(input: Op, name: &str, args: Vec<Term>) -> Op {
    Op::Rel {
        input: b(input),
        name: name.into(),
        args,
    }
}

fn par() -> Op {
    expand(tv("x"), "parent_of", tv("y"), at(0), Access::Scan)
}

/// `x -> m`, then the recursive read `anc(m, y)`, projected onto (x, y).
fn recursive_branch() -> Op {
    project(
        rel(
            expand(tv("x"), "parent_of", tv("m"), at(0), Access::Scan),
            "anc",
            vec![tv("m"), tv("y")],
        ),
        &["x", "y"],
    )
}

fn anc(rules: Vec<Op>) -> RelDef {
    RelDef {
        name: "anc".into(),
        vars: vec![v("x"), v("y")],
        rules,
    }
}

fn exec(w: &World, root: Op) -> Result<Vec<Vec<Value>>, QueryError> {
    let p = plan(&["a", "b"], project(root, &["a", "b"]));
    run(w, &p, &Params::new(), &Limits::default()).map(|rs| value_rows(&rs))
}

fn pairs(n: &[EntityId], ps: &[(usize, usize)]) -> Vec<Vec<Value>> {
    sorted_rows(
        ps.iter()
            .map(|&(a, b)| vec![Value::Ref(n[a]), Value::Ref(n[b])])
            .collect(),
    )
}

fn closure(n: &[EntityId]) -> Vec<Vec<Value>> {
    pairs(
        n,
        &[
            (0, 1),
            (0, 2),
            (0, 3),
            (1, 1),
            (1, 2),
            (1, 3),
            (2, 1),
            (2, 2),
            (2, 3),
            (3, 1),
            (3, 2),
            (3, 3),
        ],
    )
}

fn call() -> Op {
    rel(Op::Unit, "anc", vec![tv("a"), tv("b")])
}

#[test]
fn a_single_rule_that_unions_a_base_branch_and_a_recursive_branch() {
    let (w, n) = world();
    let rule = project(
        Op::Union {
            inputs: vec![par(), recursive_branch()],
        },
        &["x", "y"],
    );
    let root = Op::Fixpoint {
        rels: vec![anc(vec![rule])],
        body: b(call()),
    };
    assert_eq!(exec(&w, root).unwrap(), closure(&n));
}

#[test]
fn the_union_branch_order_does_not_matter() {
    let (w, n) = world();
    let rule = project(
        Op::Union {
            inputs: vec![recursive_branch(), par()],
        },
        &["x", "y"],
    );
    let root = Op::Fixpoint {
        rels: vec![anc(vec![rule])],
        body: b(call()),
    };
    assert_eq!(exec(&w, root).unwrap(), closure(&n));
}

#[test]
fn a_union_rule_alongside_a_plain_rule_of_the_same_relation() {
    let (w, n) = world();
    let union_rule = project(
        Op::Union {
            inputs: vec![par(), recursive_branch()],
        },
        &["x", "y"],
    );
    let plain = project(par(), &["x", "y"]);
    let root = Op::Fixpoint {
        rels: vec![anc(vec![plain, union_rule])],
        body: b(call()),
    };
    assert_eq!(exec(&w, root).unwrap(), closure(&n));
}

#[test]
fn a_nested_fixpoint_reading_the_outer_relation_is_a_plan_error() {
    let (w, _) = world();
    // z(m, y) = anc(m, y), defined in a Fixpoint nested inside anc's own rule
    let z = RelDef {
        name: "z".into(),
        vars: vec![v("m"), v("y")],
        rules: vec![project(
            rel(Op::Unit, "anc", vec![tv("m"), tv("y")]),
            &["m", "y"],
        )],
    };
    let nested = Op::Fixpoint {
        rels: vec![z],
        body: b(project(
            Op::Join {
                left: b(expand(tv("x"), "parent_of", tv("m"), at(0), Access::Scan)),
                right: b(rel(Op::Unit, "z", vec![tv("m"), tv("y")])),
            },
            &["x", "y"],
        )),
    };
    let root = Op::Fixpoint {
        rels: vec![anc(vec![project(par(), &["x", "y"]), nested])],
        body: b(call()),
    };
    match exec(&w, root) {
        Err(QueryError::Plan(m)) => assert!(m.contains("anc"), "{m}"),
        other => panic!("expected a Plan error naming the relation, got {other:?}"),
    }
}
