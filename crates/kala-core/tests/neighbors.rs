//! Slice 2, item 6: Neighbors at a tick (docs/slice-2-decisions.md, "Item 6").
//!
//! API under test (new):
//!   `pub enum Direction { Out, In, Both }`           (root re-export, module `world`)
//!   `World::neighbors(&self, entity: EntityId, attr: &str, dir: Direction, t: Tick)
//!        -> Result<Vec<(EntityId, EdgeId)>, Error>`
//!   `View::neighbors(...)` same, at the pinned canon.
//!   `Error::NotARef(String)` (the attribute name) for a non-ref attribute.
//! Out: entity is the source, returns (target, edge). In: entity is the target,
//! returns (source, edge). Both = union. Sorted ascending by (neighbor, edge),
//! no duplicates. Directed attributes only (bidirectional is item 7).

use kala_core::*;
use proptest::prelude::*;

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(a("bought", ValueType::Ref, Kind::Event, Cardinality::Many).directed())
        .attribute(a("trusts", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("witness", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::Many))
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

struct Fx {
    w: World,
    greg: EntityId,
    edwin: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    Fx {
        w,
        greg,
        edwin,
        mara,
        varrow,
        ashford,
    }
}

fn edge(w: &World, s: EntityId, a: &str, t: EntityId) -> EdgeId {
    w.edge_id(s, a, t).expect("edge interned")
}

fn who(v: &[(EntityId, EdgeId)]) -> Vec<EntityId> {
    v.iter().map(|x| x.0).collect()
}

// ---- who is in Varrow at T ----

#[test]
fn who_is_in_varrow_follows_assert_retract_assert() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), tk(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-200))
        .unwrap();
    let e = edge(&f.w, f.greg, "located_in", f.varrow);
    let at = |t: i64| {
        f.w.neighbors(f.varrow, "located_in", Direction::In, Tick(t))
            .unwrap()
    };
    for present in [-300, -260, -251, -200, -150, i64::MAX] {
        assert_eq!(at(present), vec![(f.greg, e)], "present at {present}");
    }
    for absent in [-301, -250, -249, -220, -201, i64::MIN] {
        assert_eq!(at(absent), vec![], "absent at {absent}");
    }
}

#[test]
fn several_sources_into_one_target_are_sorted_by_neighbor() {
    let mut f = fx();
    // write in reverse of ID order
    f.w.assert(f.mara, "located_in", Value::Ref(f.varrow), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), tk(0))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(0))
        .unwrap();
    let got =
        f.w.neighbors(f.varrow, "located_in", Direction::In, Tick(0))
            .unwrap();
    assert_eq!(who(&got), vec![f.greg, f.edwin, f.mara]);
    for (n, e) in got {
        assert_eq!(e, edge(&f.w, n, "located_in", f.varrow));
    }
}

