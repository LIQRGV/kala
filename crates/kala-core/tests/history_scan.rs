//! Slice 7, item 0(a)+(c): `history_scan` (RED step; written from the contract).
//!
//! API under test (new):
//!   `View::history_scan(&self, attr: &str) -> Result<Vec<(EntityId, Change)>, Error>`
//!   `World::history_scan(&self, attr: &str) -> Result<Vec<(EntityId, Change)>, Error>`
//! For every entity and edge ID defined at the view's canon, ascending by
//! EntityId, exactly the changes `View::history(id, attr)` returns for it at
//! that canon, in the same order. Empty histories contribute nothing; IDs not
//! defined at the canon are left out. Unknown attribute: `UnknownAttribute`.

use kala_core::*;
use proptest::prelude::*;

fn schema() -> Schema {
    let a = |n, t, k, c| AttrDef::new(n, t, k, c);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(a("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed())
        .attribute(a("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional())
        .attribute(
            a("parent_of", ValueType::Ref, Kind::State, Cardinality::Many)
                .directed()
                .inverse("child_of"),
        )
        .attribute(a("present", ValueType::Ref, Kind::State, Cardinality::Many))
        .attribute(a(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn rep_tick(t: i64) -> Correction {
    Correction::Replace {
        value: None,
        tick: Some(Tick(t)),
        op: None,
        precision: None,
    }
}

/// The oracle: loop over every candidate ID, ascending, keep non-empty
/// histories.
///
/// Inverse names (`child_of`) are `UnknownAttribute` for `history`, so their
/// oracle is the base attribute's loop with (source, Ref(target)) swapped to
/// (target, Ref(source)), ordered by target ascending. Pinned: the entity
/// sequence and, per entity, the multiset of changes (intra-entity order is
/// compared sorted by Debug form).
fn oracle(v: &View<'_>, attr: &str, ids: &[EntityId]) -> Vec<(EntityId, Change)> {
    if attr == "child_of" {
        let mut out = vec![];
        for (s, mut c) in oracle(v, "parent_of", ids) {
            if let Value::Ref(t) = c.value {
                c.value = Value::Ref(s);
                out.push((t, c));
            }
        }
        return norm(out);
    }
    let mut ids = ids.to_vec();
    ids.sort();
    ids.dedup();
    let mut out = vec![];
    for id in ids {
        for c in v.history(id, attr).unwrap_or_default() {
            out.push((id, c));
        }
    }
    out
}

fn norm(mut v: Vec<(EntityId, Change)>) -> Vec<(EntityId, Change)> {
    v.sort_by_key(|(e, c)| (*e, format!("{c:?}")));
    v
}

/// Scan result, normalised like the inverse oracle for `child_of` only.
fn scanned(v: &View<'_>, attr: &str) -> Vec<(EntityId, Change)> {
    let got = v.history_scan(attr).unwrap();
    if attr == "child_of" { norm(got) } else { got }
}

/// Every entity plus every edge between any pair of them for the given attrs.
fn candidates(w: &World, ents: &[EntityId], attrs: &[&str]) -> Vec<EntityId> {
    let mut ids = ents.to_vec();
    for &s in ents {
        for &t in ents {
            for a in attrs {
                if let Some(e) = w.edge_id(s, a, t) {
                    ids.push(e.into());
                }
            }
        }
    }
    ids
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

fn all(f: &Fx) -> Vec<EntityId> {
    vec![f.greg, f.edwin, f.mara, f.varrow, f.ashford]
}

const ATTRS: [&str; 6] = [
    "located_in",
    "ally_of",
    "parent_of",
    "child_of",
    "present",
    "disguised",
];

fn check_all_attrs(f: &Fx, v: &View<'_>) {
    let ids = candidates(&f.w, &all(f), &ATTRS);
    for a in ATTRS.iter().chain(&["name"]) {
        assert_eq!(scanned(v, a), oracle(v, a, &ids), "attr {a}");
    }
}

#[test]
fn scan_is_ascending_by_entity_then_history_order() {
    let mut f = fx();
    // written in scrambled order
    f.w.assert(f.mara, "located_in", r(f.varrow), tk(5))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.ashford), tk(601))
        .unwrap();
    let got = f.w.history_scan("located_in").unwrap();
    let ids: Vec<_> = got.iter().map(|(e, _)| *e).collect();
    assert_eq!(ids, vec![f.greg, f.greg, f.greg, f.mara]);
    let want: Vec<_> = f.w.history(f.greg, "located_in").unwrap();
    assert_eq!(
        got.iter()
            .take(3)
            .map(|(_, c)| c.clone())
            .collect::<Vec<_>>(),
        want
    );
    assert_eq!(
        got,
        oracle(&f.w.at_canon(f.w.latest_tx()), "located_in", &all(&f))
    );
}

#[test]
fn empty_world_and_empty_attribute_give_empty_vec() {
    let f = fx();
    assert_eq!(f.w.history_scan("located_in").unwrap(), vec![]);
    assert_eq!(f.w.history_scan("child_of").unwrap(), vec![]);
}

#[test]
fn unknown_attribute_is_an_error() {
    let f = fx();
    assert!(matches!(
        f.w.history_scan("nope"),
        Err(Error::UnknownAttribute(a)) if a == "nope"
    ));
    assert!(matches!(
        f.w.at_canon(f.w.latest_tx()).history_scan("nope"),
        Err(Error::UnknownAttribute(_))
    ));
}

#[test]
fn bidirectional_matches_history_per_id() {
    let mut f = fx();
    f.w.assert(f.mara, "ally_of", r(f.edwin), tk(-208)).unwrap();
    f.w.retract(f.edwin, "ally_of", r(f.mara), tk(-100))
        .unwrap();
    f.w.assert(f.edwin, "ally_of", r(f.greg), tk(0)).unwrap();
    let v = f.w.at_canon(f.w.latest_tx());
    let ids = candidates(&f.w, &all(&f), &ATTRS);
    let got = v.history_scan("ally_of").unwrap();
    assert_eq!(got, oracle(&v, "ally_of", &ids));
    assert!(!got.is_empty());
    // both orientations are read as history reads them
    assert!(got.iter().any(|(e, _)| *e == f.mara));
    assert!(got.iter().any(|(e, _)| *e == f.edwin));
}

#[test]
fn inverse_name_matches_history_per_id() {
    let mut f = fx();
    f.w.assert(f.greg, "parent_of", r(f.mara), tk(0)).unwrap();
    f.w.assert(f.greg, "parent_of", r(f.edwin), tk(3)).unwrap();
    f.w.retract(f.greg, "parent_of", r(f.mara), tk(5)).unwrap();
    let v = f.w.at_canon(f.w.latest_tx());
    let ids = candidates(&f.w, &all(&f), &ATTRS);
    let got = scanned(&v, "child_of");
    assert_eq!(got, oracle(&v, "child_of", &ids));
    assert!(!got.is_empty());
    assert_eq!(got.iter().map(|(e, _)| *e).min(), Some(f.edwin));
    assert!(got.iter().all(|(_, c)| c.value == r(f.greg)));
    // raw scan is ascending by target
    let raw: Vec<_> = v
        .history_scan("child_of")
        .unwrap()
        .iter()
        .map(|(e, _)| *e)
        .collect();
    assert!(raw.windows(2).all(|w| w[0] <= w[1]));
    check_all_attrs(&f, &v);
}

#[test]
fn edge_property_histories_are_included() {
    let mut f = fx();
    f.w.assert(f.edwin, "present", r(f.varrow), tk(-300))
        .unwrap();
    let edge: EntityId = f.w.edge_id(f.edwin, "present", f.varrow).unwrap().into();
    f.w.assert(edge, "disguised", Value::Bool(true), tk(-300))
        .unwrap();
    f.w.retract(edge, "disguised", Value::Bool(true), tk(-280))
        .unwrap();
    f.w.assert(edge, "disguised", Value::Bool(false), tk(-280))
        .unwrap();
    let got = f.w.history_scan("disguised").unwrap();
    assert_eq!(got.len(), 3);
    assert!(got.iter().all(|(e, _)| *e == edge));
    assert_eq!(
        got.iter().map(|(_, c)| c.clone()).collect::<Vec<_>>(),
        f.w.history(edge, "disguised").unwrap()
    );
}

#[test]
fn entity_defined_after_the_canon_is_left_out() {
    let mut f = fx();
    f.w.assert(f.greg, "name", Value::Str("Greg".into()), tk(0))
        .unwrap();
    let old = f.w.latest_tx();
    let late = f.w.create_entity("CHARACTER", Some("late")).unwrap();
    f.w.assert(late, "name", Value::Str("Late".into()), tk(0))
        .unwrap();
    let before = f.w.at_canon(old).history_scan("name").unwrap();
    assert_eq!(
        before.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
        vec![f.greg]
    );
    let after = f.w.history_scan("name").unwrap();
    assert_eq!(
        after.iter().map(|(e, _)| *e).collect::<Vec<_>>(),
        vec![f.greg, late]
    );
}

#[test]
fn retcon_is_honoured_per_canon() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-300))
        .unwrap();
    let ret =
        f.w.retract(f.greg, "located_in", r(f.varrow), tk(-250))
            .unwrap()
            .entries[0];
    f.w.assert(f.mara, "located_in", r(f.ashford), tk(1))
        .unwrap();
    let c0 = f.w.latest_tx();
    f.w.correct(ret, rep_tick(-280)).unwrap(); // move the retract
    let c1 = f.w.latest_tx();
    let gone =
        f.w.assert(f.edwin, "located_in", r(f.varrow), tk(7))
            .unwrap()
            .entries[0];
    f.w.correct(gone, Correction::Delete).unwrap();
    let c2 = f.w.latest_tx();
    let ids = all(&f);
    for c in [c0, c1, c2] {
        let v = f.w.at_canon(c);
        assert_eq!(
            v.history_scan("located_in").unwrap(),
            oracle(&v, "located_in", &ids)
        );
    }
    let ticks = |c| {
        f.w.at_canon(c)
            .history_scan("located_in")
            .unwrap()
            .into_iter()
            .filter(|(e, _)| *e == f.greg)
            .map(|(_, c)| c.tick.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(ticks(c0), vec![-300, -250]);
    assert_eq!(ticks(c1), vec![-300, -280]);
    assert_eq!(ticks(c2), vec![-300, -280]);
    // the deleted fact is absent at c2 (nothing for edwin), and the world's latest read matches c2
    assert!(
        f.w.at_canon(c2)
            .history_scan("located_in")
            .unwrap()
            .iter()
            .all(|(e, _)| *e != f.edwin)
    );
    assert_eq!(
        f.w.history_scan("located_in").unwrap(),
        f.w.at_canon(c2).history_scan("located_in").unwrap()
    );
}

// ------------------------------------------------------------- precision (c)

#[test]
fn precision_is_carried_per_change() {
    let mut f = fx();
    f.w.assert_with_precision(f.greg, "located_in", r(f.varrow), tk(-300), Precision::Year)
        .unwrap();
    f.w.retract_with_precision(
        f.greg,
        "located_in",
        r(f.varrow),
        tk(-250),
        Precision::Century,
    )
    .unwrap();
    f.w.assert(f.mara, "located_in", r(f.varrow), tk(0))
        .unwrap();
    let got = f.w.history_scan("located_in").unwrap();
    let p: Vec<_> = got.iter().map(|(e, c)| (*e, c.precision)).collect();
    assert_eq!(
        p,
        vec![
            (f.greg, Precision::Year),
            (f.greg, Precision::Century),
            (f.mara, Precision::Exact)
        ]
    );
}

#[test]
fn revised_fact_precision_survives_in_scan() {
    let mut f = fx();
    let a =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(-300))
            .unwrap()
            .entries[0];
    let c0 = f.w.latest_tx();
    // precision-only replace, then a tick move that keeps the current precision
    f.w.correct(
        a,
        Correction::Replace {
            value: None,
            tick: None,
            op: None,
            precision: Some(Precision::Decade),
        },
    )
    .unwrap();
    let c1 = f.w.latest_tx();
    f.w.correct(a, rep_tick(-310)).unwrap();
    let got = f.w.history_scan("located_in").unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(
        (got[0].1.tick, got[0].1.precision),
        (Tick(-310), Precision::Decade)
    );
    let old = f.w.at_canon(c0).history_scan("located_in").unwrap();
    assert_eq!(old[0].1.precision, Precision::Exact);
    let mid = f.w.at_canon(c1).history_scan("located_in").unwrap();
    assert_eq!(
        (mid[0].1.tick, mid[0].1.precision),
        (Tick(-300), Precision::Decade)
    );
}

// ------------------------------------------------------------- proptest

#[derive(Clone, Debug)]
enum Step {
    Assert {
        attr: usize,
        a: usize,
        b: usize,
        t: i64,
        retract: bool,
    },
    Prop {
        a: usize,
        b: usize,
        t: i64,
        on: bool,
    },
    Delete(usize),
    Move(usize, i64),
    Late,
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => (0usize..3, 0usize..5, 0usize..5, -6i64..6, any::<bool>())
            .prop_map(|(attr, a, b, t, retract)| Step::Assert { attr, a, b, t, retract }),
        2 => (0usize..5, 0usize..5, -6i64..6, any::<bool>())
            .prop_map(|(a, b, t, on)| Step::Prop { a, b, t, on }),
        1 => (0usize..40).prop_map(Step::Delete),
        1 => (0usize..40, -6i64..6).prop_map(|(i, t)| Step::Move(i, t)),
        1 => Just(Step::Late),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn scan_equals_history_loop_at_every_canon(steps in proptest::collection::vec(step(), 0..30)) {
        let mut f = fx();
        let mut ents = all(&f);
        let attrs = ["located_in", "ally_of", "parent_of"];
        for s in steps {
            match s {
                Step::Assert { attr, a, b, t, retract } => {
                    let (a, b) = (ents[a % ents.len()], ents[b % ents.len()]);
                    let _ = if retract {
                        f.w.retract(a, attrs[attr], r(b), tk(t))
                    } else {
                        f.w.assert(a, attrs[attr], r(b), tk(t))
                    };
                }
                Step::Prop { a, b, t, on } => {
                    let (a, b) = (ents[a % ents.len()], ents[b % ents.len()]);
                    let _ = f.w.assert(a, "present", r(b), tk(t));
                    if let Some(e) = f.w.edge_id(a, "present", b) {
                        let _ = if on {
                            f.w.assert(e.into(), "disguised", Value::Bool(true), tk(t))
                        } else {
                            f.w.retract(e.into(), "disguised", Value::Bool(true), tk(t))
                        };
                    }
                }
                Step::Delete(i) => {
                    let n = f.w.entries().count();
                    if n > 0 {
                        let id = f.w.entries().nth(i % n).unwrap().id;
                        let _ = f.w.correct(id, Correction::Delete);
                    }
                }
                Step::Move(i, t) => {
                    let n = f.w.entries().count();
                    if n > 0 {
                        let id = f.w.entries().nth(i % n).unwrap().id;
                        let _ = f.w.correct(id, rep_tick(t));
                    }
                }
                Step::Late => {
                    ents.push(f.w.create_entity("CHARACTER", None).unwrap());
                }
            }
        }
        let ids = candidates(&f.w, &ents, &ATTRS);
        for tx in 0..=f.w.latest_tx().0 {
            let v = f.w.at_canon(TxId(tx));
            for a in ATTRS {
                prop_assert_eq!(scanned(&v, a), oracle(&v, a, &ids), "attr {} canon {}", a, tx);
            }
        }
    }
}
