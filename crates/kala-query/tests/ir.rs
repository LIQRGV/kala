//! Slice 6, item 1: the language-neutral IR and `Plan::explain`
//! (docs/slice-6-decisions.md, "Item 1: IR" and "Plan printer").
//!
//! Expected public paths: `kala_query::{Var, Term, TickSource, CanonSource,
//! AttrRef, Access, CmpOp, Predicate, Op, Plan}` and
//! `Plan::explain(&self, &kala_core::World) -> String`.

use kala_core::{AttrDef, Cardinality, EntityId, Kind, Schema, Tick, Value, ValueType, World};
use kala_query::{Access, AttrRef, CanonSource, CmpOp, Op, Plan, Predicate, Term, TickSource, Var};

fn schema() -> Schema {
    Schema::new().node_type("CHARACTER").attribute(
        AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
    )
}

fn world() -> (World, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    (w, edwin)
}

fn v(n: &str) -> Var {
    Var(n.to_string())
}
fn tv(n: &str) -> Term {
    Term::Var(v(n))
}
fn tp(n: &str) -> Term {
    Term::Param(n.to_string())
}
fn b(o: Op) -> Box<Op> {
    Box::new(o)
}
fn expand(entity: Term, attr: &str, value: Term, tick: TickSource, access: Access) -> Op {
    Op::Expand {
        input: b(Op::Unit),
        entity,
        attr: AttrRef::Named(attr.to_string()),
        value,
        tick,
        access,
    }
}

fn allies_plan(edwin: EntityId, tick: TickSource) -> Plan {
    Plan {
        find: vec![v("a")],
        params: vec![],
        default_tick: tick,
        canon: CanonSource::Latest,
        root: Op::Project {
            input: b(expand(
                Term::Const(Value::Ref(edwin)),
                "ally_of",
                tv("a"),
                TickSource::Default,
                Access::E,
            )),
            vars: vec![v("a")],
        },
    }
}

#[test]
fn construct_every_type_clone_debug_eq() {
    let (_w, edwin) = world();
    let terms = [
        tv("a"),
        tp("who"),
        Term::Const(Value::I64(3)),
        Term::Const(Value::Ref(edwin)),
        Term::Any,
    ];
    for t in &terms {
        assert_eq!(t, &t.clone());
        assert!(!format!("{t:?}").is_empty());
    }
    let ticks = [
        TickSource::Default,
        TickSource::Const(Tick(-5)),
        TickSource::Param("t".into()),
        TickSource::Var(v("t")),
    ];
    for t in &ticks {
        assert_eq!(t, &t.clone());
    }
    for (i, x) in ticks.iter().enumerate() {
        for (j, y) in ticks.iter().enumerate() {
            assert_eq!(i == j, x == y);
        }
    }
    let canons = [
        CanonSource::Latest,
        CanonSource::Const(41),
        CanonSource::Param("c".into()),
    ];
    for c in &canons {
        assert_eq!(c, &c.clone());
    }
    assert_ne!(canons[0], canons[1]);
    assert_ne!(AttrRef::Named("x".into()), AttrRef::Handle);
    let accesses = [
        Access::E,
        Access::R,
        Access::V,
        Access::Scan,
        Access::Handle,
    ];
    for (i, x) in accesses.iter().enumerate() {
        for (j, y) in accesses.iter().enumerate() {
            assert_eq!(i == j, x == y);
        }
    }
    let cmps = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ];
    for c in cmps {
        let p = Predicate {
            op: c,
            left: tv("x"),
            right: Term::Const(Value::I64(1)),
        };
        assert_eq!(p, p.clone());
        assert!(!format!("{p:?}").is_empty());
    }
    // every Op variant
    let ops = vec![
        Op::Unit,
        Op::NodeScan {
            input: b(Op::Unit),
            var: v("c"),
            node_type: "CHARACTER".into(),
        },
        Op::NodeSeek {
            input: b(Op::Unit),
            var: v("e"),
            entity: tp("who"),
        },
        expand(
            tv("e"),
            "ally_of",
            tv("a"),
            TickSource::Var(v("t")),
            Access::E,
        ),
        Op::Expand {
            input: b(Op::Unit),
            entity: tv("e"),
            attr: AttrRef::Handle,
            value: Term::Any,
            tick: TickSource::Default,
            access: Access::Handle,
        },
        Op::Filter {
            input: b(Op::Unit),
            pred: Predicate {
                op: CmpOp::Lt,
                left: tv("x"),
                right: tv("y"),
            },
        },
        Op::Join {
            left: b(Op::Unit),
            right: b(Op::Unit),
        },
        Op::Project {
            input: b(Op::Unit),
            vars: vec![v("a")],
        },
        Op::Union {
            inputs: vec![Op::Unit, Op::Unit],
        },
    ];
    for o in &ops {
        assert_eq!(o, &o.clone());
        assert!(!format!("{o:?}").is_empty());
    }
    for (i, x) in ops.iter().enumerate() {
        for (j, y) in ops.iter().enumerate() {
            assert_eq!(i == j, x == y, "ops {i} and {j}");
        }
    }
}

