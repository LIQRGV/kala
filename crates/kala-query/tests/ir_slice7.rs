//! Slice 7, item 1: IR additions (docs/slice-7.md, item 1 and "Syntax
//! decisions"). RED until `Datum`, `OpTerm`, `Func`, `AggFn`, `Agg`, `RelDef`
//! and the new `Op` variants (History, During, Apply, Not, Difference,
//! Intersect, Aggregate, Fixpoint, Rel) exist at the `kala_query` root.
//!
//! Expected API: see the item-1 contract (Datum order: every Value < Op <
//! Intervals; `Plan::explain` lines per variant; `run`/`execute` of a plan
//! containing a new op is `QueryError::Plan(msg)` with msg naming the op,
//! until the item that implements it).

mod common;

use std::collections::HashSet;

use common::*;
use kala_core::{
    AttrDef, Cardinality, EntityId, Interval, IntervalSet, Kind, Schema, Tick, Value, ValueType,
    World,
};
use kala_query::{
    Access, Agg, AggFn, Datum, Func, Limits, MAX_PLAN_DEPTH, Op, OpTerm, Params, QueryError,
    RelDef, Term, TickSource, run,
};

fn world() -> (World, EntityId) {
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "parent_of",
        ValueType::Ref,
        Kind::State,
        Cardinality::Many,
    ));
    let mut w = World::in_memory(schema).unwrap();
    let e = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    (w, e)
}

fn iv(from: i64, until: Option<i64>) -> Interval {
    Interval {
        from: Tick(from),
        until: until.map(Tick),
    }
}
fn set(ivs: &[(i64, Option<i64>)]) -> IntervalSet {
    IntervalSet::from_intervals(ivs.iter().map(|&(f, u)| iv(f, u)))
}

// ---------------------------------------------------------------- Datum

#[test]
fn datum_order_values_then_op_then_intervals() {
    let big_value = Datum::from(Value::Str("zzzz".into()));
    let ref_value = Datum::from(Value::Ref(world().1));
    let a = Datum::from(kala_core::Op::Assert);
    let r = Datum::from(kala_core::Op::Retract);
    let empty = Datum::from(IntervalSet::empty());
    assert!(big_value < a);
    assert!(ref_value < a);
    let fl = Datum::from(Value::Bool(false));
    assert!(fl < a);
    assert!(a < r);
    assert!(r < empty);
    assert!(a < empty);
    assert!(big_value < empty);
    // Within Values: Value order. Within Intervals: IntervalSet order.
    let (m5, p3) = (Datum::from(Value::I64(-5)), Datum::from(Value::I64(3)));
    assert!(m5 < p3);
    let (s1, s2) = (set(&[(-300, Some(-250))]), set(&[(-200, None)]));
    assert_eq!(s1.cmp(&s2), std::cmp::Ordering::Less);
    let (d1, d2) = (Datum::from(s1), Datum::from(s2));
    assert!(d1 < d2);
}

#[test]
fn datum_from_eq_hash_clone_and_as_value() {
    let d = Datum::from(Value::I64(7));
    assert_eq!(d, Datum::Value(Value::I64(7)));
    assert_eq!(d.clone(), d);
    assert_eq!(d.as_value(), Some(&Value::I64(7)));
    assert_eq!(Datum::from(kala_core::Op::Retract).as_value(), None);
    assert_eq!(Datum::from(IntervalSet::empty()).as_value(), None);
    assert_eq!(
        Datum::from(kala_core::Op::Assert),
        Datum::Op(kala_core::Op::Assert)
    );
    let s = set(&[(0, Some(5))]);
    assert_eq!(Datum::from(s.clone()), Datum::Intervals(s));
    let h: HashSet<Datum> = [d.clone(), d.clone(), Datum::from(kala_core::Op::Assert)].into();
    assert_eq!(h.len(), 2);
    assert!(format!("{d:?}").contains("I64"));
}

#[test]
fn datum_compares_with_value_both_ways_and_never_across_kinds() {
    let d = Datum::from(Value::Str("a".into()));
    assert!(d == Value::Str("a".into()));
    assert!(Value::Str("a".into()) == d);
    assert!(d != Value::Str("b".into()));
    assert!(Value::Bool(true) != Datum::from(kala_core::Op::Assert));
    assert!(Datum::from(kala_core::Op::Assert) != Value::Bool(true));
    assert!(Datum::from(IntervalSet::empty()) != Value::I64(0));
    // Vec<Vec<Datum>> == Vec<Vec<Value>> keeps old assertions compiling.
    assert_eq!(vec![vec![d]], vec![vec![Value::Str("a".into())]]);
}

