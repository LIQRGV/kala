//! Slice 6, item 0(c): `World::attr_stats` (RED step; written from the contract).
//!
//! API under test (new):
//!   `World::attr_stats(&self, attr: &str) -> Result<AttrStats, Error>`
//!   `pub struct AttrStats { pub entities: usize, pub values: usize, pub pairs: usize }`
//!   (`Copy`, `Debug`, `PartialEq`, `Eq`; re-exported at the crate root)
//!
//! Over every Fact and Revised entry ever committed for the attribute (any
//! op, all canons; Deleted markers ignored), counted in stored orientation
//! (bidirectional: normalised (min, max) by entity ID): `entities` = distinct
//! entities, `values` = distinct values (by encoding), `pairs` = distinct
//! (entity, value). Never shrink. An inverse name reports the base
//! attribute's stats. Unknown attribute: `UnknownAttribute`. Maintained on
//! apply and on replay (reopen).

use kala_core::log::EntryBody;
use kala_core::*;
use proptest::prelude::*;
use std::collections::BTreeSet;

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
        .attribute(a("bought", ValueType::Ref, Kind::Event, Cardinality::Many).directed())
}

fn st(entities: usize, values: usize, pairs: usize) -> AttrStats {
    AttrStats {
        entities,
        values,
        pairs,
    }
}

fn tk(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
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

// ------------------------------------------------------------- hand cases

#[test]
fn empty_attribute_has_zero_stats() {
    let f = fx();
    for a in ["located_in", "ally_of", "child_of", "name", "bio", "bought"] {
        assert_eq!(f.w.attr_stats(a).unwrap(), st(0, 0, 0), "{a}");
    }
}

#[test]
fn unknown_attribute_is_an_error() {
    let f = fx();
    assert!(matches!(
        f.w.attr_stats("nope"),
        Err(Error::UnknownAttribute(a)) if a == "nope"
    ));
}

#[test]
fn stats_is_copy_debug_eq() {
    let f = fx();
    let a = f.w.attr_stats("name").unwrap();
    let b = a; // Copy
    assert_eq!(a, b);
    assert!(!format!("{a:?}").is_empty());
    assert_eq!(a.entities, 0);
    assert_eq!(a.values, 0);
    assert_eq!(a.pairs, 0);
}

#[test]
fn counts_distinct_entities_values_and_pairs() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 1, 1));
    // same value, other entity
    f.w.assert(f.mara, "located_in", r(f.varrow), tk(0))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(2, 1, 2));
    // other value, first entity
    f.w.assert(f.greg, "located_in", r(f.ashford), tk(10))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(2, 2, 3));
    // a third entity with a third value
    f.w.assert(f.edwin, "located_in", r(f.mara), tk(10))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(3, 3, 4));
}

#[test]
fn retracts_and_reasserts_of_the_same_pair_add_nothing() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(10))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(20))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 1, 1));
}

#[test]
fn retract_of_a_pair_counts_even_when_stats_never_shrink() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    f.w.retract(f.greg, "located_in", r(f.varrow), tk(10))
        .unwrap();
    // nothing holds at the latest tick any more, the estimate stays
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 1, 1));
}

#[test]
fn out_of_order_prequel_counts_like_any_other_write() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.ashford), tk(601))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(-400))
        .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 2, 2));
}

#[test]
fn noop_and_failed_writes_are_not_counted() {
    let mut f = fx();
    f.w.assert(f.greg, "age", Value::I64(30), tk(0)).unwrap();
    let before = f.w.attr_stats("age").unwrap();
    // asserting a value that already holds at that tick: 0 changed
    let res = f.w.assert(f.greg, "age", Value::I64(30), tk(5)).unwrap();
    assert_eq!(res.changed, 0);
    // retracting a value that is not asserted at that tick: 0 changed
    let res = f.w.retract(f.edwin, "age", Value::I64(99), tk(5)).unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(f.w.attr_stats("age").unwrap(), before);
    // same-tick conflict: an error, nothing stored
    assert!(f.w.retract(f.greg, "age", Value::I64(30), tk(0)).is_err());
    // wrong type: an error
    assert!(f.w.assert(f.greg, "age", Value::Bool(true), tk(0)).is_err());
    assert_eq!(f.w.attr_stats("age").unwrap(), before);
}

