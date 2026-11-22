//! Slice 2, item 7: bidirectional attributes (docs/slice-2-decisions.md,
//! "Item 7" and the bidirectional parts of Items 3 and 5).
//!
//! Contract under test (no new API, new behaviour):
//! - A write on a bidirectional attribute normalises `(a, b)` to
//!   `(min, max)` by EntityId before no-op / same-tick judgement and
//!   storage: both spellings are one edge and one history.
//!   `edge_id(a, attr, b) == edge_id(b, attr, a)`, `edge(id)` returns
//!   `(min, attr, max)`.
//! - `SameTickConflict.entity` is the entity as the caller wrote it.
//! - One E row (entity = min, value = Ref(max)) and one R row
//!   (target = max, source = min) per stored write.
//! - Reads agree from both sides: `neighbors` (Out, In and Both all return
//!   the partners from either side), `as_of`, `as_of_value`, `history`
//!   (values are `Ref(partner)`).
//! - Replay is Corrupt for a bidirectional DefineEdge with source > target
//!   and for a bidirectional Fact stored with entity > value.
//! - Directed attributes are never normalised.
//!
//! Entity ID order used throughout, by creation order inside CHARACTER:
//! edwin (seq 1) < mara (2) < lisa (3) < greg (4).
//! Self-alliance: history of a self-edge is unspecified (E and R rows
//! coincide) and is deliberately not asserted.
mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use proptest::prelude::*;
use std::path::{Path, PathBuf};