#[test]
fn several_targets_from_one_source_out() {
    let mut f = fx();
    f.w.assert(f.greg, "trusts", Value::Ref(f.mara), tk(1))
        .unwrap();
    f.w.assert(f.greg, "trusts", Value::Ref(f.edwin), tk(1))
        .unwrap();
    let got =
        f.w.neighbors(f.greg, "trusts", Direction::Out, Tick(1))
            .unwrap();
    assert_eq!(who(&got), vec![f.edwin, f.mara]);
    assert!(
        f.w.neighbors(f.greg, "trusts", Direction::Out, Tick(0))
            .unwrap()
            .is_empty()
    );
    // targets see greg via In, not Out
    assert!(
        f.w.neighbors(f.mara, "trusts", Direction::Out, Tick(1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        who(&f
            .w
            .neighbors(f.mara, "trusts", Direction::In, Tick(1))
            .unwrap()),
        vec![f.greg]
    );
}

#[test]
fn out_equals_as_of_for_ref_attributes() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), tk(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", Value::Ref(f.ashford), tk(-250))
        .unwrap();
    f.w.assert(f.greg, "trusts", Value::Ref(f.mara), tk(-100))
        .unwrap();
    for attr in ["located_in", "trusts"] {
        for t in [i64::MIN, -301, -300, -251, -250, -100, 0, i64::MAX] {
            let out =
                f.w.neighbors(f.greg, attr, Direction::Out, Tick(t))
                    .unwrap();
            let as_of = f.w.as_of(f.greg, attr, Tick(t)).unwrap();
            let mut expect: Vec<(EntityId, EdgeId)> = as_of
                .iter()
                .map(|v| match v {
                    Value::Ref(x) => (*x, edge(&f.w, f.greg, attr, *x)),
                    other => panic!("non-ref {other:?}"),
                })
                .collect();
            expect.sort();
            assert_eq!(out, expect, "{attr} @ {t}");
        }
    }
}

// ---- Both ----

#[test]
fn both_is_the_sorted_union_without_duplicates() {
    let mut f = fx();
    f.w.assert(f.greg, "trusts", Value::Ref(f.edwin), tk(0))
        .unwrap();
    f.w.assert(f.mara, "trusts", Value::Ref(f.greg), tk(0))
        .unwrap();
    f.w.assert(f.greg, "trusts", Value::Ref(f.mara), tk(0))
        .unwrap();
    let out =
        f.w.neighbors(f.greg, "trusts", Direction::Out, Tick(0))
            .unwrap();
    let inn =
        f.w.neighbors(f.greg, "trusts", Direction::In, Tick(0))
            .unwrap();
    let both =
        f.w.neighbors(f.greg, "trusts", Direction::Both, Tick(0))
            .unwrap();
    let mut expect: Vec<_> = out.iter().chain(inn.iter()).copied().collect();
    expect.sort();
    expect.dedup();
    assert_eq!(both, expect);
    assert_eq!(
        both.len(),
        3,
        "mara appears twice, with two different edges"
    );
}

#[test]
fn mutual_directed_edges_are_two_edges_under_both() {
    let mut f = fx();
    f.w.assert(f.edwin, "trusts", Value::Ref(f.mara), tk(0))
        .unwrap();
    f.w.assert(f.mara, "trusts", Value::Ref(f.edwin), tk(0))
        .unwrap();
    let e1 = edge(&f.w, f.edwin, "trusts", f.mara);
    let e2 = edge(&f.w, f.mara, "trusts", f.edwin);
    assert_ne!(e1, e2);
    let both =
        f.w.neighbors(f.edwin, "trusts", Direction::Both, Tick(0))
            .unwrap();
    assert_eq!(both, vec![(f.mara, e1.min(e2)), (f.mara, e1.max(e2))]);
    assert_eq!(
        f.w.neighbors(f.edwin, "trusts", Direction::Out, Tick(0))
            .unwrap(),
        vec![(f.mara, e1)]
    );
    assert_eq!(
        f.w.neighbors(f.edwin, "trusts", Direction::In, Tick(0))
            .unwrap(),
        vec![(f.mara, e2)]
    );
}

#[test]
fn self_edge_appears_once_under_both() {
    let mut f = fx();
    f.w.assert(f.greg, "trusts", Value::Ref(f.greg), tk(0))
        .unwrap();
    let e = edge(&f.w, f.greg, "trusts", f.greg);
    for d in [Direction::Out, Direction::In, Direction::Both] {
        assert_eq!(
            f.w.neighbors(f.greg, "trusts", d, Tick(0)).unwrap(),
            vec![(f.greg, e)],
            "{d:?}"
        );
    }
}

// ---- inverse ----

#[test]
fn inverse_name_reads_the_same_edges_flipped() {
    let mut f = fx();
    let kid2 = f.w.create_entity("CHARACTER", Some("kid2")).unwrap();
    f.w.assert(f.mara, "parent_of", Value::Ref(f.greg), tk(-10))
        .unwrap();
    f.w.assert(f.mara, "parent_of", Value::Ref(kid2), tk(-5))
        .unwrap();
    f.w.assert(f.edwin, "parent_of", Value::Ref(f.mara), tk(-100))
        .unwrap();
    f.w.retract(f.mara, "parent_of", Value::Ref(f.greg), tk(5))
        .unwrap();
    for x in [f.mara, f.greg, kid2, f.edwin] {
        for t in [i64::MIN, -101, -100, -10, -6, -5, 4, 5, 6, i64::MAX] {
            let n = |a: &str, d| f.w.neighbors(x, a, d, Tick(t)).unwrap();
            assert_eq!(n("child_of", Direction::Out), n("parent_of", Direction::In));
            assert_eq!(n("child_of", Direction::In), n("parent_of", Direction::Out));
            assert_eq!(
                n("child_of", Direction::Both),
                n("parent_of", Direction::Both)
            );
        }
    }
    let kids =
        f.w.neighbors(f.mara, "parent_of", Direction::Out, Tick(0))
            .unwrap();
    // greg: assert -10, retract only @5 -> still a kid at 0; kid2 asserted @-5.
    assert_eq!(who(&kids), vec![f.greg, kid2]);
    let parents =
        f.w.neighbors(f.greg, "child_of", Direction::Out, Tick(0))
            .unwrap();
    assert_eq!(
        who(&parents),
        vec![f.mara],
        "parent_of holds at 0 (retracted at 5)"
    );
    let later =
        f.w.neighbors(f.greg, "child_of", Direction::Out, Tick(5))
            .unwrap();
    assert!(later.is_empty());
}

// ---- events ----

#[test]
fn event_ref_holds_from_its_tick_on_forever() {
    let mut f = fx();
    f.w.assert(f.edwin, "bought", Value::Ref(f.ashford), tk(-208))
        .unwrap();
    let e = edge(&f.w, f.edwin, "bought", f.ashford);
    for t in [-208, -207, 0, i64::MAX] {
        assert_eq!(
            f.w.neighbors(f.edwin, "bought", Direction::Out, Tick(t))
                .unwrap(),
            vec![(f.ashford, e)]
        );
        assert_eq!(
            f.w.neighbors(f.ashford, "bought", Direction::In, Tick(t))
                .unwrap(),
            vec![(f.edwin, e)]
        );
    }
    for t in [-209, i64::MIN] {
        assert!(
            f.w.neighbors(f.edwin, "bought", Direction::Out, Tick(t))
                .unwrap()
                .is_empty()
        );
    }
}

// ---- edges as subjects and targets ----

#[test]
fn neighbors_of_an_edge_through_a_ref_valued_edge_property() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-300))
        .unwrap();
    let ge = edge(&f.w, f.greg, "located_in", f.varrow);
    let ee: EntityId = ge.into();
    // property on the edge whose value is another entity
    f.w.assert(ee, "witness", Value::Ref(f.mara), tk(-300))
        .unwrap();
    let pe = edge(&f.w, ee, "witness", f.mara);
    assert_eq!(
        f.w.neighbors(ee, "witness", Direction::Out, Tick(-300))
            .unwrap(),
        vec![(f.mara, pe)]
    );
    assert!(
        f.w.neighbors(ee, "witness", Direction::Out, Tick(-301))
            .unwrap()
            .is_empty()
    );
    // the edge is a subject: mara sees it under In
    assert_eq!(
        f.w.neighbors(f.mara, "witness", Direction::In, Tick(-300))
            .unwrap(),
        vec![(ee, pe)]
    );
}