// ----------------------------------------------- structural equality

fn history_op() -> Op {
    Op::History {
        input: b(Op::Unit),
        entity: tv("g"),
        attr: "located_in".into(),
        value: tv("p"),
        tick: tv("t"),
        op: OpTerm::Var(v("op")),
    }
}

#[test]
fn new_types_clone_debug_eq() {
    assert_eq!(OpTerm::Any, OpTerm::Any.clone());
    assert_ne!(
        OpTerm::Const(kala_core::Op::Assert),
        OpTerm::Const(kala_core::Op::Retract)
    );
    assert_ne!(OpTerm::Var(v("a")), OpTerm::Var(v("b")));
    let f = Func::NotEmpty;
    let g = f; // Copy
    assert_eq!(f, g);
    assert_ne!(Func::Union, Func::Difference);
    let a = Agg {
        func: AggFn::Sum,
        var: v("x"),
        out: v("n"),
    };
    assert_eq!(a.clone(), a);
    assert_ne!(
        a,
        Agg {
            func: AggFn::Max,
            ..a.clone()
        }
    );
    let r = RelDef {
        name: "anc".into(),
        vars: vec![v("a"), v("d")],
        rules: vec![Op::Unit],
    };
    assert_eq!(r.clone(), r);
    assert_ne!(
        r,
        RelDef {
            name: "other".into(),
            ..r.clone()
        }
    );
    assert!(!format!("{a:?}{r:?}{:?}", OpTerm::Any).is_empty());
}

#[test]
fn func_and_aggfn_names() {
    assert_eq!(
        [
            Func::Intersect,
            Func::Union,
            Func::Difference,
            Func::NotEmpty,
            Func::Contains
        ]
        .map(Func::name),
        ["intersect", "union", "difference", "not-empty", "contains"]
    );
    assert_eq!(
        [
            AggFn::Count,
            AggFn::CountDistinct,
            AggFn::Min,
            AggFn::Max,
            AggFn::Sum
        ]
        .map(AggFn::name),
        ["count", "count-distinct", "min", "max", "sum"]
    );
}

fn all_new_ops() -> Vec<(&'static str, Op)> {
    let (_, e) = world();
    vec![
        ("History", history_op()),
        (
            "During",
            Op::During {
                input: b(Op::Unit),
                entity: tr(e),
                attr: "parent_of".into(),
                value: tv("c"),
                out: v("ie"),
            },
        ),
        (
            "Apply",
            Op::Apply {
                input: b(Op::Unit),
                func: Func::Intersect,
                args: vec![tv("ie"), tv("im")],
                out: Some(v("both")),
            },
        ),
        (
            "Not",
            Op::Not {
                input: b(Op::Unit),
                body: b(leaf_expand(e)),
            },
        ),
        (
            "Difference",
            Op::Difference {
                left: b(Op::Unit),
                right: b(leaf_expand(e)),
            },
        ),
        (
            "Intersect",
            Op::Intersect {
                left: b(Op::Unit),
                right: b(leaf_expand(e)),
            },
        ),
        (
            "Aggregate",
            Op::Aggregate {
                input: b(leaf_expand(e)),
                group: vec![v("p")],
                aggs: vec![Agg {
                    func: AggFn::Count,
                    var: v("c"),
                    out: v("n"),
                }],
            },
        ),
        (
            "Fixpoint",
            Op::Fixpoint {
                rels: vec![RelDef {
                    name: "anc".into(),
                    vars: vec![v("a")],
                    rules: vec![Op::Unit],
                }],
                body: b(Op::Unit),
            },
        ),
        (
            "Rel",
            Op::Rel {
                input: b(Op::Unit),
                name: "anc".into(),
                args: vec![tv("a")],
            },
        ),
    ]
}

fn leaf_expand(e: EntityId) -> Op {
    expand(tr(e), "parent_of", tv("c"), at(0), Access::E)
}

