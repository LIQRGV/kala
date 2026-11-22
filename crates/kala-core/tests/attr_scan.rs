//! Slice 6, item 0(a): `attr_scan` (RED step; written from the contract).
//!
//! API under test (new):
//!   `View::attr_scan(&self, attr: &str, t: Tick) -> Result<Vec<(EntityId, Value)>, Error>`
//!   `World::attr_scan(&self, attr: &str, t: Tick) -> Result<Vec<(EntityId, Value)>, Error>`
//! Every (entity, value) whose latest op with tick <= t (at the view's canon,
//! corrections honoured like `as_of`) is an assert; sorted ascending by
//! (entity, value), no duplicates. Bidirectional: both orientations, a self
//! pair once. Inverse name: swapped pairs. Unknown attr: `UnknownAttribute`.
//!
//! The sort order of values is the contract's `Value: Ord` (tested in
//! value_order.rs); this file sorts its oracle by `(entity, encoding bytes)`
//! so it does not depend on `Ord for Value`.

use kala_core::log::EntryBody;
use kala_core::*;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

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
        .attribute(a("name", ValueType::Str, Kind::State, Cardinality::One))
        .attribute(a("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(a("age", ValueType::I64, Kind::State, Cardinality::Many))
        .attribute(a("height", ValueType::F64, Kind::State, Cardinality::Many))
        .attribute(a("alive", ValueType::Bool, Kind::State, Cardinality::Many))
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

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

// ------------------------------------------------------------- hand cases

#[test]
fn assert_retract_reassert_follows_latest_op() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-300))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(-250))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-200))
        .unwrap();
    let want = vec![(f.greg, r(f.varrow))];
    for present in [-300, -260, -251, -200, -150, i64::MAX] {
        assert_eq!(
            f.w.attr_scan("located_in", Tick(present)).unwrap(),
            want,
            "present at {present}"
        );
    }
    for absent in [i64::MIN, -301, -250, -249, -220, -201] {
        assert_eq!(
            f.w.attr_scan("located_in", Tick(absent)).unwrap(),
            vec![],
            "absent at {absent}"
        );
    }
}

#[test]
fn retract_boundary_is_half_open() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(10))
        .unwrap();
    assert_eq!(f.w.attr_scan("located_in", Tick(9)).unwrap().len(), 1);
    assert_eq!(f.w.attr_scan("located_in", Tick(10)).unwrap().len(), 0);
    assert_eq!(f.w.attr_scan("located_in", Tick(0)).unwrap().len(), 1);
    assert_eq!(f.w.attr_scan("located_in", Tick(-1)).unwrap().len(), 0);
}

#[test]
fn extreme_ticks() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(30), tk(i64::MIN))
        .unwrap();
    f.w.assert(f.edwin, "age", Value::I64(40), tk(i64::MAX))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("age", Tick(i64::MIN)).unwrap(),
        vec![(f.greg, Value::I64(30))]
    );
    assert_eq!(
        f.w.attr_scan("age", Tick(i64::MAX)).unwrap(),
        vec![(f.greg, Value::I64(30)), (f.edwin, Value::I64(40))]
    );
}

#[test]
fn out_of_order_prequel_writes() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.ashford), tk(601))
        .unwrap();
    f.w.assert(f.mara, "located_in", r(f.varrow), tk(200))
        .unwrap();
    // prequel: written last, happened first
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-400))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(-300))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("located_in", Tick(-350)).unwrap(),
        vec![(f.greg, r(f.varrow))]
    );
    assert_eq!(f.w.attr_scan("located_in", Tick(-300)).unwrap(), vec![]);
    assert_eq!(
        f.w.attr_scan("located_in", Tick(250)).unwrap(),
        vec![(f.mara, r(f.varrow))]
    );
    assert_eq!(
        f.w.attr_scan("located_in", Tick(601)).unwrap(),
        vec![(f.greg, r(f.ashford)), (f.mara, r(f.varrow))]
    );
}

#[test]
fn overlapping_values_of_cardinality_one_are_both_listed() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(200))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.ashford), tk(601))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("located_in", Tick(700)).unwrap(),
        vec![(f.greg, r(f.varrow)), (f.greg, r(f.ashford))],
        "Varrow and Ashford (cardinality is a label); order is (entity, value)"
    );
}