#[test]
fn identical_plans_equal_and_tick_change_makes_them_unequal() {
    let (_w, edwin) = world();
    let p1 = allies_plan(edwin, TickSource::Const(Tick(-210)));
    let p2 = allies_plan(edwin, TickSource::Const(Tick(-210)));
    assert_eq!(p1, p2);
    assert_eq!(p1, p1.clone());
    assert_ne!(p1, allies_plan(edwin, TickSource::Const(Tick(-200))));
    assert_ne!(p1, allies_plan(edwin, TickSource::Default));
    // a changed per-Expand tick also breaks equality
    let mut p3 = p1.clone();
    if let Op::Project { input, .. } = &mut p3.root {
        if let Op::Expand { tick, .. } = input.as_mut() {
            *tick = TickSource::Const(Tick(7));
        } else {
            panic!("shape");
        }
    }
    assert_ne!(p1, p3);
}

#[test]
fn explain_edwins_allies_at_minus_210() {
    let (w, edwin) = world();
    assert_eq!(w.format_id(edwin), "CHARACTER#1");
    let p = allies_plan(edwin, TickSource::Const(Tick(-210)));
    assert_eq!(
        p.explain(&w),
        "Find ?a  at -210  canon latest\n\
         \x20 Project ?a\n\
         \x20   Expand [CHARACTER#1 :ally_of ?a] @latest via E\n\
         \x20     Unit\n"
    );
}

#[test]
fn explain_default_tick_and_const_canon() {
    let (w, edwin) = world();
    let mut p = allies_plan(edwin, TickSource::Default);
    p.canon = CanonSource::Const(41);
    assert_eq!(
        p.explain(&w),
        "Find ?a  at latest  canon 41\n\
         \x20 Project ?a\n\
         \x20   Expand [CHARACTER#1 :ally_of ?a] @latest via E\n\
         \x20     Unit\n"
    );
}

#[test]
fn explain_params_header_param_tick_param_canon() {
    let (w, _) = world();
    let p = Plan {
        find: vec![v("a"), v("b")],
        params: vec!["who".into(), "t".into()],
        default_tick: TickSource::Param("t".into()),
        canon: CanonSource::Param("c".into()),
        root: Op::Project {
            input: b(expand(
                tp("who"),
                "ally_of",
                tv("a"),
                TickSource::Param("t".into()),
                Access::E,
            )),
            vars: vec![v("a"), v("b")],
        },
    };
    assert_eq!(
        p.explain(&w),
        "Find ?a ?b  in $who $t  at $t  canon $c\n\
         \x20 Project ?a ?b\n\
         \x20   Expand [$who :ally_of ?a] @$t via E\n\
         \x20     Unit\n"
    );
}