#[test]
fn new_ops_clone_and_compare_structurally() {
    for (name, op) in all_new_ops() {
        assert_eq!(op.clone(), op, "{name}");
        assert!(!format!("{op:?}").is_empty());
    }
    let ops = all_new_ops();
    for (i, (n1, a)) in ops.iter().enumerate() {
        for (j, (n2, c)) in ops.iter().enumerate() {
            assert_eq!(i == j, a == c, "{n1} vs {n2}");
        }
    }
    // Field changes are visible.
    let mut other = history_op();
    if let Op::History { op, .. } = &mut other {
        *op = OpTerm::Const(kala_core::Op::Assert);
    }
    assert_ne!(other, history_op());
    let apply = |out| Op::Apply {
        input: b(Op::Unit),
        func: Func::NotEmpty,
        args: vec![tv("x")],
        out,
    };
    assert_ne!(apply(None), apply(Some(v("y"))));
}

// ------------------------------------------------------------- explain

fn one(root: Op) -> kala_query::Plan {
    plan(&["x"], root)
}
fn body_of(w: &World, p: &kala_query::Plan) -> String {
    // The text after the `Find` header line.
    p.explain(w).split_once('\n').unwrap().1.to_owned()
}

#[test]
fn explain_history_lines() {
    let (w, e) = world();
    let h = |entity, value, tick, op| Op::History {
        input: b(Op::Unit),
        entity,
        attr: "located_in".into(),
        value,
        tick,
        op,
    };
    assert_eq!(
        body_of(&w, &one(h(tv("g"), tv("p"), tv("t"), OpTerm::Var(v("op"))))),
        "  History [?g :located_in ?p ?t ?op]\n    Unit\n"
    );
    assert_eq!(
        body_of(&w, &one(h(tr(e), ti(3), tv("t"), OpTerm::Any))),
        "  History [CHARACTER#1 :located_in 3 ?t _]\n    Unit\n"
    );
    let p = one(Op::History {
        input: b(Op::Unit),
        entity: tr(e),
        attr: "missing_arm".into(),
        value: Term::Const(Value::Bool(true)),
        tick: tv("t"),
        op: OpTerm::Const(kala_core::Op::Assert),
    });
    assert_eq!(
        body_of(&w, &p),
        "  History [CHARACTER#1 :missing_arm true ?t :assert]\n    Unit\n"
    );
    let p = one(h(
        Term::Any,
        tp("v"),
        tv("t"),
        OpTerm::Const(kala_core::Op::Retract),
    ));
    assert_eq!(
        body_of(&w, &p),
        "  History [_ :located_in $v ?t :retract]\n    Unit\n"
    );
}

#[test]
fn explain_during_apply_lines() {
    let (w, e) = world();
    let during = Op::During {
        input: b(Op::Unit),
        entity: tr(e),
        attr: "located_in".into(),
        value: ts("Varrow"),
        out: v("ie"),
    };
    assert_eq!(
        body_of(&w, &one(during.clone())),
        "  During [CHARACTER#1 :located_in \"Varrow\"] -> ?ie\n    Unit\n"
    );
    let apply = |func, args: Vec<Term>, out| Op::Apply {
        input: b(Op::Unit),
        func,
        args,
        out,
    };
    assert_eq!(
        body_of(
            &w,
            &one(apply(
                Func::Intersect,
                vec![tv("ie"), tv("im")],
                Some(v("both"))
            ))
        ),
        "  Apply intersect(?ie, ?im) -> ?both\n    Unit\n"
    );
    assert_eq!(
        body_of(&w, &one(apply(Func::NotEmpty, vec![tv("both")], None))),
        "  Apply not-empty(?both)\n    Unit\n"
    );
    assert_eq!(
        body_of(
            &w,
            &one(apply(Func::Contains, vec![tv("ie"), ti(-230)], None))
        ),
        "  Apply contains(?ie, -230)\n    Unit\n"
    );
    assert_eq!(
        body_of(
            &w,
            &one(apply(Func::Union, vec![tv("a"), tp("b")], Some(v("u"))))
        ),
        "  Apply union(?a, $b) -> ?u\n    Unit\n"
    );
    assert_eq!(
        body_of(
            &w,
            &one(apply(
                Func::Difference,
                vec![tv("a"), tv("b")],
                Some(v("d"))
            ))
        ),
        "  Apply difference(?a, ?b) -> ?d\n    Unit\n"
    );
    // Input chains under the operator.
    let chained = Op::Apply {
        input: b(during),
        func: Func::NotEmpty,
        args: vec![tv("ie")],
        out: None,
    };
    assert_eq!(
        body_of(&w, &one(chained)),
        "  Apply not-empty(?ie)\n    During [CHARACTER#1 :located_in \"Varrow\"] -> ?ie\n      Unit\n"
    );
}