#[test]
fn neighbors_in_of_an_edge_that_is_a_ref_target() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-300))
        .unwrap();
    let ee: EntityId = edge(&f.w, f.greg, "located_in", f.varrow).into();
    f.w.assert(f.mara, "witness", Value::Ref(ee), tk(-290))
        .unwrap();
    f.w.assert(f.edwin, "witness", Value::Ref(ee), tk(-280))
        .unwrap();
    let we = edge(&f.w, f.mara, "witness", ee);
    let wd = edge(&f.w, f.edwin, "witness", ee);
    assert_eq!(
        f.w.neighbors(ee, "witness", Direction::In, Tick(-290))
            .unwrap(),
        vec![(f.mara, we)]
    );
    let both =
        f.w.neighbors(ee, "witness", Direction::In, Tick(-280))
            .unwrap();
    assert_eq!(who(&both), vec![f.edwin, f.mara]);
    assert_eq!(both, vec![(f.edwin, wd), (f.mara, we)]);
    assert_eq!(
        f.w.neighbors(f.mara, "witness", Direction::Out, Tick(-290))
            .unwrap(),
        vec![(ee, we)]
    );
}

// ---- canon ----

#[test]
fn pinned_canon_does_not_see_later_prequel_or_retract() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-200))
        .unwrap();
    let c1 = f.w.latest_tx();
    // prequel written later
    f.w.assert(f.edwin, "located_in", Value::Ref(f.varrow), tk(-500))
        .unwrap();
    let c2 = f.w.latest_tx();
    // retract written later, ending greg's stay
    f.w.retract(f.greg, "located_in", Value::Ref(f.varrow), tk(-100))
        .unwrap();
    let c3 = f.w.latest_tx();
    let ge = edge(&f.w, f.greg, "located_in", f.varrow);
    let ee = edge(&f.w, f.edwin, "located_in", f.varrow);

    let n = |c: TxId, t: i64, d| {
        f.w.at_canon(c)
            .neighbors(f.varrow, "located_in", d, Tick(t))
            .unwrap()
    };
    assert_eq!(n(c1, -500, Direction::In), vec![]);
    assert_eq!(n(c1, 0, Direction::In), vec![(f.greg, ge)]);
    assert_eq!(n(c2, -500, Direction::In), vec![(f.edwin, ee)]);
    assert_eq!(n(c2, 0, Direction::In), {
        let mut v = vec![(f.greg, ge), (f.edwin, ee)];
        v.sort();
        v
    });
    assert_eq!(n(c3, 0, Direction::In), vec![(f.edwin, ee)]);
    assert_eq!(n(c3, -150, Direction::In).len(), 2);
    // View Out agrees at the same canon
    assert_eq!(
        f.w.at_canon(c1)
            .neighbors(f.greg, "located_in", Direction::Out, Tick(0))
            .unwrap(),
        vec![(f.varrow, ge)]
    );
    assert_eq!(
        f.w.at_canon(c3)
            .neighbors(f.greg, "located_in", Direction::Out, Tick(0))
            .unwrap(),
        vec![]
    );
    // world.neighbors == latest-canon view
    assert_eq!(
        f.w.neighbors(f.varrow, "located_in", Direction::In, Tick(0))
            .unwrap(),
        n(c3, 0, Direction::In)
    );
}