fn schema() -> Schema {
    let a = AttrDef::new;
    Schema::new()
        .node_type("CHARACTER")
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(a("trusts", ValueType::Ref, Kind::State, Cardinality::Many).directed())
        .attribute(a(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

#[derive(Clone, Copy)]
struct Cast {
    edwin: EntityId,
    mara: EntityId,
    lisa: EntityId,
    greg: EntityId,
}

fn cast(w: &mut World) -> Cast {
    let c = Cast {
        edwin: w.create_entity("CHARACTER", Some("edwin")).unwrap(),
        mara: w.create_entity("CHARACTER", Some("mara")).unwrap(),
        lisa: w.create_entity("CHARACTER", Some("lisa")).unwrap(),
        greg: w.create_entity("CHARACTER", Some("greg")).unwrap(),
    };
    assert!(c.edwin < c.mara && c.mara < c.lisa && c.lisa < c.greg);
    c
}

fn fx() -> (World, Cast) {
    let mut w = World::in_memory(schema()).unwrap();
    let c = cast(&mut w);
    (w, c)
}

fn file_fx() -> (tempfile::TempDir, PathBuf, World, Cast) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let c = cast(&mut w);
    (dir, p, w, c)
}

fn ally(w: &mut World, a: EntityId, b: EntityId, t: i64) -> WriteResult {
    w.assert(a, "ally_of", Value::Ref(b), tk(t)).unwrap()
}

fn unally(w: &mut World, a: EntityId, b: EntityId, t: i64) -> WriteResult {
    w.retract(a, "ally_of", Value::Ref(b), tk(t)).unwrap()
}

const DIRS: [Direction; 3] = [Direction::Out, Direction::In, Direction::Both];

// ---------------------------------------------------------------------------
// One edge, either spelling
// ---------------------------------------------------------------------------

#[test]
fn second_spelling_at_a_later_tick_is_changed_0_and_one_edge() {
    let (mut w, c) = fx();
    assert_eq!(ally(&mut w, c.edwin, c.mara, 10).changed, 1);
    let tx = w.latest_tx();
    let r = ally(&mut w, c.mara, c.edwin, 20);
    assert_eq!(
        r.changed, 0,
        "Mara ally_of Edwin while Edwin ally_of Mara holds"
    );
    assert_eq!(r.tick, Tick(20));
    assert_eq!(w.latest_tx(), tx, "a no-op appends nothing");
    let e = w.edge_id(c.edwin, "ally_of", c.mara).expect("edge");
    assert_eq!(w.edge_id(c.mara, "ally_of", c.edwin), Some(e));
    assert_eq!(e.seq(), 1, "exactly one edge interned");
    assert_eq!(w.edge(e), Some((c.edwin, "ally_of".to_string(), c.mara)));
    assert!(w.edge(EdgeId::new(2).unwrap()).is_none());
}

#[test]
fn writing_first_with_the_larger_id_stores_the_pair_normalised() {
    let (mut w, c) = fx();
    assert_eq!(ally(&mut w, c.mara, c.edwin, 10).changed, 1);
    let e = w.edge_id(c.mara, "ally_of", c.edwin).expect("edge");
    assert_eq!(w.edge_id(c.edwin, "ally_of", c.mara), Some(e));
    let (s, a, t) = w.edge(e).unwrap();
    assert_eq!((s, a.as_str(), t), (c.edwin, "ally_of", c.mara));
    assert!(s < t, "edge(id).0 is the smaller id");
    // and the other spelling is a no-op
    assert_eq!(ally(&mut w, c.edwin, c.mara, 11).changed, 0);
}

#[test]
fn retracting_either_spelling_ends_the_alliance() {
    for retract_with_larger_first in [false, true] {
        for assert_with_larger_first in [false, true] {
            let (mut w, c) = fx();
            let (a1, a2) = if assert_with_larger_first {
                (c.mara, c.edwin)
            } else {
                (c.edwin, c.mara)
            };
            let (r1, r2) = if retract_with_larger_first {
                (c.mara, c.edwin)
            } else {
                (c.edwin, c.mara)
            };
            ally(&mut w, a1, a2, 1);
            assert_eq!(unally(&mut w, r1, r2, 5).changed, 1);
            let e = w.edge_id(c.edwin, "ally_of", c.mara).unwrap();
            for x in [c.edwin, c.mara] {
                let other = if x == c.edwin { c.mara } else { c.edwin };
                for (t, present) in [
                    (i64::MIN, false),
                    (0, false),
                    (1, true),
                    (4, true),
                    (5, false),
                    (i64::MAX, false),
                ] {
                    for d in DIRS {
                        let want = if present { vec![(other, e)] } else { vec![] };
                        assert_eq!(
                            w.neighbors(x, "ally_of", d, Tick(t)).unwrap(),
                            want,
                            "{x:?} {d:?} @ {t} (assert larger first: {assert_with_larger_first}, retract larger first: {retract_with_larger_first})"
                        );
                    }
                    assert_eq!(
                        w.as_of(x, "ally_of", Tick(t)).unwrap(),
                        if present {
                            vec![Value::Ref(other)]
                        } else {
                            vec![]
                        }
                    );
                    assert_eq!(
                        w.as_of_value(x, "ally_of", &Value::Ref(other), Tick(t))
                            .unwrap(),
                        present
                    );
                }
            }
            // a second retract by either spelling is a no-op now
            assert_eq!(unally(&mut w, c.mara, c.edwin, 6).changed, 0);
            assert_eq!(unally(&mut w, c.edwin, c.mara, 6).changed, 0);
            // re-assert through the other spelling keeps the edge
            assert_eq!(ally(&mut w, a2, a1, 9).changed, 1);
            assert_eq!(w.edge_id(a1, "ally_of", a2), Some(e));
            assert_eq!(
                w.neighbors(c.mara, "ally_of", Direction::Both, Tick(9))
                    .unwrap(),
                vec![(c.edwin, e)]
            );
        }
    }
}

#[test]
fn reads_agree_on_both_sides_whatever_spelling_was_written() {
    for larger_first in [false, true] {
        let (mut w, c) = fx();
        let (a, b) = if larger_first {
            (c.mara, c.edwin)
        } else {
            (c.edwin, c.mara)
        };
        ally(&mut w, a, b, -300);
        unally(&mut w, b, a, -250);
        ally(&mut w, b, a, -200);
        let e = w.edge_id(a, "ally_of", b).unwrap();
        for (x, p) in [(c.edwin, c.mara), (c.mara, c.edwin)] {
            for t in [
                i64::MIN,
                -301,
                -300,
                -251,
                -250,
                -249,
                -201,
                -200,
                0,
                i64::MAX,
            ] {
                let on = (-300..-250).contains(&t) || t >= -200;
                for d in DIRS {
                    assert_eq!(
                        w.neighbors(x, "ally_of", d, Tick(t)).unwrap(),
                        if on { vec![(p, e)] } else { vec![] },
                        "{x:?} {d:?} @ {t}"
                    );
                }
                assert_eq!(
                    w.as_of(x, "ally_of", Tick(t)).unwrap(),
                    if on { vec![Value::Ref(p)] } else { vec![] }
                );
                assert_eq!(
                    w.as_of_value(x, "ally_of", &Value::Ref(p), Tick(t))
                        .unwrap(),
                    on
                );
            }
            let h = w.history(x, "ally_of").unwrap();
            let want = vec![
                Change {
                    tick: Tick(-300),
                    op: Op::Assert,
                    value: Value::Ref(p),
                },
                Change {
                    tick: Tick(-250),
                    op: Op::Retract,
                    value: Value::Ref(p),
                },
                Change {
                    tick: Tick(-200),
                    op: Op::Assert,
                    value: Value::Ref(p),
                },
            ];
            assert_eq!(h, want, "history of {x:?}");
        }
    }
}

#[test]
fn several_partners_mix_spellings_and_sort_by_partner() {
    let (mut w, c) = fx();
    ally(&mut w, c.lisa, c.mara, 0); // larger first
    ally(&mut w, c.mara, c.edwin, 0); // larger first
    ally(&mut w, c.mara, c.greg, 0); // smaller first
    let e_em = w.edge_id(c.edwin, "ally_of", c.mara).unwrap();
    let e_ml = w.edge_id(c.mara, "ally_of", c.lisa).unwrap();
    let e_mg = w.edge_id(c.greg, "ally_of", c.mara).unwrap();
    let want = vec![(c.edwin, e_em), (c.lisa, e_ml), (c.greg, e_mg)];
    for d in DIRS {
        assert_eq!(
            w.neighbors(c.mara, "ally_of", d, Tick(0)).unwrap(),
            want,
            "{d:?}"
        );
    }
    assert_eq!(
        w.as_of(c.mara, "ally_of", Tick(0)).unwrap(),
        vec![Value::Ref(c.edwin), Value::Ref(c.lisa), Value::Ref(c.greg)]
    );
    for (x, e) in [(c.edwin, e_em), (c.lisa, e_ml), (c.greg, e_mg)] {
        for d in DIRS {
            assert_eq!(
                w.neighbors(x, "ally_of", d, Tick(0)).unwrap(),
                vec![(c.mara, e)]
            );
        }
    }
    // Lisa and Greg are not allied
    assert!(
        w.neighbors(c.lisa, "ally_of", Direction::Both, Tick(0))
            .unwrap()
            .iter()
            .all(|n| n.0 != c.greg)
    );
}

// ---------------------------------------------------------------------------
// Same-tick conflicts and no-ops across spellings
// ---------------------------------------------------------------------------

#[test]
fn same_tick_conflict_across_spellings_names_the_entity_as_written() {
    let (mut w, c) = fx();
    ally(&mut w, c.edwin, c.mara, 10);
    let tx = w.latest_tx();
    let r = w.retract(c.mara, "ally_of", Value::Ref(c.edwin), tk(10));
    assert_eq!(
        r,
        Err(Error::SameTickConflict {
            entity: c.mara,
            attr: "ally_of".to_string(),
            tick: Tick(10)
        })
    );
    assert_eq!(w.latest_tx(), tx);
    // other order of writing: retract stored, assert with the other spelling
    unally(&mut w, c.edwin, c.mara, 20);
    let r = w.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(20));
    assert_eq!(
        r,
        Err(Error::SameTickConflict {
            entity: c.mara,
            attr: "ally_of".to_string(),
            tick: Tick(20)
        })
    );
    let r = w.assert(c.edwin, "ally_of", Value::Ref(c.mara), tk(20));
    assert_eq!(
        r,
        Err(Error::SameTickConflict {
            entity: c.edwin,
            attr: "ally_of".to_string(),
            tick: Tick(20)
        })
    );
    // the alliance still reads: [10, 20)
    assert_eq!(
        w.as_of(c.edwin, "ally_of", Tick(19)).unwrap(),
        vec![Value::Ref(c.mara)]
    );
    assert!(w.as_of(c.edwin, "ally_of", Tick(20)).unwrap().is_empty());
}

#[test]
fn repeating_the_same_op_with_the_other_spelling_at_the_same_tick_is_a_noop() {
    let (mut w, c) = fx();
    ally(&mut w, c.edwin, c.mara, 10);
    let tx = w.latest_tx();
    assert_eq!(ally(&mut w, c.mara, c.edwin, 10).changed, 0);
    unally(&mut w, c.edwin, c.mara, 20);
    let tx2 = w.latest_tx();
    assert_eq!(unally(&mut w, c.mara, c.edwin, 20).changed, 0);
    assert_eq!(w.latest_tx(), tx2);
    assert_eq!(w.history(c.edwin, "ally_of").unwrap().len(), 2);
    assert!(tx < tx2);
}

// ---------------------------------------------------------------------------
// Index rows
// ---------------------------------------------------------------------------

type ERow = (EntityId, AttrId, Value, Tick, TxId, Mark);
type RRow = (EntityId, AttrId, EntityId, Tick, TxId, Mark);

fn ref_rows(w: &World) -> (Vec<ERow>, Vec<RRow>) {
    let d = w.index_dump();
    let e =
        d.e.into_iter()
            .filter(|r| matches!(r.2, Value::Ref(_)))
            .collect();
    (e, d.r)
}

#[test]
fn one_e_row_under_min_and_one_r_row_under_max_per_write() {
    let (mut w, c) = fx();
    ally(&mut w, c.mara, c.edwin, 10); // larger first
    ally(&mut w, c.lisa, c.mara, 11); // larger first
    unally(&mut w, c.edwin, c.mara, 12); // smaller first
    ally(&mut w, c.mara, c.edwin, 10); // no-op: no rows
    let (e, r) = ref_rows(&w);
    assert_eq!(e.len(), 3, "{e:?}");
    assert_eq!(r.len(), 3, "{r:?}");
    for (ent, _, v, ..) in &e {
        let Value::Ref(partner) = v else {
            unreachable!()
        };
        assert!(
            ent <= partner,
            "E row under the smaller id: {ent:?} {partner:?}"
        );
    }
    for (target, _, source, ..) in &r {
        assert!(source <= target, "R row under the larger id");
    }
    let ally_attr = e[0].1;
    // exactly these rows, in any key order
    let mut got_e: Vec<_> = e
        .iter()
        .map(|x| (x.0, x.2.clone(), x.3, x.5))
        .map(|(a, b, t, o)| (a.raw(), format!("{b:?}"), t, o as u8))
        .collect();
    got_e.sort();
    let mut want_e = vec![
        (c.edwin, Value::Ref(c.mara), Tick(10), Op::Assert),
        (c.mara, Value::Ref(c.lisa), Tick(11), Op::Assert),
        (c.edwin, Value::Ref(c.mara), Tick(12), Op::Retract),
    ]
    .into_iter()
    .map(|(a, b, t, o)| (a.raw(), format!("{b:?}"), t, o as u8))
    .collect::<Vec<_>>();
    want_e.sort();
    assert_eq!(got_e, want_e);
    let mut got_r: Vec<_> = r
        .iter()
        .map(|x| (x.0.raw(), x.2.raw(), x.3, x.5 as u8))
        .collect();
    got_r.sort();
    let mut want_r = vec![
        (c.mara.raw(), c.edwin.raw(), Tick(10), Op::Assert as u8),
        (c.lisa.raw(), c.mara.raw(), Tick(11), Op::Assert as u8),
        (c.mara.raw(), c.edwin.raw(), Tick(12), Op::Retract as u8),
    ];
    want_r.sort();
    assert_eq!(got_r, want_r);
    assert!(e.iter().all(|x| x.1 == ally_attr) && r.iter().all(|x| x.1 == ally_attr));
    // mirror: every E row has its R row with the same tick/tx/op
    for (ent, a, v, t, tx, op) in &e {
        let Value::Ref(p) = v else { unreachable!() };
        assert!(
            r.contains(&(*p, *a, *ent, *t, *tx, *op)),
            "mirror of {ent:?}->{p:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Self alliance
// ---------------------------------------------------------------------------

#[test]
fn self_alliance_is_one_edge_and_neighbors_returns_the_entity_once() {
    let (mut w, c) = fx();
    assert_eq!(ally(&mut w, c.edwin, c.edwin, 5).changed, 1);
    let e = w.edge_id(c.edwin, "ally_of", c.edwin).unwrap();
    assert_eq!(w.edge(e), Some((c.edwin, "ally_of".to_string(), c.edwin)));
    for d in DIRS {
        assert_eq!(
            w.neighbors(c.edwin, "ally_of", d, Tick(5)).unwrap(),
            vec![(c.edwin, e)],
            "{d:?}"
        );
        assert!(
            w.neighbors(c.edwin, "ally_of", d, Tick(4))
                .unwrap()
                .is_empty()
        );
        assert!(
            w.neighbors(c.mara, "ally_of", d, Tick(5))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        w.as_of(c.edwin, "ally_of", Tick(5)).unwrap(),
        vec![Value::Ref(c.edwin)]
    );
    assert!(
        w.as_of_value(c.edwin, "ally_of", &Value::Ref(c.edwin), Tick(5))
            .unwrap()
    );
    assert_eq!(ally(&mut w, c.edwin, c.edwin, 6).changed, 0);
    assert_eq!(unally(&mut w, c.edwin, c.edwin, 7).changed, 1);
    for d in DIRS {
        assert!(
            w.neighbors(c.edwin, "ally_of", d, Tick(7))
                .unwrap()
                .is_empty()
        );
    }
    assert!(w.as_of(c.edwin, "ally_of", Tick(7)).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Edge properties
// ---------------------------------------------------------------------------

#[test]
fn edge_properties_work_through_either_edge_id_spelling() {
    let (mut w, c) = fx();
    ally(&mut w, c.mara, c.edwin, -300);
    let e1 = w.edge_id(c.edwin, "ally_of", c.mara).unwrap();
    let e2 = w.edge_id(c.mara, "ally_of", c.edwin).unwrap();
    assert_eq!(e1, e2);
    assert_eq!(
        w.assert(e1.into(), "disguised", Value::Bool(true), tk(-300))
            .unwrap()
            .changed,
        1
    );
    // the same fact through the other spelling's ID is the same entity: no-op
    assert_eq!(
        w.assert(e2.into(), "disguised", Value::Bool(true), tk(-300))
            .unwrap()
            .changed,
        0
    );
    w.retract(e2.into(), "disguised", Value::Bool(true), tk(-280))
        .unwrap();
    w.assert(e1.into(), "disguised", Value::Bool(false), tk(-280))
        .unwrap();
    for id in [e1, e2] {
        assert_eq!(
            w.as_of(id.into(), "disguised", Tick(-290)).unwrap(),
            vec![Value::Bool(true)]
        );
        assert_eq!(
            w.as_of(id.into(), "disguised", Tick(-280)).unwrap(),
            vec![Value::Bool(false)]
        );
        assert_eq!(w.history(id.into(), "disguised").unwrap().len(), 3);
    }
}

// ---------------------------------------------------------------------------
// Directed attributes are not normalised
// ---------------------------------------------------------------------------

#[test]
fn directed_trusts_keeps_two_edges_the_traitor_example() {
    let (mut w, c) = fx();
    // Mara trusts Edwin, but Edwin does not trust Mara.
    assert_eq!(
        w.assert(c.mara, "trusts", Value::Ref(c.edwin), tk(0))
            .unwrap()
            .changed,
        1
    );
    let m_e = w.edge_id(c.mara, "trusts", c.edwin).unwrap();
    assert_eq!(w.edge_id(c.edwin, "trusts", c.mara), None);
    assert_eq!(
        w.edge(m_e),
        Some((c.mara, "trusts".to_string(), c.edwin)),
        "stored as written (larger first)"
    );
    assert_eq!(
        w.neighbors(c.edwin, "trusts", Direction::Out, Tick(0))
            .unwrap(),
        vec![]
    );
    assert_eq!(
        w.neighbors(c.edwin, "trusts", Direction::In, Tick(0))
            .unwrap(),
        vec![(c.mara, m_e)]
    );
    assert_eq!(
        w.neighbors(c.mara, "trusts", Direction::Out, Tick(0))
            .unwrap(),
        vec![(c.edwin, m_e)]
    );
    assert_eq!(
        w.neighbors(c.mara, "trusts", Direction::In, Tick(0))
            .unwrap(),
        vec![]
    );
    // now the other direction: a second, independent edge
    assert_eq!(
        w.assert(c.edwin, "trusts", Value::Ref(c.mara), tk(5))
            .unwrap()
            .changed,
        1
    );
    let e_m = w.edge_id(c.edwin, "trusts", c.mara).unwrap();
    assert_ne!(e_m, m_e);
    assert_eq!(w.edge(e_m), Some((c.edwin, "trusts".to_string(), c.mara)));
    // each direction has its own history and retract
    assert_eq!(
        w.retract(c.mara, "trusts", Value::Ref(c.edwin), tk(9))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        w.neighbors(c.edwin, "trusts", Direction::Out, Tick(9))
            .unwrap(),
        vec![(c.mara, e_m)]
    );
    assert_eq!(
        w.neighbors(c.mara, "trusts", Direction::Out, Tick(9))
            .unwrap(),
        vec![]
    );
    // opposite-direction writes never conflict with each other at one tick
    assert_eq!(
        w.assert(c.mara, "trusts", Value::Ref(c.edwin), tk(20))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        w.retract(c.edwin, "trusts", Value::Ref(c.mara), tk(20))
            .unwrap()
            .changed,
        1
    );
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

#[test]
fn txn_second_spelling_at_the_same_tick_is_changed_0() {
    let (mut w, c) = fx();
    let mut t = w.begin();
    assert_eq!(
        t.assert(c.edwin, "ally_of", Value::Ref(c.mara), tk(7))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(7))
            .unwrap()
            .changed,
        0
    );
    let a = t.edge_id(c.edwin, "ally_of", c.mara).expect("staged edge");
    assert_eq!(t.edge_id(c.mara, "ally_of", c.edwin), Some(a));
    t.commit().unwrap();
    assert_eq!(w.edge_id(c.mara, "ally_of", c.edwin), Some(a));
    assert_eq!(a.seq(), 1);
    let (e, r) = ref_rows(&w);
    assert_eq!((e.len(), r.len()), (1, 1));
    assert_eq!(w.history(c.mara, "ally_of").unwrap().len(), 1);
}

#[test]
fn txn_larger_first_is_stored_normalised_and_the_other_spelling_noops() {
    let (mut w, c) = fx();
    let mut t = w.begin();
    t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(7))
        .unwrap();
    let a = t.edge_id(c.edwin, "ally_of", c.mara).expect("staged edge");
    assert_eq!(t.edge_id(c.mara, "ally_of", c.edwin), Some(a));
    // later tick, other spelling: already holds, no-op
    assert_eq!(
        t.assert(c.edwin, "ally_of", Value::Ref(c.mara), tk(8))
            .unwrap()
            .changed,
        0
    );
    t.commit().unwrap();
    assert_eq!(w.edge(a), Some((c.edwin, "ally_of".to_string(), c.mara)));
}

#[test]
fn txn_retract_then_other_spelling_assert_at_same_tick_conflicts_and_poisons() {
    let (mut w, c) = fx();
    ally(&mut w, c.edwin, c.mara, 5);
    let tx = w.latest_tx();
    let mut t = w.begin();
    assert_eq!(
        t.retract(c.edwin, "ally_of", Value::Ref(c.mara), tk(10))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(
        t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(10)),
        Err(Error::SameTickConflict {
            entity: c.mara,
            attr: "ally_of".to_string(),
            tick: Tick(10)
        })
    );
    // poisoned
    assert_eq!(
        t.assert(c.edwin, "ally_of", Value::Ref(c.lisa), tk(11)),
        Err(Error::TransactionAborted)
    );
    assert_eq!(t.commit(), Err(Error::TransactionAborted));
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(
        w.as_of(c.edwin, "ally_of", Tick(10)).unwrap(),
        vec![Value::Ref(c.mara)],
        "the staged retract is not visible"
    );
}

#[test]
fn txn_assert_then_other_spelling_retract_at_same_tick_conflicts() {
    let (mut w, c) = fx();
    let mut t = w.begin();
    t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(10))
        .unwrap();
    assert_eq!(
        t.retract(c.edwin, "ally_of", Value::Ref(c.mara), tk(10)),
        Err(Error::SameTickConflict {
            entity: c.edwin,
            attr: "ally_of".to_string(),
            tick: Tick(10)
        })
    );
    assert_eq!(t.commit(), Err(Error::TransactionAborted));
    assert!(w.edge_id(c.edwin, "ally_of", c.mara).is_none());
}

#[test]
fn failed_write_after_a_normalised_staged_write_poisons_and_consumes_no_edge() {
    let (mut w, c) = fx();
    let tx = w.latest_tx();
    {
        let mut t = w.begin();
        t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(5))
            .unwrap();
        assert!(t.edge_id(c.edwin, "ally_of", c.mara).is_some());
        assert!(matches!(
            t.assert(c.edwin, "no_such_attr", Value::Bool(true), tk(5)),
            Err(Error::UnknownAttribute(_))
        ));
        assert_eq!(
            t.assert(c.edwin, "ally_of", Value::Ref(c.lisa), tk(5)),
            Err(Error::TransactionAborted)
        );
        assert_eq!(t.commit(), Err(Error::TransactionAborted));
    }
    assert_eq!(w.latest_tx(), tx);
    assert!(w.edge_id(c.edwin, "ally_of", c.mara).is_none());
    assert!(w.as_of(c.edwin, "ally_of", Tick(5)).unwrap().is_empty());
    // dropped without commit: also no edge consumed
    {
        let mut t = w.begin();
        t.assert(c.mara, "ally_of", Value::Ref(c.edwin), tk(5))
            .unwrap();
        let _ = t.assert(c.edwin, "no_such_attr", Value::Bool(true), tk(5));
    }
    ally(&mut w, c.edwin, c.lisa, 1);
    assert_eq!(w.edge_id(c.edwin, "ally_of", c.lisa).unwrap().seq(), 1);
}

#[test]
fn as_of_value_with_an_undefined_ref_value_is_ok_false() {
    let (w, c) = fx();
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    assert_eq!(
        w.as_of_value(c.edwin, "ally_of", &Value::Ref(ghost), Tick(0)),
        Ok(false)
    );
    let edge_ghost: EntityId = EdgeId::new(77).unwrap().into();
    assert_eq!(
        w.as_of_value(c.edwin, "ally_of", &Value::Ref(edge_ghost), Tick(0)),
        Ok(false)
    );
}

// ---------------------------------------------------------------------------
// Self pair: appears once everywhere, each stored change once in history
// ---------------------------------------------------------------------------

#[test]
fn self_pair_history_lists_each_stored_change_once_and_old_canon_agrees() {
    let (mut w, c) = fx();
    ally(&mut w, c.edwin, c.edwin, 5);
    let tx_a = w.latest_tx();
    unally(&mut w, c.edwin, c.edwin, 9);
    let tx_b = w.latest_tx();
    ally(&mut w, c.edwin, c.edwin, 20);
    let me = Value::Ref(c.edwin);
    let want = vec![
        Change {
            tick: Tick(5),
            op: Op::Assert,
            value: me.clone(),
        },
        Change {
            tick: Tick(9),
            op: Op::Retract,
            value: me.clone(),
        },
        Change {
            tick: Tick(20),
            op: Op::Assert,
            value: me.clone(),
        },
    ];
    assert_eq!(w.history(c.edwin, "ally_of").unwrap(), want);
    assert_eq!(
        w.at_canon(tx_b).history(c.edwin, "ally_of").unwrap(),
        want[..2]
    );
    assert_eq!(
        w.at_canon(tx_a).history(c.edwin, "ally_of").unwrap(),
        want[..1]
    );
    let e = w.edge_id(c.edwin, "ally_of", c.edwin).unwrap();
    for (v, t, want_len) in [
        (&w.at_canon(tx_a), 7, 1),
        (&w.at_canon(tx_a), 100, 1),
        (&w.at_canon(tx_b), 100, 0),
    ] {
        assert_eq!(
            v.as_of(c.edwin, "ally_of", Tick(t)).unwrap().len(),
            want_len
        );
        for d in DIRS {
            assert_eq!(
                v.neighbors(c.edwin, "ally_of", d, Tick(t)).unwrap(),
                if want_len == 1 {
                    vec![(c.edwin, e)]
                } else {
                    vec![]
                },
                "{d:?} t={t}"
            );
        }
    }
    // before it was written
    let v0 = w.at_canon(TxId(w.latest_tx().0 - 3));
    assert!(v0.history(c.edwin, "ally_of").unwrap().is_empty());
    assert!(v0.as_of(c.edwin, "ally_of", Tick(7)).unwrap().is_empty());
}

#[test]
fn self_pair_mixed_with_other_partners_at_the_same_tick_orders_by_value() {
    let (mut w, c) = fx();
    // lisa (3) is written before edwin's self pair and mara; greg after.
    ally(&mut w, c.lisa, c.edwin, 5); // stored (edwin, lisa)
    ally(&mut w, c.edwin, c.edwin, 5);
    ally(&mut w, c.mara, c.edwin, 5); // stored (edwin, mara)
    ally(&mut w, c.edwin, c.greg, 5);
    let want_vals = vec![
        Value::Ref(c.edwin),
        Value::Ref(c.mara),
        Value::Ref(c.lisa),
        Value::Ref(c.greg),
    ];
    assert_eq!(w.as_of(c.edwin, "ally_of", Tick(5)).unwrap(), want_vals);
    let ns = w
        .neighbors(c.edwin, "ally_of", Direction::Both, Tick(5))
        .unwrap();
    assert_eq!(
        ns.iter().map(|n| n.0).collect::<Vec<_>>(),
        vec![c.edwin, c.mara, c.lisa, c.greg]
    );
    for d in DIRS {
        assert_eq!(
            w.neighbors(c.edwin, "ally_of", d, Tick(5)).unwrap(),
            ns,
            "{d:?}"
        );
    }
    let h = w.history(c.edwin, "ally_of").unwrap();
    assert_eq!(h.len(), 4, "each stored change once: {h:?}");
    assert_eq!(
        h.iter().map(|x| x.value.clone()).collect::<Vec<_>>(),
        want_vals
    );
    // partner sides: each sees edwin once
    for p in [c.mara, c.lisa, c.greg] {
        assert_eq!(
            w.as_of(p, "ally_of", Tick(5)).unwrap(),
            vec![Value::Ref(c.edwin)]
        );
        assert_eq!(w.history(p, "ally_of").unwrap().len(), 1);
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn snapshot(w: &World, c: &Cast) -> String {
    let mut s = String::new();
    let ents = [c.edwin, c.mara, c.lisa, c.greg];
    for &x in &ents {
        for &y in &ents {
            s += &format!("edge {x:?} {y:?} = {:?}\n", w.edge_id(x, "ally_of", y));
            for t in [i64::MIN, -5, 0, 5, 10, 20, i64::MAX] {
                s += &format!(
                    "asof_value {x:?} {y:?} {t} = {:?}\n",
                    w.as_of_value(x, "ally_of", &Value::Ref(y), Tick(t))
                        .unwrap()
                );
            }
        }
        s += &format!("history {x:?} = {:?}\n", w.history(x, "ally_of").unwrap());
        for t in [i64::MIN, -5, 0, 5, 10, 20, i64::MAX] {
            s += &format!(
                "as_of {x:?} {t} = {:?}\n",
                w.as_of(x, "ally_of", Tick(t)).unwrap()
            );
            for d in DIRS {
                s += &format!(
                    "nb {x:?} {d:?} {t} = {:?}\n",
                    w.neighbors(x, "ally_of", d, Tick(t)).unwrap()
                );
            }
        }
    }
    for n in 1..=4 {
        s += &format!("edge#{n} = {:?}\n", w.edge(EdgeId::new(n).unwrap()));
    }
    s += &format!("{:?}\n", w.index_dump());
    s
}

fn populate_mixed(w: &mut World, c: &Cast) {
    ally(w, c.mara, c.edwin, -5);
    unally(w, c.edwin, c.mara, 5);
    ally(w, c.edwin, c.mara, 10);
    ally(w, c.lisa, c.mara, 0);
    ally(w, c.edwin, c.greg, 0);
    ally(w, c.greg, c.greg, 0);
}

#[test]
fn reopen_gives_identical_answers_and_the_log_holds_ordered_pairs() {
    let (_d, p, mut w, c) = file_fx();
    populate_mixed(&mut w, &c);
    let before = snapshot(&w, &c);
    assert!(before.contains("nb "), "sanity");
    drop(w);
    let w2 = World::open(&p).unwrap();
    assert_eq!(snapshot(&w2, &c), before);
    drop(w2);

    let log = read_log(&p).unwrap();
    let mut edges = 0;
    let mut facts = 0;
    let ally_attr = log
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == "ally_of" => Some(*id),
            _ => None,
        })
        .unwrap();
    for e in &log {
        match &e.body {
            EntryBody::DefineEdge {
                source,
                attr,
                target,
                ..
            } if *attr == ally_attr => {
                edges += 1;
                assert!(
                    source <= target,
                    "DefineEdge source <= target: {source:?} {target:?}"
                );
            }
            EntryBody::Fact {
                entity,
                attr,
                value: Value::Ref(v),
                ..
            } if *attr == ally_attr => {
                facts += 1;
                assert!(entity <= v, "Fact entity <= value: {entity:?} {v:?}");
            }
            _ => {}
        }
    }
    assert_eq!(edges, 4, "edwin-mara, lisa-mara, edwin-greg, greg-greg");
    assert_eq!(facts, 6, "-5, 5, 10, 0, 0, 0; the no-ops are not stored");
    let strict = log
        .iter()
        .filter(|e| {
            matches!(&e.body,
        EntryBody::DefineEdge { source, attr, target, .. } if *attr == ally_attr && source < target)
        })
        .count();
    assert_eq!(strict, 3);
}

// ---- hand-built replay ----

fn define_edge(id: u64, source: u64, attr: u32, target: u64) -> (u8, Vec<u8>) {
    let mut b = id.to_le_bytes().to_vec();
    b.extend_from_slice(&source.to_le_bytes());
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&target.to_le_bytes());
    (6, b)
}

fn edge_raw(seq: u64) -> u64 {
    (1u64 << 63) | seq
}

fn fact(entity: u64, attr: u32, tick: i64, retract: bool, value: &Value) -> (u8, Vec<u8>) {
    let mut b = entity.to_le_bytes().to_vec();
    b.extend_from_slice(&attr.to_le_bytes());
    b.extend_from_slice(&tick.to_le_bytes());
    b.push(retract as u8);
    b.extend_from_slice(&value.encode().unwrap());
    (4, b)
}

fn append_txn(path: &Path, entries: &[(u8, Vec<u8>)]) {
    let log = read_log(path).unwrap();
    let mut next_id = log.last().unwrap().id.0 + 1;
    let tx = log
        .iter()
        .filter(|e| matches!(e.body, EntryBody::Commit { .. }))
        .count() as u64;
    let mut body = Vec::new();
    for (kind, b) in entries {
        body.extend(common::frame(*kind, next_id, b));
        next_id += 1;
    }
    let commit = common::commit_frame(&body, next_id, tx);
    body.extend(commit);
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    use std::io::Write;
    f.write_all(&common::txn(tx, &body)).unwrap();
}

struct Hand {
    _dir: tempfile::TempDir,
    path: PathBuf,
    c: Cast,
    ally: u32,
}

fn hand() -> Hand {
    let (dir, path, w, c) = file_fx();
    drop(w);
    let ally = read_log(&path)
        .unwrap()
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == "ally_of" => Some(id.0),
            _ => None,
        })
        .unwrap();
    Hand {
        _dir: dir,
        path,
        c,
        ally,
    }
}

fn assert_corrupt(h: &Hand, entries: &[(u8, Vec<u8>)], label: &str) {
    append_txn(&h.path, entries);
    let before = std::fs::read(&h.path).unwrap();
    match World::open(&h.path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: expected Corrupt, open succeeded"),
    }
    assert_eq!(
        std::fs::read(&h.path).unwrap(),
        before,
        "{label}: file touched"
    );
}

#[test]
fn control_correctly_ordered_hand_built_bidirectional_edge_opens() {
    let h = hand();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(1), h.c.edwin.raw(), h.ally, h.c.mara.raw()),
            fact(h.c.edwin.raw(), h.ally, 5, false, &Value::Ref(h.c.mara)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    let e = EdgeId::new(1).unwrap();
    assert_eq!(w.edge_id(h.c.mara, "ally_of", h.c.edwin), Some(e));
    assert_eq!(
        w.edge(e),
        Some((h.c.edwin, "ally_of".to_string(), h.c.mara))
    );
    assert_eq!(
        w.neighbors(h.c.mara, "ally_of", Direction::Both, Tick(5))
            .unwrap(),
        vec![(h.c.edwin, e)]
    );
    assert_eq!(
        w.neighbors(h.c.edwin, "ally_of", Direction::Both, Tick(5))
            .unwrap(),
        vec![(h.c.mara, e)]
    );
}

#[test]
fn replay_bidirectional_define_edge_with_source_above_target_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(
            edge_raw(1),
            h.c.mara.raw(),
            h.ally,
            h.c.edwin.raw(),
        )],
        "DefineEdge(mara, ally_of, edwin)",
    );
    // with a matching fact too
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.c.mara.raw(), h.ally, h.c.edwin.raw()),
            fact(h.c.mara.raw(), h.ally, 5, false, &Value::Ref(h.c.edwin)),
        ],
        "DefineEdge + Fact both larger first",
    );
}