#[test]
fn explain_not_difference_intersect_children_input_first() {
    let (w, e) = world();
    let l = || leaf_expand(e);
    let r = || expand(tv("c"), "parent_of", tv("d"), at(1), Access::Scan);
    let expect = |name: &str| {
        format!(
            "  {name}\n    Expand [CHARACTER#1 :parent_of ?c] @0 via E\n      Unit\n    Expand [?c :parent_of ?d] @1 via scan\n      Unit\n"
        )
    };
    assert_eq!(
        body_of(
            &w,
            &one(Op::Not {
                input: b(l()),
                body: b(r())
            })
        ),
        expect("Not")
    );
    assert_eq!(
        body_of(
            &w,
            &one(Op::Difference {
                left: b(l()),
                right: b(r())
            })
        ),
        expect("Difference")
    );
    assert_eq!(
        body_of(
            &w,
            &one(Op::Intersect {
                left: b(l()),
                right: b(r())
            })
        ),
        expect("Intersect")
    );
}

#[test]
fn explain_aggregate_and_rel_lines() {
    let (w, e) = world();
    let agg = |group: &[&str], aggs: Vec<Agg>| Op::Aggregate {
        input: b(Op::Unit),
        group: group.iter().map(|g| v(g)).collect(),
        aggs,
    };
    let a = |func, var, out| Agg {
        func,
        var: v(var),
        out: v(out),
    };
    assert_eq!(
        body_of(&w, &one(agg(&["p"], vec![a(AggFn::Count, "c", "n")]))),
        "  Aggregate ?p: count ?c -> ?n\n    Unit\n"
    );
    assert_eq!(
        body_of(
            &w,
            &one(agg(
                &["p", "q"],
                vec![a(AggFn::Count, "c", "n"), a(AggFn::Max, "t", "m")]
            ))
        ),
        "  Aggregate ?p ?q: count ?c -> ?n, max ?t -> ?m\n    Unit\n"
    );
    assert_eq!(
        body_of(&w, &one(agg(&[], vec![a(AggFn::CountDistinct, "c", "n")]))),
        "  Aggregate: count-distinct ?c -> ?n\n    Unit\n"
    );
    assert_eq!(
        body_of(
            &w,
            &one(agg(
                &[],
                vec![a(AggFn::Min, "c", "lo"), a(AggFn::Sum, "c", "s")]
            ))
        ),
        "  Aggregate: min ?c -> ?lo, sum ?c -> ?s\n    Unit\n"
    );
    let rel = Op::Rel {
        input: b(Op::Unit),
        name: "ancestor".into(),
        args: vec![tr(e), tv("d")],
    };
    assert_eq!(
        body_of(&w, &one(rel)),
        "  Rel ancestor(CHARACTER#1, ?d)\n    Unit\n"
    );
}

#[test]
fn explain_fixpoint_with_define_children() {
    let (w, e) = world();
    let base = project(
        expand(
            tv("a"),
            "parent_of",
            tv("d"),
            TickSource::Default,
            Access::Scan,
        ),
        &["a", "d"],
    );
    let step = project(
        Op::Rel {
            input: b(expand(
                tv("a"),
                "parent_of",
                tv("x"),
                TickSource::Default,
                Access::Scan,
            )),
            name: "ancestor".into(),
            args: vec![tv("x"), tv("d")],
        },
        &["a", "d"],
    );
    let fix = Op::Fixpoint {
        rels: vec![RelDef {
            name: "ancestor".into(),
            vars: vec![v("a"), v("d")],
            rules: vec![base, step],
        }],
        body: b(Op::Rel {
            input: b(Op::Unit),
            name: "ancestor".into(),
            args: vec![tr(e), tv("d")],
        }),
    };
    let p = plan(&["d"], project(fix, &["d"]));
    assert_eq!(
        p.explain(&w),
        "Find ?d  at latest  canon latest\n\
         \x20 Project ?d\n\
         \x20   Fixpoint\n\
         \x20     Define ancestor ?a ?d\n\
         \x20       Project ?a ?d\n\
         \x20         Expand [?a :parent_of ?d] @latest via scan\n\
         \x20           Unit\n\
         \x20       Project ?a ?d\n\
         \x20         Rel ancestor(?x, ?d)\n\
         \x20           Expand [?a :parent_of ?x] @latest via scan\n\
         \x20             Unit\n\
         \x20     Rel ancestor(CHARACTER#1, ?d)\n\
         \x20       Unit\n"
    );
}