#[test]
fn canon_zero_has_no_entities_so_reads_are_unknown_entity() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(0))
        .unwrap();
    let v = f.w.at_canon(TxId(0));
    for d in [Direction::Out, Direction::In, Direction::Both] {
        // neither entity is defined at canon 0
        assert_eq!(
            v.neighbors(f.greg, "located_in", d, Tick(0)),
            Err(Error::UnknownEntity(f.greg))
        );
        assert_eq!(
            v.neighbors(f.varrow, "located_in", d, Tick(0)),
            Err(Error::UnknownEntity(f.varrow))
        );
    }
}

// ---- reopen ----

#[test]
fn answers_are_identical_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut before = Vec::new();
    let (greg, varrow, mara);
    {
        let mut w = World::create(&p, schema()).unwrap();
        greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
        mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
        varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
        w.assert(greg, "located_in", Value::Ref(varrow), tk(-300))
            .unwrap();
        w.retract(greg, "located_in", Value::Ref(varrow), tk(-250))
            .unwrap();
        w.assert(greg, "located_in", Value::Ref(varrow), tk(-200))
            .unwrap();
        w.assert(mara, "parent_of", Value::Ref(greg), tk(-400))
            .unwrap();
        for (x, a) in [
            (varrow, "located_in"),
            (greg, "located_in"),
            (greg, "child_of"),
            (mara, "parent_of"),
        ] {
            for d in [Direction::Out, Direction::In, Direction::Both] {
                for t in [-401, -400, -300, -250, -220, -200, 0] {
                    before.push(((x, a, t), w.neighbors(x, a, d, Tick(t)).unwrap()));
                }
            }
        }
        assert!(before.iter().any(|(_, v)| !v.is_empty()));
    }
    let w = World::open(&p).unwrap();
    let mut i = 0;
    for (x, a) in [
        (varrow, "located_in"),
        (greg, "located_in"),
        (greg, "child_of"),
        (mara, "parent_of"),
    ] {
        for d in [Direction::Out, Direction::In, Direction::Both] {
            for t in [-401, -400, -300, -250, -220, -200, 0] {
                assert_eq!(
                    before[i].1,
                    w.neighbors(x, a, d, Tick(t)).unwrap(),
                    "{:?}",
                    before[i].0
                );
                i += 1;
            }
        }
    }
}