#[test]
fn dropped_transaction_is_not_counted_and_committed_one_is() {
    let mut f = fx();
    {
        let mut txn = f.w.begin();
        txn.assert(f.greg, "age", Value::I64(1), tk(0)).unwrap();
        drop(txn);
    }
    assert_eq!(f.w.attr_stats("age").unwrap(), st(0, 0, 0));
    {
        let mut txn = f.w.begin();
        txn.assert(f.greg, "age", Value::I64(1), tk(0)).unwrap();
        txn.assert(f.edwin, "age", Value::I64(1), tk(0)).unwrap();
        txn.commit().unwrap();
    }
    assert_eq!(f.w.attr_stats("age").unwrap(), st(2, 1, 2));
}

#[test]
fn failed_transaction_write_counts_nothing() {
    let mut f = fx();
    let mut txn = f.w.begin();
    txn.assert(f.greg, "age", Value::I64(1), tk(0)).unwrap();
    assert!(txn.assert(f.greg, "age", Value::Bool(true), tk(0)).is_err());
    drop(txn);
    assert_eq!(f.w.attr_stats("age").unwrap(), st(0, 0, 0));
}

#[test]
fn f64_values_are_distinct_by_encoding_so_minus_zero_is_zero() {
    let mut f = fx();
    f.w.assert(f.greg, "height", Value::F64(0.0), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "height", Value::F64(-0.0), tk(0))
        .unwrap();
    f.w.assert(f.mara, "height", Value::F64(1.5), tk(0))
        .unwrap();
    assert_eq!(f.w.attr_stats("height").unwrap(), st(3, 2, 3));
}

#[test]
fn string_and_unindexed_attributes_count_too() {
    let mut f = fx();
    f.w.assert(f.greg, "name", Value::Str("Greg".into()), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "name", Value::Str("Greg".into()), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "name", Value::Str("greg".into()), tk(1))
        .unwrap();
    assert_eq!(
        f.w.attr_stats("name").unwrap(),
        st(2, 2, 3),
        "exact bytes: Greg and greg are different values"
    );
    let long = "x".repeat(4000);
    f.w.assert(f.greg, "bio", Value::Str(long.clone()), tk(0))
        .unwrap();
    f.w.assert(f.edwin, "bio", Value::Str(long), tk(0)).unwrap();
    f.w.assert(f.mara, "bio", Value::Str("short".into()), tk(0))
        .unwrap();
    assert_eq!(f.w.attr_stats("bio").unwrap(), st(3, 2, 3));
}

#[test]
fn stats_are_per_attribute() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
        .unwrap();
    f.w.assert(f.greg, "age", Value::I64(3), tk(0)).unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 1, 1));
    assert_eq!(f.w.attr_stats("age").unwrap(), st(1, 1, 1));
    assert_eq!(f.w.attr_stats("name").unwrap(), st(0, 0, 0));
}

#[test]
fn edge_property_facts_count_under_their_attribute() {
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    let e = EntityId::from(res.edges[0]);
    f.w.assert(e, "alive", Value::Bool(true), tk(0)).unwrap();
    assert_eq!(f.w.attr_stats("alive").unwrap(), st(1, 1, 1));
}

#[test]
fn bidirectional_counts_stored_orientation_once() {
    let mut f = fx();
    // edwin < mara by ID: stored as (edwin, mara)
    f.w.assert(f.mara, "ally_of", r(f.edwin), tk(0)).unwrap();
    assert_eq!(f.w.attr_stats("ally_of").unwrap(), st(1, 1, 1));
    // the other spelling of the same pair is a no-op
    f.w.assert(f.edwin, "ally_of", r(f.mara), tk(1)).unwrap();
    assert_eq!(f.w.attr_stats("ally_of").unwrap(), st(1, 1, 1));
    // a second pair sharing the min entity: one more value and pair
    f.w.assert(f.edwin, "ally_of", r(f.greg), tk(2)).unwrap();
    // greg < edwin so it is stored (greg, edwin): a new entity and new value
    assert_eq!(f.w.attr_stats("ally_of").unwrap(), st(2, 2, 2));
}