#[test]
fn explain_fixpoint_two_rels_and_empty_rules() {
    let (w, _) = world();
    let fix = Op::Fixpoint {
        rels: vec![
            RelDef {
                name: "p".into(),
                vars: vec![v("a")],
                rules: vec![],
            },
            RelDef {
                name: "q".into(),
                vars: vec![],
                rules: vec![Op::Unit],
            },
        ],
        body: b(Op::Unit),
    };
    assert_eq!(
        body_of(&w, &one(fix)),
        "  Fixpoint\n    Define p ?a\n    Define q\n      Unit\n    Unit\n"
    );
}

// ----------------------------------------------------------- depth

/// Fixpoint over one RelDef whose single rule is an Expand chain of `nodes`
/// operators (Unit included): the deepest path is Fixpoint + chain.
fn fixpoint_with_chain(e: EntityId, nodes: usize) -> Op {
    let mut op = leaf_expand(e);
    for _ in 0..nodes - 2 {
        op = expand_on(op, tr(e), "parent_of", tv("c"), at(0), Access::E);
    }
    Op::Fixpoint {
        rels: vec![RelDef {
            name: "r".into(),
            vars: vec![v("c")],
            rules: vec![op],
        }],
        body: b(Op::Unit),
    }
}

fn plan_msg(w: &World, root: Op) -> String {
    match run(w, &plan(&["c"], root), &Params::new(), &Limits::default()) {
        Err(QueryError::Plan(m)) => m,
        other => panic!("expected a Plan error, got {other:?}"),
    }
}

#[test]
fn depth_counts_paths_through_rule_bodies() {
    let (w, e) = world();
    // Fixpoint (1) + chain (MAX - 1) = MAX: not too deep (the item-1 error
    // is "not implemented", never "too deep").
    let ok = plan_msg(&w, fixpoint_with_chain(e, MAX_PLAN_DEPTH - 1));
    assert!(!ok.contains("too deep") && ok.contains("Fixpoint"), "{ok}");
    // One more operator on the rule's path: too deep.
    let deep = plan_msg(&w, fixpoint_with_chain(e, MAX_PLAN_DEPTH));
    assert!(deep.contains("too deep"), "{deep}");
}

#[test]
fn depth_counts_paths_through_not_bodies_and_right_sides() {
    let (w, e) = world();
    let chain = |nodes: usize| {
        let mut op = leaf_expand(e);
        for _ in 0..nodes - 2 {
            op = expand_on(op, tr(e), "parent_of", tv("c"), at(0), Access::E);
        }
        op
    };
    type Mk = fn(Op) -> Op;
    let mks: [(&str, Mk); 3] = [
        ("Not", |c| Op::Not {
            input: Box::new(Op::Unit),
            body: Box::new(c),
        }),
        ("Difference", |c| Op::Difference {
            left: Box::new(Op::Unit),
            right: Box::new(c),
        }),
        ("Intersect", |c| Op::Intersect {
            left: Box::new(Op::Unit),
            right: Box::new(c),
        }),
    ];
    for (name, mk) in mks {
        let ok = plan_msg(&w, mk(chain(MAX_PLAN_DEPTH - 1)));
        assert!(
            !ok.contains("too deep") && ok.contains(name),
            "{name}: {ok}"
        );
        let deep = plan_msg(&w, mk(chain(MAX_PLAN_DEPTH)));
        assert!(deep.contains("too deep"), "{name}: {deep}");
    }
}

// ------------------------------------- unimplemented in item 1

#[test]
fn run_of_each_new_op_is_a_plan_error_naming_the_op() {
    let (w, _) = world();
    for (name, op) in all_new_ops() {
        // Item 4 implemented Not, Difference and Intersect; they run now (see
        // set_ops.rs and the Datalog negation tests); item 5 implemented
        // History (see the Datalog history_patterns tests); item 7 makes
        // Aggregate run (see aggregate_ir.rs); item 6
        // implemented During (see interval_queries.rs). Apply stays in the
        // loop: with unbound arguments it is still a plan error naming it.
        if [
            "Not",
            "Difference",
            "Intersect",
            "History",
            "During",
            "Aggregate",
        ]
        .contains(&name)
        {
            continue;
        }
        let m = plan_msg(&w, op.clone());
        assert!(m.contains(name), "{name}: {m}");
        // Also when buried under supported operators.
        let wrapped = project(join(Op::Unit, op), &["c"]);
        let m = plan_msg(&w, wrapped);
        assert!(m.contains(name), "wrapped {name}: {m}");
    }
}