#[test]
fn replay_bidirectional_fact_with_entity_above_value_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.c.edwin.raw(), h.ally, h.c.mara.raw()),
            fact(h.c.mara.raw(), h.ally, 5, false, &Value::Ref(h.c.edwin)),
        ],
        "assert stored larger first",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.c.edwin.raw(), h.ally, h.c.mara.raw()),
            fact(h.c.edwin.raw(), h.ally, 1, false, &Value::Ref(h.c.mara)),
            fact(h.c.mara.raw(), h.ally, 5, true, &Value::Ref(h.c.edwin)),
        ],
        "retract stored larger first",
    );
}

#[test]
fn replay_of_a_bidirectional_self_pair_define_edge_is_accepted() {
    let h = hand();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(1), h.c.lisa.raw(), h.ally, h.c.lisa.raw()),
            fact(h.c.lisa.raw(), h.ally, 5, false, &Value::Ref(h.c.lisa)),
            fact(h.c.lisa.raw(), h.ally, 9, true, &Value::Ref(h.c.lisa)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    let e = EdgeId::new(1).unwrap();
    assert_eq!(w.edge(e), Some((h.c.lisa, "ally_of".to_string(), h.c.lisa)));
    assert_eq!(
        w.neighbors(h.c.lisa, "ally_of", Direction::Both, Tick(5))
            .unwrap(),
        vec![(h.c.lisa, e)]
    );
    assert!(
        w.neighbors(h.c.lisa, "ally_of", Direction::Both, Tick(9))
            .unwrap()
            .is_empty()
    );
    assert_eq!(w.history(h.c.lisa, "ally_of").unwrap().len(), 2);
}