#[test]
fn result_is_sorted_by_entity_then_value_without_duplicates() {
    let mut f = fx();
    // written in scrambled order
    f.w.assert(f.mara, "age", Value::I64(7), tk(0)).unwrap();
    f.w.assert(f.greg, "age", Value::I64(9), tk(0)).unwrap();
    f.w.assert(f.greg, "age", Value::I64(-3), tk(0)).unwrap();
    f.w.assert(f.edwin, "age", Value::I64(1), tk(0)).unwrap();
    f.w.assert(f.greg, "age", Value::I64(9), tk(5)).unwrap(); // duplicate assert: no-op
    assert_eq!(
        f.w.attr_scan("age", Tick(5)).unwrap(),
        vec![
            (f.greg, Value::I64(-3)),
            (f.greg, Value::I64(9)),
            (f.edwin, Value::I64(1)),
            (f.mara, Value::I64(7)),
        ]
    );
}

#[test]
fn value_types_scan_back_as_written() {
    let mut f = fx();
    f.w.assert(f.greg, "name", Value::Str("Greg".into()), tk(0))
        .unwrap();
    f.w.assert(f.greg, "height", Value::F64(1.5), tk(0))
        .unwrap();
    f.w.assert(f.greg, "alive", Value::Bool(true), tk(0))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("name", Tick(0)).unwrap(),
        vec![(f.greg, Value::Str("Greg".into()))]
    );
    assert_eq!(
        f.w.attr_scan("height", Tick(0)).unwrap(),
        vec![(f.greg, Value::F64(1.5))]
    );
    assert_eq!(
        f.w.attr_scan("alive", Tick(0)).unwrap(),
        vec![(f.greg, Value::Bool(true))]
    );
}

#[test]
fn unindexed_string_attribute_returns_full_values() {
    let mut f = fx();
    let long = "x".repeat(5000);
    f.w.assert(f.greg, "bio", Value::Str(long.clone()), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "bio", Value::Str("short".into()), tk(0))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("bio", Tick(0)).unwrap(),
        vec![
            (f.greg, Value::Str(long.clone())),
            (f.edwin, Value::Str("short".into()))
        ]
    );
    f.w.retract(f.greg, "bio", Value::Str(long), tk(3)).unwrap();
    assert_eq!(f.w.attr_scan("bio", Tick(3)).unwrap().len(), 1);
}

#[test]
fn unknown_attribute_is_an_error() {
    let f = fx();
    assert!(matches!(
        f.w.attr_scan("nope", Tick(0)),
        Err(Error::UnknownAttribute(a)) if a == "nope"
    ));
    assert!(matches!(
        f.w.at_canon(f.w.latest_tx()).attr_scan("nope", Tick(0)),
        Err(Error::UnknownAttribute(_))
    ));
}

#[test]
fn empty_world_and_empty_attribute_give_empty_vec() {
    let f = fx();
    assert_eq!(f.w.attr_scan("located_in", Tick(0)).unwrap(), vec![]);
    assert_eq!(f.w.attr_scan("ally_of", Tick(i64::MAX)).unwrap(), vec![]);
    assert_eq!(f.w.attr_scan("child_of", Tick(i64::MIN)).unwrap(), vec![]);
}

#[test]
fn bidirectional_returns_both_orientations() {
    let mut f = fx();
    f.w.assert(f.mara, "ally_of", r(f.edwin), tk(-208)).unwrap();
    let want = vec![(f.edwin, r(f.mara)), (f.mara, r(f.edwin))];
    assert_eq!(f.w.attr_scan("ally_of", Tick(-208)).unwrap(), want);
    assert_eq!(f.w.attr_scan("ally_of", Tick(0)).unwrap(), want);
    assert_eq!(f.w.attr_scan("ally_of", Tick(-209)).unwrap(), vec![]);
    f.w.retract(f.edwin, "ally_of", r(f.mara), tk(-100))
        .unwrap(); // other spelling ends it
    assert_eq!(f.w.attr_scan("ally_of", Tick(-101)).unwrap(), want);
    assert_eq!(f.w.attr_scan("ally_of", Tick(-100)).unwrap(), vec![]);
}