#[test]
fn bidirectional_self_pair() {
    let mut f = fx();
    f.w.assert(f.greg, "ally_of", r(f.greg), tk(0)).unwrap();
    assert_eq!(f.w.attr_stats("ally_of").unwrap(), st(1, 1, 1));
}

#[test]
fn inverse_name_reports_the_base_attribute_stats() {
    let mut f = fx();
    f.w.assert(f.greg, "parent_of", r(f.mara), tk(0)).unwrap();
    f.w.assert(f.greg, "parent_of", r(f.edwin), tk(0)).unwrap();
    f.w.assert(f.mara, "parent_of", r(f.edwin), tk(0)).unwrap();
    let base = f.w.attr_stats("parent_of").unwrap();
    assert_eq!(base, st(2, 2, 3), "stored orientation: parents as entities");
    assert_eq!(f.w.attr_stats("child_of").unwrap(), base);
}

#[test]
fn event_attribute_counts_asserts() {
    let mut f = fx();
    f.w.assert(f.greg, "bought", r(f.varrow), tk(0)).unwrap();
    f.w.assert(f.edwin, "bought", r(f.varrow), tk(0)).unwrap();
    assert_eq!(f.w.attr_stats("bought").unwrap(), st(2, 1, 2));
}

#[test]
fn correction_delete_does_not_shrink_stats() {
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    f.w.assert(f.mara, "located_in", r(f.ashford), tk(0))
        .unwrap();
    let before = f.w.attr_stats("located_in").unwrap();
    assert_eq!(before, st(2, 2, 2));
    f.w.correct(res.entries[0], Correction::Delete).unwrap();
    assert_eq!(
        f.w.attr_stats("located_in").unwrap(),
        before,
        "the deleted fact's Fact entry is still a committed entry; DELETED markers add nothing"
    );
}

#[test]
fn correction_replace_adds_the_revised_entry() {
    let mut f = fx();
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), tk(0))
            .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 1, 1));
    // value moves: the Revised entry carries a new value -> a new pair
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
    assert_eq!(f.w.attr_stats("located_in").unwrap(), st(1, 2, 2));
    // a tick move to an already counted pair adds nothing
    let res2 =
        f.w.assert(f.mara, "located_in", r(f.varrow), tk(5))
            .unwrap();
    let mid = f.w.attr_stats("located_in").unwrap();
    assert_eq!(mid, st(2, 2, 3));
    f.w.correct(
        res2.entries[0],
        Correction::Replace {
            value: None,
            tick: Some(Tick(-5)),
            op: None,
            precision: None,
        },
    )
    .unwrap();
    assert_eq!(f.w.attr_stats("located_in").unwrap(), mid);
}

#[test]
fn correction_replace_bidirectional_repairing_counts_stored_orientation() {
    let mut f = fx();
    let res = f.w.assert(f.edwin, "ally_of", r(f.mara), tk(0)).unwrap();
    f.w.correct(
        res.entries[0],
        Correction::Replace {
            value: Some(r(f.greg)),
            tick: None,
            op: None,
            precision: None,
        },
    )
    .unwrap();
    // original stored as (edwin, mara); revised pair {edwin, greg} stored (greg, edwin)
    assert_eq!(f.w.attr_stats("ally_of").unwrap(), st(2, 2, 2));
}