#[test]
fn explain_every_op_access_and_cmp() {
    let (w, edwin) = world();
    let handle = Op::Expand {
        input: b(Op::Unit),
        entity: tv("e"),
        attr: AttrRef::Handle,
        value: Term::Const(Value::Str("edwin".into())),
        tick: TickSource::Default,
        access: Access::Handle,
    };
    let mut filtered = Op::Unit;
    for op in [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ] {
        filtered = Op::Filter {
            input: b(filtered),
            pred: Predicate {
                op,
                left: tv("x"),
                right: tp("y"),
            },
        };
    }
    let p = Plan {
        find: vec![v("a")],
        params: vec!["who".into()],
        default_tick: TickSource::Const(Tick(5)),
        canon: CanonSource::Latest,
        root: Op::Project {
            input: b(Op::Union {
                inputs: vec![
                    Op::Join {
                        left: b(Op::NodeScan {
                            input: b(Op::Unit),
                            var: v("a"),
                            node_type: "CHARACTER".into(),
                        }),
                        right: b(Op::NodeSeek {
                            input: b(Op::Unit),
                            var: v("e"),
                            entity: Term::Const(Value::Ref(edwin)),
                        }),
                    },
                    expand(
                        tv("e"),
                        "ally_of",
                        tv("a"),
                        TickSource::Const(Tick(-200)),
                        Access::E,
                    ),
                    expand(
                        tv("e"),
                        "ally_of",
                        tv("a"),
                        TickSource::Var(v("t")),
                        Access::R,
                    ),
                    expand(
                        tv("e"),
                        "ally_of",
                        Term::Any,
                        TickSource::Default,
                        Access::V,
                    ),
                    expand(
                        Term::Any,
                        "ally_of",
                        tv("a"),
                        TickSource::Default,
                        Access::Scan,
                    ),
                    handle,
                    filtered,
                ],
            }),
            vars: vec![v("a")],
        },
    };
    let expected = "\
Find ?a  in $who  at 5  canon latest
  Project ?a
    Union
      Join
        NodeScan ?a CHARACTER
          Unit
        NodeSeek ?e = CHARACTER#1
          Unit
      Expand [?e :ally_of ?a] @-200 via E
        Unit
      Expand [?e :ally_of ?a] @?t via R
        Unit
      Expand [?e :ally_of _] @latest via V
        Unit
      Expand [_ :ally_of ?a] @latest via scan
        Unit
      Expand [?e :handle \"edwin\"] via handle
        Unit
      Filter ?x >= $y
        Filter ?x > $y
          Filter ?x <= $y
            Filter ?x < $y
              Filter ?x != $y
                Filter ?x = $y
                  Unit
";
    assert_eq!(p.explain(&w), expected);
}

#[test]
fn explain_constant_terms_of_each_type() {
    let (w, edwin) = world();
    let f = |l: Term, r: Term, input: Op| Op::Filter {
        input: b(input),
        pred: Predicate {
            op: CmpOp::Eq,
            left: l,
            right: r,
        },
    };
    let mut root = Op::Unit;
    root = f(
        Term::Const(Value::Str("a\"b\\c\nd".into())),
        Term::Const(Value::I64(-5)),
        root,
    );
    root = f(
        Term::Const(Value::F64(1.5)),
        Term::Const(Value::Bool(true)),
        root,
    );
    root = f(
        Term::Const(Value::Bool(false)),
        Term::Const(Value::Ref(edwin)),
        root,
    );
    root = f(Term::Any, Term::Const(Value::I64(i64::MIN)), root);
    let p = Plan {
        find: vec![],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root,
    };
    let expected = "\
Find   at latest  canon latest
  Filter _ = -9223372036854775808
    Filter false = CHARACTER#1
      Filter 1.5 = true
        Filter \"a\\\"b\\\\c\\nd\" = -5
          Unit
";
    assert_eq!(p.explain(&w), expected);
}

#[test]
fn explain_string_constant_in_expand_value() {
    let (w, _) = world();
    let p = Plan {
        find: vec![v("e")],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: expand(
            tv("e"),
            "name",
            Term::Const(Value::Str("it's \"x\"".into())),
            TickSource::Default,
            Access::V,
        ),
    };
    let out = p.explain(&w);
    assert!(out.ends_with("\n"));
    assert_eq!(
        out.lines().nth(1).unwrap(),
        "  Expand [?e :name \"it's \\\"x\\\"\"] @latest via V"
    );
}

#[test]
fn explain_does_not_panic_on_undefined_ref() {
    let (w, edwin) = world();
    let ghost = EntityId::new(edwin.node_type(), 999).unwrap();
    let p = allies_plan(ghost, TickSource::Default);
    let out = p.explain(&w);
    assert!(!out.is_empty());
    let f = Plan {
        root: Op::NodeSeek {
            input: b(Op::Unit),
            var: v("e"),
            entity: Term::Const(Value::Ref(ghost)),
        },
        ..p
    };
    assert!(!f.explain(&w).is_empty());
}

/// Prints a single-constant plan and returns the Filter line carrying it.
fn print_const(w: &World, c: Value) -> String {
    let p = Plan {
        find: vec![],
        params: vec![],
        default_tick: TickSource::Default,
        canon: CanonSource::Latest,
        root: Op::Filter {
            input: b(Op::Unit),
            pred: Predicate {
                op: CmpOp::Eq,
                left: Term::Const(c),
                right: Term::Any,
            },
        },
    };
    let out = p.explain(w);
    let mut lines = out.lines();
    lines.next().unwrap();
    lines.next().unwrap().to_string()
}

#[test]
fn explain_f64_constants() {
    let (w, _) = world();
    assert_eq!(print_const(&w, Value::F64(0.0)), "  Filter 0.0 = _");
    assert_eq!(print_const(&w, Value::F64(1e300)), "  Filter 1e300 = _");
    assert_eq!(print_const(&w, Value::F64(-2.5)), "  Filter -2.5 = _");
}

#[test]
fn explain_edge_ref_prints_edge_id() {
    use kala_core::EdgeId;
    let (w, _) = world();
    let e = EntityId::from(EdgeId::new(7).unwrap());
    assert_eq!(print_const(&w, Value::Ref(e)), "  Filter edge#7 = _");
}

#[test]
fn explain_string_constant_with_invisible_and_combining_chars() {
    let (w, _) = world();
    // Finding if this fails: U+202E (bidi override) printed raw would let a
    // string constant visually reorder the plan text. The printer must escape it.
    let rlo = print_const(&w, Value::Str("a\u{202E}b".into()));
    assert!(
        !rlo.contains('\u{202E}'),
        "bidi control printed raw: {rlo:?}"
    );
    // Zero-width space is invisible; printing it raw hides content in the plan.
    let zw = print_const(&w, Value::Str("a\u{200B}b".into()));
    assert!(
        !zw.contains('\u{200B}'),
        "zero-width space printed raw: {zw:?}"
    );
    // Combining mark: pin that it is distinguishable from the precomposed char
    // (the printer must not normalize), whichever way it is escaped.
    let nfd = print_const(&w, Value::Str("e\u{0301}".into()));
    let nfc = print_const(&w, Value::Str("\u{00E9}".into()));
    assert_ne!(
        nfd, nfc,
        "decomposed and precomposed look identical in the plan"
    );
}

#[test]
fn explain_pins_escaped_forms_of_special_chars() {
    let (w, _) = world();
    assert_eq!(
        print_const(&w, Value::Str("a\u{202E}b".into())),
        "  Filter \"a\\u{202e}b\" = _"
    );
    assert_eq!(
        print_const(&w, Value::Str("a\u{200B}b".into())),
        "  Filter \"a\\u{200b}b\" = _"
    );
    // a combining mark is escaped too, so NFD and NFC stay distinguishable
    assert_eq!(
        print_const(&w, Value::Str("e\u{0301}".into())),
        "  Filter \"e\\u{301}\" = _"
    );
}