#[test]
fn bidirectional_self_pair_appears_once() {
    let mut f = fx();
    f.w.assert(f.greg, "ally_of", r(f.greg), tk(0)).unwrap();
    assert_eq!(
        f.w.attr_scan("ally_of", Tick(0)).unwrap(),
        vec![(f.greg, r(f.greg))]
    );
}

#[test]
fn bidirectional_with_several_partners_is_sorted() {
    let mut f = fx();
    f.w.assert(f.mara, "ally_of", r(f.greg), tk(0)).unwrap();
    f.w.assert(f.edwin, "ally_of", r(f.greg), tk(0)).unwrap();
    f.w.assert(f.edwin, "ally_of", r(f.mara), tk(0)).unwrap();
    assert_eq!(
        f.w.attr_scan("ally_of", Tick(0)).unwrap(),
        vec![
            (f.greg, r(f.edwin)),
            (f.greg, r(f.mara)),
            (f.edwin, r(f.greg)),
            (f.edwin, r(f.mara)),
            (f.mara, r(f.greg)),
            (f.mara, r(f.edwin)),
        ]
    );
}

#[test]
fn inverse_name_returns_swapped_pairs() {
    let mut f = fx();
    f.w.assert(f.greg, "parent_of", r(f.mara), tk(0)).unwrap();
    f.w.assert(f.greg, "parent_of", r(f.edwin), tk(0)).unwrap();
    assert_eq!(
        f.w.attr_scan("parent_of", Tick(0)).unwrap(),
        vec![(f.greg, r(f.edwin)), (f.greg, r(f.mara))]
    );
    assert_eq!(
        f.w.attr_scan("child_of", Tick(0)).unwrap(),
        vec![(f.edwin, r(f.greg)), (f.mara, r(f.greg))]
    );
    f.w.retract(f.greg, "parent_of", r(f.mara), tk(5)).unwrap();
    assert_eq!(
        f.w.attr_scan("child_of", Tick(5)).unwrap(),
        vec![(f.edwin, r(f.greg))]
    );
}

#[test]
fn directed_attribute_is_not_mirrored() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    assert_eq!(
        f.w.attr_scan("located_in", Tick(0)).unwrap(),
        vec![(f.greg, r(f.varrow))]
    );
}

#[test]
fn correction_delete_visible_at_latest_but_not_at_older_canon() {
    let mut f = fx();
    let before_any = f.w.latest_tx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(-300))
            .unwrap();
    let entry = res.entries[0];
    let tx_written = f.w.latest_tx();
    f.w.assert(f.mara, "located_in", r(f.ashford), tk(-300))
        .unwrap();
    f.w.correct(entry, Correction::Delete).unwrap();
    let tx_now = f.w.latest_tx();
    assert!(tx_now > tx_written);

    let latest = f.w.attr_scan("located_in", Tick(-300)).unwrap();
    assert_eq!(
        latest,
        vec![(f.mara, r(f.ashford))],
        "deleted fact is absent"
    );
    assert_eq!(
        f.w.at_canon(tx_now)
            .attr_scan("located_in", Tick(-300))
            .unwrap(),
        latest
    );
    // the canon right after the original write still sees it
    assert_eq!(
        f.w.at_canon(tx_written)
            .attr_scan("located_in", Tick(-300))
            .unwrap(),
        vec![(f.greg, r(f.varrow))]
    );
    assert_eq!(
        f.w.at_canon(before_any)
            .attr_scan("located_in", Tick(-300))
            .unwrap(),
        vec![]
    );
}