#[test]
fn replay_larger_first_define_edge_on_a_non_first_edge_seq_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.c.edwin.raw(), h.ally, h.c.mara.raw()),
            define_edge(edge_raw(2), h.c.greg.raw(), h.ally, h.c.lisa.raw()),
        ],
        "edge#2 DefineEdge(greg, ally_of, lisa)",
    );
    // the same shape across two transactions
    let h = hand();
    append_txn(
        &h.path,
        &[define_edge(
            edge_raw(1),
            h.c.edwin.raw(),
            h.ally,
            h.c.mara.raw(),
        )],
    );
    assert_corrupt(
        &h,
        &[define_edge(
            edge_raw(2),
            h.c.lisa.raw(),
            h.ally,
            h.c.mara.raw(),
        )],
        "edge#2 in a later txn",
    );
}

#[test]
fn control_directed_attribute_may_be_stored_larger_first() {
    let h = hand();
    let trusts = read_log(&h.path)
        .unwrap()
        .iter()
        .find_map(|e| match &e.body {
            EntryBody::DefineAttr { id, def } if def.name == "trusts" => Some(id.0),
            _ => None,
        })
        .unwrap();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(1), h.c.mara.raw(), trusts, h.c.edwin.raw()),
            fact(h.c.mara.raw(), trusts, 5, false, &Value::Ref(h.c.edwin)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(
        w.edge(EdgeId::new(1).unwrap()),
        Some((h.c.mara, "trusts".to_string(), h.c.edwin))
    );
}

