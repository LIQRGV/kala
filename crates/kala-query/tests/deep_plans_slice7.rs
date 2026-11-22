//! Slice 7, item 1: deep plans through the new operators. `explain`, `Drop`
//! and the depth check stay iterative: every body runs on a 2 MiB stack.
//! RED until the new `Op` variants exist.

mod common;

use common::*;
use kala_core::{AttrDef, Cardinality, Kind, Schema, ValueType, World};
use kala_query::{
    Access, Agg, AggFn, Func, Limits, MAX_EXPLAIN_BYTES, MAX_PLAN_DEPTH, Op, Params, QueryError,
    RelDef, run,
};

fn on_small_stack<F: FnOnce() + Send + 'static>(f: F) {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test body panicked (stack overflow aborts the process instead)");
}

fn world() -> World {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "n",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ));
    World::in_memory(schema).unwrap()
}

fn leaf() -> Op {
    expand(tv("e"), "n", tv("x"), at(0), Access::Scan)
}

/// Nests `n` levels of `wrap` around a leaf.
fn nest(n: usize, wrap: fn(Op) -> Op) -> Op {
    let mut op = leaf();
    for _ in 0..n {
        op = wrap(op);
    }
    op
}

fn not_body(c: Op) -> Op {
    Op::Not {
        input: Box::new(Op::Unit),
        body: Box::new(c),
    }
}
fn not_input(c: Op) -> Op {
    Op::Not {
        input: Box::new(c),
        body: Box::new(Op::Unit),
    }
}
fn diff_right(c: Op) -> Op {
    Op::Difference {
        left: Box::new(Op::Unit),
        right: Box::new(c),
    }
}
fn inter_right(c: Op) -> Op {
    Op::Intersect {
        left: Box::new(Op::Unit),
        right: Box::new(c),
    }
}
fn fix_rule(c: Op) -> Op {
    Op::Fixpoint {
        rels: vec![RelDef {
            name: "r".into(),
            vars: vec![v("x")],
            rules: vec![c, Op::Unit],
        }],
        body: Box::new(Op::Unit),
    }
}
fn fix_body(c: Op) -> Op {
    Op::Fixpoint {
        rels: vec![],
        body: Box::new(c),
    }
}
fn rel_input(c: Op) -> Op {
    Op::Rel {
        input: Box::new(c),
        name: "r".into(),
        args: vec![tv("x")],
    }
}
fn agg_input(c: Op) -> Op {
    Op::Aggregate {
        input: Box::new(c),
        group: vec![v("x")],
        aggs: vec![Agg {
            func: AggFn::Count,
            var: v("x"),
            out: v("n"),
        }],
    }
}
fn apply_input(c: Op) -> Op {
    Op::Apply {
        input: Box::new(c),
        func: Func::NotEmpty,
        args: vec![tv("x")],
        out: None,
    }
}

type Wrap = (&'static str, fn(Op) -> Op);
const WRAPS: [Wrap; 9] = [
    ("not body", not_body),
    ("not input", not_input),
    ("difference right", diff_right),
    ("intersect right", inter_right),
    ("fixpoint rule", fix_rule),
    ("fixpoint body", fix_body),
    ("rel input", rel_input),
    ("aggregate input", agg_input),
    ("apply input", apply_input),
];

#[test]
fn explain_of_3000_deep_new_op_nesting_works_on_a_small_stack() {
    on_small_stack(|| {
        let w = world();
        for (name, wrap) in WRAPS {
            let p = plan(&["x"], nest(3_000, wrap));
            let text = p.explain(&w);
            assert!(text.starts_with("Find ?x"), "{name}");
            // Quadratic indentation exceeds the cap: truncated, bounded.
            assert!(
                text.len() <= MAX_EXPLAIN_BYTES + 64,
                "{name}: {} bytes",
                text.len()
            );
            assert!(text.ends_with("  ... (plan truncated)\n"), "{name}");
        }
    });
}

#[test]
fn explain_of_a_shallow_new_op_plan_is_not_truncated() {
    on_small_stack(|| {
        let w = world();
        let text = plan(&["x"], nest(20, not_body)).explain(&w);
        assert!(!text.contains("truncated"));
        assert_eq!(text.matches("Not\n").count(), 20);
    });
}

#[test]
fn dropping_100_000_deep_new_op_nesting_does_not_overflow() {
    on_small_stack(|| {
        for (name, wrap) in WRAPS {
            let op = nest(100_000, wrap);
            drop(op);
            let _ = name;
        }
    });
}

#[test]
fn clone_and_eq_of_deep_not_nesting_do_not_overflow_at_3000() {
    on_small_stack(|| {
        let a = nest(3_000, not_body);
        let c = a.clone();
        assert_eq!(a, c);
        drop((a, c));
    });
}

#[test]
fn running_a_3000_deep_plan_is_a_plan_error_not_a_stack_overflow() {
    on_small_stack(|| {
        let w = world();
        for (name, wrap) in WRAPS {
            let r = run(
                &w,
                &plan(&["x"], nest(3_000, wrap)),
                &Params::new(),
                &Limits::default(),
            );
            if name == "not input" {
                // Item 4: Not runs. A 3000-deep chain of Not-with-Unit-body over
                // the leaf must finish without overflowing the stack, with no rows.
                let rs = r.unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert!(rs.rows.is_empty(), "{name}: {:?}", rs.rows);
                continue;
            }
            assert!(
                matches!(&r, Err(QueryError::Plan(m)) if !m.contains("too deep")),
                "{name}: {r:?}"
            );
        }
    });
}

#[test]
fn running_a_plan_far_deeper_than_max_is_refused_as_too_deep() {
    on_small_stack(|| {
        let w = world();
        for (name, wrap) in WRAPS {
            let op = nest(MAX_PLAN_DEPTH * 3, wrap);
            let r = run(&w, &plan(&["x"], op), &Params::new(), &Limits::default());
            assert!(
                matches!(&r, Err(QueryError::Plan(m)) if m.contains("too deep")),
                "{name}: {r:?}"
            );
        }
    });
}