#[test]
fn correction_replace_moved_tick_visible_at_latest_but_not_older_canon() {
    let mut f = fx();
    let res = f.w.assert(f.edwin, "ally_of", r(f.mara), tk(-208)).unwrap();
    let old = f.w.latest_tx();
    f.w.correct(
        res.entries[0],
        Correction::Replace {
            value: None,
            tick: Some(Tick(-210)),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    let both = vec![(f.edwin, r(f.mara)), (f.mara, r(f.edwin))];
    // latest canon: holds from -210, not before
    assert_eq!(f.w.attr_scan("ally_of", Tick(-210)).unwrap(), both);
    assert_eq!(f.w.attr_scan("ally_of", Tick(-209)).unwrap(), both);
    assert_eq!(f.w.attr_scan("ally_of", Tick(-211)).unwrap(), vec![]);
    // older canon: still from -208
    let v = f.w.at_canon(old);
    assert_eq!(v.attr_scan("ally_of", Tick(-209)).unwrap(), vec![]);
    assert_eq!(v.attr_scan("ally_of", Tick(-208)).unwrap(), both);
}

#[test]
fn correction_replace_same_tick_op_flip_ends_the_value() {
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    let old = f.w.latest_tx();
    f.w.correct(
        res.entries[0],
        Correction::Replace {
            value: None,
            tick: None,
            op: Some(Op::Retract),
            precision: None,
        },
    )
    .unwrap();
    assert_eq!(f.w.attr_scan("located_in", Tick(5)).unwrap(), vec![]);
    assert_eq!(
        f.w.at_canon(old).attr_scan("located_in", Tick(5)).unwrap(),
        vec![(f.greg, r(f.varrow))]
    );
}

#[test]
fn correction_replace_value_moves_pair() {
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    f.w.correct(
        res.entries[0],
        Correction::Replace {
            value: Some(r(f.ashford)),
            tick: None,
            op: None,
            precision: None,
        },
    )
    .unwrap();
    assert_eq!(
        f.w.attr_scan("located_in", Tick(0)).unwrap(),
        vec![(f.greg, r(f.ashford))]
    );
}

#[test]
fn view_and_world_agree_at_latest() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "ally_of", r(f.mara), tk(0)).unwrap();
    for a in ["located_in", "ally_of", "child_of"] {
        for t in [-1, 0, 1] {
            assert_eq!(
                f.w.attr_scan(a, Tick(t)).unwrap(),
                f.w.at_canon(f.w.latest_tx()).attr_scan(a, Tick(t)).unwrap()
            );
        }
    }
}

#[test]
fn canon_zero_sees_nothing() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    assert_eq!(
        f.w.at_canon(TxId(0))
            .attr_scan("located_in", Tick(0))
            .unwrap(),
        vec![]
    );
}

#[test]
fn edge_property_attribute_on_edge_entity_is_scanned_too() {
    // facts whose entity is an edge ID are ordinary pairs (entity = the edge ID)
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    let e = EntityId::from(res.edges[0]);
    f.w.assert(e, "alive", Value::Bool(true), tk(0)).unwrap();
    assert_eq!(
        f.w.attr_scan("alive", Tick(0)).unwrap(),
        vec![(e, Value::Bool(true))]
    );
}

// ---------------------------------------------------------- property test

const NCHAR: usize = 4;
const NPLACE: usize = 3;
const REF_ATTRS: [&str; 3] = ["located_in", "ally_of", "parent_of"];
const ALL_ATTRS: [&str; 8] = [
    "located_in",
    "ally_of",
    "parent_of",
    "name",
    "bio",
    "age",
    "height",
    "alive",
];

#[derive(Clone, Debug)]
enum Step {
    Write {
        attr: usize,
        e: usize,
        v: usize,
        tick: i64,
        retract: bool,
    },
    Delete {
        pick: usize,
    },
    Replace {
        pick: usize,
        tick: Option<i64>,
        flip: bool,
    },
}

fn tick_strat() -> impl Strategy<Value = i64> {
    prop_oneof![
        8 => -6i64..6,
        1 => Just(i64::MIN),
        1 => Just(i64::MAX),
    ]
}

fn step_strat() -> impl Strategy<Value = Step> {
    prop_oneof![
        10 => (0..ALL_ATTRS.len(), 0..NCHAR + NPLACE, 0usize..3, tick_strat(), any::<bool>())
            .prop_map(|(attr, e, v, tick, retract)| Step::Write { attr, e, v, tick, retract }),
        2 => (0usize..64).prop_map(|pick| Step::Delete { pick }),
        3 => (0usize..64, proptest::option::of(tick_strat()), any::<bool>())
            .prop_map(|(pick, tick, flip)| Step::Replace { pick, tick, flip }),
    ]
}

fn pool(w: &mut World) -> Vec<EntityId> {
    let mut v = Vec::new();
    for _ in 0..NCHAR {
        v.push(w.create_entity("CHARACTER", None).unwrap());
    }
    for _ in 0..NPLACE {
        v.push(w.create_entity("PLACE", None).unwrap());
    }
    v
}