// ---- errors ----

#[test]
fn unknown_attribute_is_an_error() {
    let f = fx();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        assert!(matches!(
            f.w.neighbors(f.greg, "nope", d, Tick(0)),
            Err(Error::UnknownAttribute(a)) if a == "nope"
        ));
        assert!(matches!(
            f.w.at_canon(TxId(0)).neighbors(f.greg, "nope", d, Tick(0)),
            Err(Error::UnknownAttribute(_))
        ));
    }
}

#[test]
fn non_ref_attribute_is_not_a_ref() {
    let f = fx();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        assert!(matches!(
            f.w.neighbors(f.greg, "age", d, Tick(0)),
            Err(Error::NotARef(a)) if a == "age"
        ));
    }
}

#[test]
fn undefined_subject_entity_or_edge_is_unknown_entity() {
    let f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 99).unwrap();
    let ghost_edge: EntityId = EdgeId::new(99).unwrap().into();
    for bad in [ghost, ghost_edge] {
        for d in [Direction::Out, Direction::In, Direction::Both] {
            assert!(
                matches!(
                    f.w.neighbors(bad, "trusts", d, Tick(0)),
                    Err(Error::UnknownEntity(e)) if e == bad
                ),
                "{bad:?} {d:?}"
            );
        }
    }
}

#[test]
fn defined_entity_with_no_edges_is_empty_not_an_error() {
    let f = fx();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        assert_eq!(f.w.neighbors(f.greg, "trusts", d, Tick(0)).unwrap(), vec![]);
    }
    let _ = f.mara;
}

// ---- property test vs. a naive model ----

#[derive(Clone, Debug)]
struct W {
    src: usize,
    attr: usize, // 0 located_in, 1 trusts, 2 bought
    tgt: usize,
    tick: i64,
    retract: bool,
}

fn w_strategy() -> impl Strategy<Value = W> {
    (
        0usize..4,
        0usize..3,
        0usize..4,
        prop_oneof![Just(i64::MIN), Just(i64::MAX), Just(0), -5i64..5],
        any::<bool>(),
    )
        .prop_map(|(src, attr, tgt, tick, retract)| W {
            src,
            attr,
            tgt,
            tick,
            retract,
        })
}

const NAMES: [&str; 3] = ["located_in", "trusts", "bought"];

type Fact = (EntityId, usize, EntityId, i64, u64, Op);

/// Latest op with tick <= t per (src, attr, tgt), among facts with tx <= canon.
fn model(
    facts: &[Fact],
    canon: u64,
    subject: EntityId,
    attr: usize,
    dir: Direction,
    t: i64,
    edges: &dyn Fn(EntityId, usize, EntityId) -> EdgeId,
) -> Vec<(EntityId, EdgeId)> {
    let mut latest: std::collections::BTreeMap<(EntityId, EntityId), (i64, Op)> =
        Default::default();
    for &(s, a, g, tick, tx, op) in facts {
        if a != attr || tx > canon || tick > t {
            continue;
        }
        let slot = latest.entry((s, g)).or_insert((tick, op));
        if tick >= slot.0 {
            *slot = (tick, op);
        }
    }
    let mut out = Vec::new();
    for ((s, g), (_, op)) in latest {
        if op != Op::Assert {
            continue;
        }
        let e = edges(s, attr, g);
        if matches!(dir, Direction::Out | Direction::Both) && s == subject {
            out.push((g, e));
        }
        if matches!(dir, Direction::In | Direction::Both) && g == subject {
            out.push((s, e));
        }
    }
    out.sort();
    out.dedup();
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn neighbors_match_the_naive_model_at_every_canon(
        writes in prop::collection::vec(w_strategy(), 1..25)
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let ents: Vec<EntityId> = (0..4)
            .map(|i| w.create_entity("CHARACTER", Some(&format!("c{i}"))).unwrap())
            .collect();
        let setup_tx = w.latest_tx();
        let mut facts: Vec<Fact> = Vec::new();
        let mut ticks = std::collections::BTreeSet::new();
        for x in &writes {
            let (s, g) = (ents[x.src], ents[x.tgt]);
            let retract = x.retract && x.attr != 2;
            let r = if retract {
                w.retract(s, NAMES[x.attr], Value::Ref(g), tk(x.tick))
            } else {
                w.assert(s, NAMES[x.attr], Value::Ref(g), tk(x.tick))
            };
            if let Ok(r) = r
                && r.changed == 1
            {
                facts.push((s, x.attr, g, x.tick, w.latest_tx().0,
                    if retract { Op::Retract } else { Op::Assert }));
                for d in [-1i64, 0, 1] {
                    ticks.insert(x.tick.saturating_add(d));
                }
            }
        }
        ticks.insert(i64::MIN);
        ticks.insert(i64::MAX);
        ticks.insert(0);
        let edges = |s: EntityId, a: usize, g: EntityId| w.edge_id(s, NAMES[a], g).unwrap();
        for canon in setup_tx.0..=w.latest_tx().0 {
            let view = w.at_canon(TxId(canon));
            for &subject in &ents {
                for (attr, name) in NAMES.iter().enumerate() {
                    for dir in [Direction::Out, Direction::In, Direction::Both] {
                        for &t in &ticks {
                            let exp = model(&facts, canon, subject, attr, dir, t, &edges);
                            let got = view
                                .neighbors(subject, name, dir, Tick(t))
                                .unwrap();
                            prop_assert_eq!(&got, &exp,
                                "canon {} {:?} {:?} {} @ {}", canon, subject, dir, name, t);
                            if canon == w.latest_tx().0 {
                                prop_assert_eq!(
                                    w.neighbors(subject, name, dir, Tick(t)).unwrap(), exp);
                            }
                        }
                    }
                }
            }
        }
    }
}