#[test]
fn counts_survive_reopen_of_a_file_backed_world() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let before;
    {
        let mut w = World::create(&path, schema()).unwrap();
        let greg = w.create_entity("CHARACTER", None).unwrap();
        let edwin = w.create_entity("CHARACTER", None).unwrap();
        let mara = w.create_entity("CHARACTER", None).unwrap();
        let varrow = w.create_entity("PLACE", None).unwrap();
        let ashford = w.create_entity("PLACE", None).unwrap();
        let res = w.assert(greg, "located_in", r(varrow), tk(0)).unwrap();
        w.assert(mara, "located_in", r(ashford), tk(3)).unwrap();
        w.retract(greg, "located_in", r(varrow), tk(9)).unwrap();
        w.assert(mara, "ally_of", r(edwin), tk(0)).unwrap();
        w.assert(greg, "parent_of", r(edwin), tk(0)).unwrap();
        w.assert(greg, "name", Value::Str("G".into()), tk(0))
            .unwrap();
        w.assert(greg, "bio", Value::Str("y".repeat(900)), tk(0))
            .unwrap();
        w.correct(res.entries[0], Correction::Delete).unwrap();
        w.assert(edwin, "age", Value::I64(4), tk(0)).unwrap();
        before = [
            "located_in",
            "ally_of",
            "parent_of",
            "child_of",
            "name",
            "bio",
            "age",
            "height",
        ]
        .map(|a| (a, w.attr_stats(a).unwrap()));
        assert_eq!(before[0].1, st(2, 2, 2));
    }
    let w = World::open(&path).unwrap();
    for (a, s) in before {
        assert_eq!(w.attr_stats(a).unwrap(), s, "{a} after reopen");
    }
    let ro = World::open_read_only(&path).unwrap();
    for (a, s) in before {
        assert_eq!(ro.attr_stats(a).unwrap(), s, "{a} after read-only open");
    }
}

#[test]
fn stats_keep_counting_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let (greg, edwin);
    {
        let mut w = World::create(&path, schema()).unwrap();
        greg = w.create_entity("CHARACTER", None).unwrap();
        edwin = w.create_entity("CHARACTER", None).unwrap();
        w.assert(greg, "age", Value::I64(1), tk(0)).unwrap();
    }
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.attr_stats("age").unwrap(), st(1, 1, 1));
    w.assert(greg, "age", Value::I64(1), tk(1)).unwrap(); // no-op
    assert_eq!(w.attr_stats("age").unwrap(), st(1, 1, 1));
    w.assert(edwin, "age", Value::I64(2), tk(1)).unwrap();
    assert_eq!(w.attr_stats("age").unwrap(), st(2, 2, 2));
}

// ---------------------------------------------------------- property test

const NCHAR: usize = 4;
const NPLACE: usize = 3;
const ATTRS: [&str; 8] = [
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
    /// An uncommitted transaction: must count nothing.
    Dropped {
        attr: usize,
        e: usize,
        v: usize,
    },
    Delete {
        pick: usize,
    },
    Replace {
        pick: usize,
        tick: Option<i64>,
        flip: bool,
        value: Option<usize>,
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
        10 => (0..ATTRS.len(), 0..NCHAR + NPLACE, 0usize..3, tick_strat(), any::<bool>())
            .prop_map(|(attr, e, v, tick, retract)| Step::Write { attr, e, v, tick, retract }),
        1 => (0..ATTRS.len(), 0..NCHAR + NPLACE, 0usize..3)
            .prop_map(|(attr, e, v)| Step::Dropped { attr, e, v }),
        2 => (0usize..64).prop_map(|pick| Step::Delete { pick }),
        3 => (0usize..64, proptest::option::of(tick_strat()), any::<bool>(), proptest::option::of(0usize..3))
            .prop_map(|(pick, tick, flip, value)| Step::Replace { pick, tick, flip, value }),
    ]
}

fn value_for(attr: usize, v: usize, ents: &[EntityId]) -> Value {
    match ATTRS[attr] {
        "located_in" => Value::Ref(ents[NCHAR + v % NPLACE]),
        "ally_of" | "parent_of" => Value::Ref(ents[v % NCHAR + (v % 2)]),
        "name" | "bio" => Value::Str(["a", "b\0c", "Édwin"][v].to_owned()),
        "age" => Value::I64([-1, 0, i64::MAX][v]),
        "height" => Value::F64([-1.5, 0.0, 2.25][v]),
        _ => Value::Bool(v.is_multiple_of(2)),
    }
}

/// A base fact entry and the index (into `ATTRS`) of its attribute.
fn pick_fact(w: &World, pick: usize) -> Option<(EntryId, usize)> {
    let facts: Vec<(EntryId, usize)> = w
        .entries()
        .filter_map(|e| match e.body {
            EntryBody::Fact { attr, .. } => {
                let name = w.attr_name(attr)?;
                Some((e.id, ATTRS.iter().position(|a| *a == name)?))
            }
            _ => None,
        })
        .collect();
    if facts.is_empty() {
        None
    } else {
        Some(facts[pick % facts.len()])
    }
}