/// The value written for `attr` given selector `v` (0..3) and writer `e`.
fn value_for(attr: usize, v: usize, ents: &[EntityId]) -> Value {
    match ALL_ATTRS[attr] {
        "located_in" => Value::Ref(ents[NCHAR + v % NPLACE]),
        "ally_of" | "parent_of" => Value::Ref(ents[v % NCHAR + (v % 2)]),
        "name" | "bio" => Value::Str(["a", "b\0c", "Édwin"][v].to_owned()),
        "age" => Value::I64([-1, 0, i64::MAX][v]),
        "height" => Value::F64([-1.5, 0.0, 2.25][v]),
        _ => Value::Bool(v.is_multiple_of(2)),
    }
}

fn run_steps(w: &mut World, ents: &[EntityId], steps: &[Step]) {
    for s in steps {
        match s {
            Step::Write {
                attr,
                e,
                v,
                tick,
                retract,
            } => {
                let val = value_for(*attr, *v, ents);
                let ent = ents[*e % (NCHAR + NPLACE)];
                let name = ALL_ATTRS[*attr];
                // errors (same-tick conflict ...) are fine: the model reads the log
                let _ = if *retract {
                    w.retract(ent, name, val, tk(*tick))
                } else {
                    w.assert(ent, name, val, tk(*tick))
                };
            }
            Step::Delete { pick } => {
                if let Some(id) = pick_fact(w, *pick) {
                    let _ = w.correct(id, Correction::Delete);
                }
            }
            Step::Replace { pick, tick, flip } => {
                if let Some(id) = pick_fact(w, *pick) {
                    let op = if *flip { Some(Op::Retract) } else { None };
                    let _ = w.correct(
                        id,
                        Correction::Replace {
                            value: None,
                            tick: tick.map(Tick),
                            op,
                            precision: None,
                        },
                    );
                }
            }
        }
    }
}

fn pick_fact(w: &World, pick: usize) -> Option<EntryId> {
    let facts: Vec<EntryId> = w
        .entries()
        .filter(|e| matches!(e.body, EntryBody::Fact { .. }))
        .map(|e| e.id)
        .collect();
    if facts.is_empty() {
        None
    } else {
        Some(facts[pick % facts.len()])
    }
}

/// Naive model read straight from the committed log: the current entry of a
/// key `(entity, attr, value, tick)` at canon N is the one with the highest
/// tx <= N (a DELETED marker empties the key); a pair holds at T if the
/// latest-tick current entry with tick <= T is an assert.
fn model_scan(w: &World, attr: &str, t: i64, canon: u64) -> Vec<(EntityId, Value)> {
    let (base, swap) = match attr {
        "child_of" => ("parent_of", true),
        a => (a, false),
    };
    let bidi = base == "ally_of";
    // key -> (tx, Option<op>) newest wins
    type Key = (EntityId, Vec<u8>, i64);
    let mut cur: BTreeMap<Key, (u64, Option<Op>)> = BTreeMap::new();
    let mut vals: BTreeMap<Vec<u8>, Value> = BTreeMap::new();
    for info in w.entries() {
        if info.tx.0 > canon {
            continue;
        }
        let (entity, a, value, tick, op) = match &info.body {
            EntryBody::Fact {
                entity,
                attr,
                value,
                tick,
                op,
                ..
            }
            | EntryBody::Revised {
                entity,
                attr,
                value,
                tick,
                op,
                ..
            } => (*entity, *attr, value.clone(), *tick, Some(*op)),
            EntryBody::Deleted {
                entity,
                attr,
                value,
                tick,
                ..
            } => (*entity, *attr, value.clone(), *tick, None),
            _ => continue,
        };
        if w.attr_name(a) != Some(base) {
            continue;
        }
        let enc = value.encode().unwrap();
        vals.insert(enc.clone(), value);
        let k = (entity, enc, tick.0);
        match cur.get(&k) {
            Some((tx, _)) if *tx > info.tx.0 => {}
            // within one tx the later entry (Revised after Deleted) wins
            _ => {
                cur.insert(k, (info.tx.0, op));
            }
        }
    }
    // group by (entity, value): latest tick <= t
    let mut best: BTreeMap<(EntityId, Vec<u8>), (i64, Op)> = BTreeMap::new();
    for ((e, enc, tick), (_, op)) in cur {
        let Some(op) = op else { continue };
        if tick > t {
            continue;
        }
        let slot = best.entry((e, enc)).or_insert((tick, op));
        if tick >= slot.0 {
            *slot = (tick, op);
        }
    }
    let mut out: BTreeSet<(u64, Vec<u8>)> = BTreeSet::new();
    let mut res: BTreeMap<(u64, Vec<u8>), (EntityId, Value)> = BTreeMap::new();
    for ((e, enc), (_, op)) in best {
        if op != Op::Assert {
            continue;
        }
        let v = vals[&enc].clone();
        let mut pairs = vec![(e, v.clone())];
        if let Value::Ref(o) = v {
            if bidi {
                pairs.push((o, Value::Ref(e)));
            }
            if swap {
                pairs = vec![(o, Value::Ref(e))];
            }
        }
        for (pe, pv) in pairs {
            let key = (pe.raw(), pv.encode().unwrap());
            out.insert(key.clone());
            res.insert(key, (pe, pv));
        }
    }
    out.into_iter().map(|k| res[&k].clone()).collect()
}