// ---- item 6 confirm: edge cases ----

#[test]
fn edge_exists_but_all_its_facts_are_newer_than_the_canon_is_empty() {
    let mut f = fx();
    let before = f.w.latest_tx();
    f.w.assert(f.greg, "located_in", Value::Ref(f.varrow), tk(-300))
        .unwrap();
    f.w.assert(f.greg, "trusts", Value::Ref(f.mara), tk(-300))
        .unwrap();
    // the edges are interned in the world now
    assert!(f.w.edge_id(f.greg, "located_in", f.varrow).is_some());
    let v = f.w.at_canon(before);
    for t in [i64::MIN, -300, 0, i64::MAX] {
        for d in [Direction::Out, Direction::In, Direction::Both] {
            for (x, a) in [
                (f.greg, "located_in"),
                (f.varrow, "located_in"),
                (f.mara, "trusts"),
                (f.greg, "trusts"),
            ] {
                assert_eq!(
                    v.neighbors(x, a, d, Tick(t)).unwrap(),
                    vec![],
                    "{x:?} {a} {d:?} @ {t}"
                );
            }
        }
    }
}

#[test]
fn inverse_name_with_undefined_subject_is_unknown_entity() {
    let f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 99).unwrap();
    let ghost_edge: EntityId = EdgeId::new(99).unwrap().into();
    for bad in [ghost, ghost_edge] {
        for d in [Direction::Out, Direction::In, Direction::Both] {
            assert!(matches!(
                f.w.neighbors(bad, "child_of", d, Tick(0)),
                Err(Error::UnknownEntity(e)) if e == bad
            ));
            assert!(matches!(
                f.w.at_canon(TxId(0)).neighbors(bad, "child_of", d, Tick(0)),
                Err(Error::UnknownEntity(e)) if e == bad
            ));
        }
    }
}

#[test]
fn error_precedence_is_name_then_subject_then_ref_check() {
    let f = fx();
    let ghost = EntityId::new(NodeTypeId(0), 99).unwrap();
    for d in [Direction::Out, Direction::In, Direction::Both] {
        // undefined subject + non-ref attribute -> UnknownEntity
        assert!(matches!(
            f.w.neighbors(ghost, "age", d, Tick(0)),
            Err(Error::UnknownEntity(e)) if e == ghost
        ));
        // unknown name beats an undefined subject
        assert!(matches!(
            f.w.neighbors(ghost, "nope", d, Tick(0)),
            Err(Error::UnknownAttribute(a)) if a == "nope"
        ));
        // defined subject + non-ref -> NotARef
        assert!(matches!(
            f.w.neighbors(f.greg, "age", d, Tick(0)),
            Err(Error::NotARef(_))
        ));
    }
}