/// Naive reference: scan every committed Fact / Revised entry of the
/// attribute (Deleted ignored), orientation normalised (min, max) for
/// bidirectional ref values.
fn model_stats(w: &World, attr: &str, bidi: bool) -> AttrStats {
    let mut ents = BTreeSet::new();
    let mut vals = BTreeSet::new();
    let mut pairs = BTreeSet::new();
    for info in w.entries() {
        let (entity, a, value) = match &info.body {
            EntryBody::Fact {
                entity,
                attr,
                value,
                ..
            }
            | EntryBody::Revised {
                entity,
                attr,
                value,
                ..
            } => (*entity, *attr, value.clone()),
            _ => continue,
        };
        if w.attr_name(a) != Some(attr) {
            continue;
        }
        let (entity, value) = match (&value, bidi) {
            (Value::Ref(o), true) if o.raw() < entity.raw() => (*o, Value::Ref(entity)),
            _ => (entity, value),
        };
        let enc = value.encode().unwrap();
        ents.insert(entity.raw());
        vals.insert(enc.clone());
        pairs.insert((entity.raw(), enc));
    }
    st(ents.len(), vals.len(), pairs.len())
}

fn all_stats(w: &World) -> Vec<AttrStats> {
    ATTRS
        .iter()
        .map(|a| w.attr_stats(a).unwrap())
        .collect::<Vec<_>>()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn attr_stats_match_naive_model_and_never_shrink(
        steps in proptest::collection::vec(step_strat(), 0..40),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.kala");
        let mut w = World::create(&path, schema()).unwrap();
        let mut ents = Vec::new();
        for _ in 0..NCHAR { ents.push(w.create_entity("CHARACTER", None).unwrap()); }
        for _ in 0..NPLACE { ents.push(w.create_entity("PLACE", None).unwrap()); }
        let mut prev = all_stats(&w);
        for s in &steps {
            match s {
                Step::Write { attr, e, v, tick, retract } => {
                    let val = value_for(*attr, *v, &ents);
                    let ent = ents[*e];
                    let _ = if *retract {
                        w.retract(ent, ATTRS[*attr], val, tk(*tick))
                    } else {
                        w.assert(ent, ATTRS[*attr], val, tk(*tick))
                    };
                }
                Step::Dropped { attr, e, v } => {
                    let val = value_for(*attr, *v, &ents);
                    let mut txn = w.begin();
                    let _ = txn.assert(ents[*e], ATTRS[*attr], val, tk(7));
                    drop(txn);
                }
                Step::Delete { pick } => {
                    if let Some((id, _)) = pick_fact(&w, *pick) {
                        let _ = w.correct(id, Correction::Delete);
                    }
                }
                Step::Replace { pick, tick, flip, value } => {
                    if let Some((id, a)) = pick_fact(&w, *pick) {
                        let value = value.map(|v| value_for(a, v, &ents));
                        let _ = w.correct(id, Correction::Replace {
                            value,
                            tick: tick.map(Tick),
                            op: flip.then_some(Op::Retract),
                            precision: None,
                        });
                    }
                }
            }
            let now = all_stats(&w);
            for (i, (p, n)) in prev.iter().zip(&now).enumerate() {
                prop_assert!(
                    n.entities >= p.entities && n.values >= p.values && n.pairs >= p.pairs,
                    "{} shrank: {p:?} -> {n:?} after {s:?}", ATTRS[i]
                );
                prop_assert!(n.pairs >= n.entities.max(n.values));
                prop_assert!(n.pairs <= n.entities * n.values);
            }
            prev = now;
        }
        for (i, a) in ATTRS.iter().enumerate() {
            let want = model_stats(&w, a, *a == "ally_of");
            prop_assert_eq!(prev[i], want, "{}", a);
        }
        prop_assert_eq!(
            w.attr_stats("child_of").unwrap(),
            model_stats(&w, "parent_of", false)
        );
        // replay gives the same numbers
        drop(w);
        let w2 = World::open(&path).unwrap();
        prop_assert_eq!(all_stats(&w2), prev);
        prop_assert_eq!(w2.attr_stats("child_of").unwrap(), w2.attr_stats("parent_of").unwrap());
    }
}