// ---------------------------------------------------------------------------
// Property test against a naive model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct MFact {
    key: (usize, usize),
    tick: i64,
    op: Op,
    tx: u64,
}

fn holds(facts: &[MFact], canon: u64, key: (usize, usize), t: i64) -> bool {
    facts
        .iter()
        .filter(|f| f.key == key && f.tx <= canon && f.tick <= t)
        .max_by_key(|f| f.tick)
        .is_some_and(|f| f.op == Op::Assert)
}

fn w_strategy() -> impl Strategy<Value = (usize, usize, i64, bool)> {
    (
        0usize..4,
        0usize..4,
        prop_oneof![8 => -3i64..=3, 1 => Just(i64::MIN), 1 => Just(i64::MAX)],
        any::<bool>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn bidirectional_matches_the_naive_model_at_every_canon(
        writes in prop::collection::vec(w_strategy(), 1..25)
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let c = cast(&mut w);
        let ents = [c.edwin, c.mara, c.lisa, c.greg]; // ascending by ID
        let setup_tx = w.latest_tx().0;
        let mut facts: Vec<MFact> = Vec::new();
        let mut interned: Vec<(usize, usize)> = Vec::new();
        let mut ticks = std::collections::BTreeSet::from([i64::MIN, 0, i64::MAX]);

        for (a, b, t, retract) in writes {
            if a == b { continue; } // self-edges: dedicated test
            let key = (a.min(b), a.max(b));
            // model verdict
            let same_tick = facts.iter().find(|f| f.key == key && f.tick == t);
            let op = if retract { Op::Retract } else { Op::Assert };
            let latest = u64::MAX;
            enum V { Conflict, Noop, Store }
            let verdict = match same_tick {
                Some(f) if f.op != op => V::Conflict,
                Some(_) => V::Noop,
                None => {
                    let h = holds(&facts, latest, key, t);
                    if h == (op == Op::Assert) { V::Noop } else { V::Store }
                }
            };
            let r = if retract {
                w.retract(ents[a], "ally_of", Value::Ref(ents[b]), tk(t))
            } else {
                w.assert(ents[a], "ally_of", Value::Ref(ents[b]), tk(t))
            };
            match verdict {
                V::Conflict => {
                    prop_assert_eq!(r, Err(Error::SameTickConflict {
                        entity: ents[a], attr: "ally_of".to_string(), tick: Tick(t) }),
                        "model predicts conflict: {} {} {} @ {}", a, b, retract, t);
                }
                V::Noop => {
                    prop_assert_eq!(r.map(|x| x.changed), Ok(0), "model predicts no-op");
                }
                V::Store => {
                    prop_assert_eq!(r.map(|x| x.changed), Ok(1), "model predicts store");
                    facts.push(MFact { key, tick: t, op, tx: w.latest_tx().0 });
                    if op == Op::Assert && !interned.contains(&key) {
                        interned.push(key);
                    }
                    for d in [-1i64, 0, 1] { ticks.insert(t.saturating_add(d)); }
                }
            }
        }

        // edges
        let edge_of = |k: (usize, usize)| interned.iter().position(|x| *x == k)
            .map(|i| EdgeId::new(i as u64 + 1).unwrap());
        for a in 0..4 {
            for b in 0..4 {
                if a == b { continue; }
                let k = (a.min(b), a.max(b));
                prop_assert_eq!(w.edge_id(ents[a], "ally_of", ents[b]), edge_of(k));
            }
        }
        for (i, k) in interned.iter().enumerate() {
            prop_assert_eq!(
                w.edge(EdgeId::new(i as u64 + 1).unwrap()),
                Some((ents[k.0], "ally_of".to_string(), ents[k.1]))
            );
        }

        let latest = w.latest_tx().0;
        for canon in setup_tx..=latest {
            let view = w.at_canon(TxId(canon));
            for x in 0..4 {
                for &t in &ticks {
                    let partners: Vec<usize> = (0..4)
                        .filter(|&p| p != x && holds(&facts, canon, (x.min(p), x.max(p)), t))
                        .collect();
                    let nb: Vec<(EntityId, EdgeId)> = partners.iter()
                        .map(|&p| (ents[p], edge_of((x.min(p), x.max(p))).unwrap()))
                        .collect();
                    for d in DIRS {
                        prop_assert_eq!(&view.neighbors(ents[x], "ally_of", d, Tick(t)).unwrap(), &nb,
                            "canon {} x {} {:?} @ {}", canon, x, d, t);
                    }
                    let vals: Vec<Value> = partners.iter().map(|&p| Value::Ref(ents[p])).collect();
                    prop_assert_eq!(&view.as_of(ents[x], "ally_of", Tick(t)).unwrap(), &vals);
                    for p in 0..4 {
                        if p == x { continue; }
                        prop_assert_eq!(
                            view.as_of_value(ents[x], "ally_of", &Value::Ref(ents[p]), Tick(t)).unwrap(),
                            partners.contains(&p));
                    }
                }
                let mut want: Vec<(i64, usize, Op)> = facts.iter()
                    .filter(|f| f.tx <= canon && (f.key.0 == x || f.key.1 == x))
                    .map(|f| (f.tick, if f.key.0 == x { f.key.1 } else { f.key.0 }, f.op))
                    .collect();
                want.sort_by_key(|(t, p, _)| (*t, *p));
                let want: Vec<Change> = want.into_iter()
                    .map(|(t, p, op)| Change { tick: Tick(t), op, value: Value::Ref(ents[p]) })
                    .collect();
                prop_assert_eq!(view.history(ents[x], "ally_of").unwrap(), want, "history canon {} x {}", canon, x);
            }
        }

        // index rows: one E under min, one R under max per stored fact
        let (e, r) = ref_rows(&w);
        prop_assert_eq!(e.len(), facts.len());
        prop_assert_eq!(r.len(), facts.len());
        for f in &facts {
            let (lo, hi) = (ents[f.key.0], ents[f.key.1]);
            prop_assert!(e.iter().any(|x| x.0 == lo && x.2 == Value::Ref(hi)
                && x.3 == Tick(f.tick) && x.4 == TxId(f.tx) && x.5 == f.op), "E row {:?}", f);
            prop_assert!(r.iter().any(|x| x.0 == hi && x.2 == lo
                && x.3 == Tick(f.tick) && x.4 == TxId(f.tx) && x.5 == f.op), "R row {:?}", f);
        }
    }
}