fn same_pairs(a: &[(EntityId, Value)], b: &[(EntityId, Value)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.0 == y.0 && x.1.encode().unwrap() == y.1.encode().unwrap())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn attr_scan_matches_naive_model_and_union_of_as_of(
        steps in proptest::collection::vec(step_strat(), 0..40),
        ticks in proptest::collection::vec(tick_strat(), 1..5),
        canons in proptest::collection::vec(0u64..60, 1..4),
    ) {
        let mut w = World::in_memory(schema()).unwrap();
        let ents = pool(&mut w);
        run_steps(&mut w, &ents, &steps);
        let latest = w.latest_tx().0;
        let mut cs = canons.clone();
        cs.push(latest);
        for c in cs {
            let view = w.at_canon(TxId(c));
            let canon = view_canon(&w, c);
            for &t in &ticks {
                for attr in ALL_ATTRS.iter().copied().chain(["child_of"]) {
                    let got = view.attr_scan(attr, Tick(t)).unwrap();
                    let want = model_scan(&w, attr, t, canon);
                    prop_assert!(
                        same_pairs(&got, &want),
                        "attr {attr} t {t} canon {canon}: got {got:?} want {want:?}"
                    );
                    // strictly ascending by (entity, encoding): sorted, unique
                    for pair in got.windows(2) {
                        let a = (pair[0].0.raw(), pair[0].1.encode().unwrap());
                        let b = (pair[1].0.raw(), pair[1].1.encode().unwrap());
                        prop_assert!(a < b, "not strictly ascending: {got:?}");
                    }
                    // independent of the log: union over entities of as_of
                    if attr != "child_of" {
                        let mut un: Vec<(EntityId, Value)> = Vec::new();
                        for &e in &ents {
                            // an entity read before its DefineEntity tx is
                            // UnknownEntity (slice-6 item 00b): no values
                            let vals = match view.as_of(e, attr, Tick(t)) {
                                Ok(vs) => vs,
                                Err(Error::UnknownEntity(x)) if x == e => Vec::new(),
                                Err(other) => panic!("as_of({e:?},{attr}): {other:?}"),
                            };
                            for v in vals {
                                un.push((e, v));
                            }
                        }
                        un.sort_by_key(|(e, v)| (e.raw(), v.encode().unwrap()));
                        un.dedup_by_key(|(e, v)| (e.raw(), v.encode().unwrap()));
                        prop_assert!(
                            same_pairs(&got, &un),
                            "attr {attr} t {t} canon {canon}: scan {got:?} != as_of union {un:?}"
                        );
                    }
                }
            }
            if c == latest {
                // the world convenience equals the latest view
                for attr in REF_ATTRS {
                    prop_assert_eq!(
                        w.attr_scan(attr, Tick(ticks[0])).unwrap(),
                        view.attr_scan(attr, Tick(ticks[0])).unwrap()
                    );
                }
            }
        }
    }
}

/// `at_canon` clamps to the latest tx; the model must use the same canon.
fn view_canon(w: &World, c: u64) -> u64 {
    c.min(w.latest_tx().0)
}
