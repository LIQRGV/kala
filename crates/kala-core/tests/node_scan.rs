//! Slice 4, item 4: NodeScan by type (docs/slice-4-decisions.md "Item 4"; CLAUDE.md
//! "Entity IDs and node types"). RED until the implementation lands: this file
//! fails to COMPILE (no `entities_of_type`).
//!
//! API expected (crate root):
//!
//! ```text
//! World::entities_of_type(&self, node_type: &str)
//!     -> Result<impl Iterator<Item = EntityId> + '_, Error>
//! View::entities_of_type(&self, node_type: &str)       // same, at the view's canon
//! ```
//!
//! Contract: every entity of that type, ascending by sequence number (= ascending
//! EntityId), never another type, never an edge; entities with no facts are listed;
//! a view lists only entities whose DefineEntity tx <= its canon; unknown type ->
//! `Error::UnknownNodeType(name)`; a declared type with no entities -> empty.
//! The "extra labels" pattern needs no new API: a string attribute `label`,
//! cardinality many, read with `lookup("label", "Deity", t)`.

use kala_core::*;
use proptest::prelude::*;
use proptest::test_runner::{RngAlgorithm, RngSeed};

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .node_type("GHOST") // declared, never instantiated
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "label",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "knows",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
}

fn scan(w: &World, ty: &str) -> Vec<EntityId> {
    w.entities_of_type(ty).unwrap().collect()
}

fn scan_at(w: &World, canon: TxId, ty: &str) -> Vec<EntityId> {
    w.at_canon(canon).entities_of_type(ty).unwrap().collect()
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

fn ascending(v: &[EntityId]) -> bool {
    v.windows(2).all(|p| p[0] < p[1] && p[0].seq() < p[1].seq())
}

// ---------------------------------------------------------------- basics

#[test]
fn lists_every_entity_of_the_type_in_ascending_sequence_order() {
    let mut w = World::in_memory(schema()).unwrap();
    let ids: Vec<_> = (0..12)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let got = scan(&w, "CHARACTER");
    assert_eq!(got, ids);
    assert!(ascending(&got));
    assert_eq!(
        got.iter().map(|e| e.seq()).collect::<Vec<_>>(),
        (1..=12).collect::<Vec<u64>>()
    );
}

#[test]
fn entities_with_no_facts_are_listed() {
    let mut w = World::in_memory(schema()).unwrap();
    let bare = w.create_entity("PLACE", None).unwrap();
    let handled = w.create_entity("PLACE", Some("varrow")).unwrap();
    assert!(w.history(bare, "name").unwrap().is_empty());
    assert_eq!(scan(&w, "PLACE"), vec![bare, handled]);
}

#[test]
fn types_never_interleave() {
    let mut w = World::in_memory(schema()).unwrap();
    let (mut cs, mut ps, mut is) = (vec![], vec![], vec![]);
    for i in 0..30 {
        match i % 3 {
            0 => cs.push(w.create_entity("CHARACTER", None).unwrap()),
            1 => ps.push(w.create_entity("PLACE", None).unwrap()),
            _ => is.push(w.create_entity("ITEM", None).unwrap()),
        }
    }
    assert_eq!(scan(&w, "CHARACTER"), cs);
    assert_eq!(scan(&w, "PLACE"), ps);
    assert_eq!(scan(&w, "ITEM"), is);
    for (ty, ids) in [("CHARACTER", &cs), ("PLACE", &ps), ("ITEM", &is)] {
        for e in scan(&w, ty) {
            assert_eq!(e.node_type(), ids[0].node_type(), "{ty}");
        }
    }
    let total = scan(&w, "CHARACTER").len() + scan(&w, "PLACE").len() + scan(&w, "ITEM").len();
    assert_eq!(total, 30);
}

#[test]
fn edges_are_never_listed_even_after_many_ref_asserts() {
    let mut w = World::in_memory(schema()).unwrap();
    let cs: Vec<_> = (0..6)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let p = w.create_entity("PLACE", None).unwrap();
    let mut edges = 0;
    for (i, a) in cs.iter().enumerate() {
        for (j, b) in cs.iter().enumerate() {
            if i != j {
                edges += w
                    .assert(*a, "knows", Value::Ref(*b), t(0))
                    .unwrap()
                    .edges
                    .len();
            }
        }
        edges += w
            .assert(*a, "located_in", Value::Ref(p), t(0))
            .unwrap()
            .edges
            .len();
    }
    assert_eq!(edges, 6 * 5 + 6, "many edges were interned");
    for ty in ["CHARACTER", "PLACE", "ITEM", "GHOST"] {
        let got = scan(&w, ty);
        assert!(got.iter().all(|e| !e.is_edge()), "{ty}: {got:?}");
    }
    assert_eq!(scan(&w, "CHARACTER"), cs);
    assert_eq!(scan(&w, "PLACE"), vec![p]);
    assert!(scan(&w, "ITEM").is_empty());
}

#[test]
fn an_unknown_type_is_unknown_node_type() {
    let w = World::in_memory(schema()).unwrap();
    for name in ["NOPE", "", "character", "CHARACTER ", "label"] {
        match w.entities_of_type(name) {
            Err(Error::UnknownNodeType(n)) => assert_eq!(n, name),
            Err(e) => panic!("{name:?}: {e:?}"),
            Ok(_) => panic!("{name:?}: expected UnknownNodeType"),
        }
        match w.at_canon(w.latest_tx()).entities_of_type(name) {
            Err(Error::UnknownNodeType(n)) => assert_eq!(n, name),
            Err(e) => panic!("{name:?}: {e:?}"),
            Ok(_) => panic!("{name:?}: expected UnknownNodeType"),
        }
    }
}

#[test]
fn a_declared_type_with_no_entities_is_empty_not_an_error() {
    let mut w = World::in_memory(schema()).unwrap();
    assert!(scan(&w, "GHOST").is_empty());
    assert!(scan(&w, "CHARACTER").is_empty(), "fresh world");
    w.create_entity("CHARACTER", None).unwrap();
    assert!(scan(&w, "GHOST").is_empty());
    assert!(scan_at(&w, TxId(0), "GHOST").is_empty());
}

#[test]
fn the_iterator_is_lazy_and_reusable_scans_agree() {
    let mut w = World::in_memory(schema()).unwrap();
    for _ in 0..5 {
        w.create_entity("ITEM", None).unwrap();
    }
    let mut it = w.entities_of_type("ITEM").unwrap();
    let first = it.next().unwrap();
    assert_eq!(first.seq(), 1);
    assert_eq!(it.count(), 4);
    assert_eq!(scan(&w, "ITEM"), scan(&w, "ITEM"));
}

// ---------------------------------------------------------------- canon

#[test]
fn an_older_canon_excludes_later_created_entities() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let tx_a = w.latest_tx();
    let p = w.create_entity("PLACE", None).unwrap();
    let tx_p = w.latest_tx();
    let b = w.create_entity("CHARACTER", None).unwrap();
    let tx_b = w.latest_tx();
    let c = w.create_entity("CHARACTER", None).unwrap();
    let tx_c = w.latest_tx();
    assert!(scan_at(&w, TxId(0), "CHARACTER").is_empty());
    assert_eq!(scan_at(&w, tx_a, "CHARACTER"), vec![a]);
    assert!(scan_at(&w, tx_a, "PLACE").is_empty());
    assert_eq!(scan_at(&w, tx_p, "PLACE"), vec![p]);
    assert_eq!(scan_at(&w, tx_p, "CHARACTER"), vec![a]);
    assert_eq!(scan_at(&w, tx_b, "CHARACTER"), vec![a, b]);
    assert_eq!(scan_at(&w, tx_c, "CHARACTER"), vec![a, b, c]);
    // a canon past the latest is the latest
    assert_eq!(scan_at(&w, TxId(tx_c.0 + 100), "CHARACTER"), vec![a, b, c]);
    assert_eq!(scan(&w, "CHARACTER"), vec![a, b, c]);
    // the main-index view agrees
    assert_eq!(
        w.at_canon_main(tx_b)
            .entities_of_type("CHARACTER")
            .unwrap()
            .collect::<Vec<_>>(),
        vec![a, b]
    );
}

#[test]
fn a_view_pinned_before_a_create_keeps_its_answer() {
    let mut w = World::in_memory(schema()).unwrap();
    w.create_entity("CHARACTER", None).unwrap();
    let pinned = w.latest_tx();
    let before = scan_at(&w, pinned, "CHARACTER");
    w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(scan_at(&w, pinned, "CHARACTER"), before);
    assert_eq!(scan(&w, "CHARACTER").len(), 2);
}

#[test]
fn facts_and_retractions_do_not_change_which_entities_exist() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let b = w.create_entity("CHARACTER", None).unwrap();
    w.assert(a, "name", s("Edwin"), t(0)).unwrap();
    w.retract(a, "name", s("Edwin"), t(5)).unwrap();
    let r = w.assert(b, "name", s("Mara"), t(0)).unwrap();
    w.correct(r.entries[0], Correction::Delete).unwrap();
    assert_eq!(scan(&w, "CHARACTER"), vec![a, b]);
}

// ---------------------------------------------------------------- transactions

#[test]
fn entities_created_in_a_committed_multi_op_txn_are_listed_together() {
    let mut w = World::in_memory(schema()).unwrap();
    let first = w.create_entity("CHARACTER", None).unwrap();
    let before_tx = w.latest_tx();
    let mut txn = w.begin();
    let c1 = txn.create_entity("CHARACTER", Some("c1")).unwrap();
    let p1 = txn.create_entity("PLACE", None).unwrap();
    let c2 = txn.create_entity("CHARACTER", None).unwrap();
    txn.assert(c1, "name", s("Edwin"), t(0)).unwrap();
    txn.assert(c1, "located_in", Value::Ref(p1), t(0)).unwrap();
    txn.assert(c2, "knows", Value::Ref(c1), t(0)).unwrap();
    txn.commit().unwrap();
    assert_eq!(scan(&w, "CHARACTER"), vec![first, c1, c2]);
    assert_eq!(scan(&w, "PLACE"), vec![p1]);
    // all of them appear at the txn's tx, none one tx earlier
    assert_eq!(scan_at(&w, before_tx, "CHARACTER"), vec![first]);
    assert!(scan_at(&w, before_tx, "PLACE").is_empty());
    assert_eq!(scan_at(&w, w.latest_tx(), "CHARACTER"), vec![first, c1, c2]);
    clean(&w);
}

#[test]
fn a_dropped_txns_entities_are_not_listed_and_their_ids_are_reused() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    {
        let mut txn = w.begin();
        txn.create_entity("CHARACTER", Some("ghost1")).unwrap();
        txn.create_entity("PLACE", None).unwrap();
    } // dropped
    assert_eq!(scan(&w, "CHARACTER"), vec![a]);
    assert!(scan(&w, "PLACE").is_empty());
    let b = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(b.seq(), 2, "the dropped txn's sequence number is reused");
    assert_eq!(scan(&w, "CHARACTER"), vec![a, b]);
    // its handle was never defined
    assert!(w.entity_by_handle("ghost1").is_none());
}

#[test]
fn a_poisoned_txns_entities_are_not_listed() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let tx = w.latest_tx();
    let mut txn = w.begin();
    txn.create_entity("CHARACTER", None).unwrap();
    txn.create_entity("ITEM", None).unwrap();
    assert!(txn.assert(a, "no_such_attribute", s("x"), t(0)).is_err());
    assert!(txn.commit().is_err());
    assert_eq!(scan(&w, "CHARACTER"), vec![a]);
    assert!(scan(&w, "ITEM").is_empty());
    assert_eq!(w.latest_tx(), tx);
    // a failed create (unknown type) poisons too
    let mut txn = w.begin();
    txn.create_entity("CHARACTER", None).unwrap();
    assert!(txn.create_entity("NOPE", None).is_err());
    assert!(txn.commit().is_err());
    assert_eq!(scan(&w, "CHARACTER"), vec![a]);
    // a duplicate handle refused at create leaves the first one only
    w.create_entity("PLACE", Some("varrow")).unwrap();
    assert!(w.create_entity("PLACE", Some("varrow")).is_err());
    assert_eq!(scan(&w, "PLACE").len(), 1);
}

#[test]
fn an_empty_txn_and_a_refused_create_change_nothing() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    assert!(w.create_entity("NOPE", None).is_err());
    w.begin().commit().ok();
    assert_eq!(scan(&w, "CHARACTER"), vec![a]);
}

// ---------------------------------------------------------------- files

#[test]
fn reopen_gives_the_same_scans_and_entities_created_after_open_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let mut cs = vec![];
    let mut ps = vec![];
    let mut txs = vec![];
    for i in 0..9 {
        if i % 3 == 0 {
            ps.push(w.create_entity("PLACE", None).unwrap());
        } else {
            cs.push(
                w.create_entity("CHARACTER", Some(&format!("c{i}")))
                    .unwrap(),
            );
        }
        txs.push(w.latest_tx());
    }
    // an edge in the mix
    w.assert(cs[0], "located_in", Value::Ref(ps[0]), t(0))
        .unwrap();
    // a dropped txn leaves nothing behind in the file either
    drop(w.begin());
    drop(w);

    let mut w = World::open(&p).unwrap();
    assert_eq!(scan(&w, "CHARACTER"), cs);
    assert_eq!(scan(&w, "PLACE"), ps);
    assert!(scan(&w, "ITEM").is_empty());
    // older canons survive a reopen
    assert_eq!(scan_at(&w, txs[0], "PLACE"), vec![ps[0]]);
    assert!(scan_at(&w, txs[0], "CHARACTER").is_empty());
    assert_eq!(scan_at(&w, txs[4], "CHARACTER"), cs[..3].to_vec());

    let late = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(late.seq(), cs.len() as u64 + 1);
    let late_item = w.create_entity("ITEM", None).unwrap();
    let mut want = cs.clone();
    want.push(late);
    assert_eq!(scan(&w, "CHARACTER"), want);
    assert_eq!(scan(&w, "ITEM"), vec![late_item]);
    clean(&w);
    drop(w);

    let w = World::open(&p).unwrap();
    assert_eq!(scan(&w, "CHARACTER"), want);
    assert_eq!(scan(&w, "ITEM"), vec![late_item]);
    assert_eq!(scan(&w, "PLACE"), ps);
}

// ---------------------------------------------------------------- the label pattern

fn labelled_world() -> (World, Vec<EntityId>, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let cs: Vec<_> = (0..5)
        .map(|_| w.create_entity("CHARACTER", None).unwrap())
        .collect();
    let shrine = w.create_entity("PLACE", None).unwrap();
    (w, cs, shrine)
}

fn lk(w: &World, label: &str, at: i64) -> Vec<EntityId> {
    w.lookup("label", &s(label), Tick(at)).unwrap()
}

#[test]
fn label_lookup_returns_exactly_the_entities_labelled_at_the_tick() {
    let (mut w, cs, shrine) = labelled_world();
    w.assert(cs[1], "label", s("Deity"), t(0)).unwrap();
    w.assert(cs[3], "label", s("Deity"), t(10)).unwrap();
    w.assert(cs[3], "label", s("Hero"), t(10)).unwrap(); // many labels on one node
    w.assert(shrine, "label", s("Deity"), t(5)).unwrap();
    w.assert(cs[0], "label", s("Hero"), t(0)).unwrap();
    assert!(lk(&w, "Deity", -1).is_empty());
    assert_eq!(lk(&w, "Deity", 0), vec![cs[1]]);
    assert_eq!(lk(&w, "Deity", 5), vec![cs[1], shrine]);
    assert_eq!(lk(&w, "Deity", 10), vec![cs[1], cs[3], shrine]);
    assert_eq!(lk(&w, "Hero", 10), vec![cs[0], cs[3]]);
    assert!(lk(&w, "Villain", 10).is_empty());
    assert!(
        lk(&w, "deity", 10).is_empty(),
        "exact lookup is case-sensitive"
    );
}

#[test]
fn a_retracted_label_leaves_the_lookup_and_a_relabel_brings_it_back() {
    let (mut w, cs, _) = labelled_world();
    w.assert(cs[2], "label", s("Deity"), t(0)).unwrap();
    w.assert(cs[4], "label", s("Deity"), t(0)).unwrap();
    w.retract(cs[2], "label", s("Deity"), t(20)).unwrap();
    w.assert(cs[2], "label", s("Deity"), t(40)).unwrap(); // re-labelled
    assert_eq!(lk(&w, "Deity", 19), vec![cs[2], cs[4]]);
    assert_eq!(lk(&w, "Deity", 20), vec![cs[4]]);
    assert_eq!(lk(&w, "Deity", 39), vec![cs[4]]);
    assert_eq!(lk(&w, "Deity", 40), vec![cs[2], cs[4]]);
    // an older canon does not know the retraction
    let tx_before_retract = TxId(w.latest_tx().0 - 2);
    assert_eq!(
        w.at_canon(tx_before_retract)
            .lookup("label", &s("Deity"), Tick(25))
            .unwrap(),
        vec![cs[2], cs[4]]
    );
}

#[test]
fn intersecting_a_label_lookup_with_a_type_scan_gives_the_labelled_characters() {
    let (mut w, cs, shrine) = labelled_world();
    let item = w.create_entity("ITEM", None).unwrap();
    for e in [cs[0], cs[2], shrine, item] {
        w.assert(e, "label", s("Deity"), t(0)).unwrap();
    }
    w.retract(cs[0], "label", s("Deity"), t(50)).unwrap();
    let deities_at = |at: i64| -> Vec<EntityId> {
        let labelled = lk(&w, "Deity", at);
        w.entities_of_type("CHARACTER")
            .unwrap()
            .filter(|e| labelled.contains(e))
            .collect()
    };
    assert_eq!(deities_at(0), vec![cs[0], cs[2]]);
    assert_eq!(deities_at(49), vec![cs[0], cs[2]]);
    assert_eq!(deities_at(50), vec![cs[2]]);
    assert!(deities_at(-1).is_empty());
    // the label spans types; the scans keep them apart
    assert_eq!(lk(&w, "Deity", 0), vec![cs[0], cs[2], shrine, item]);
    let places: Vec<_> = w
        .entities_of_type("PLACE")
        .unwrap()
        .filter(|e| lk(&w, "Deity", 0).contains(e))
        .collect();
    assert_eq!(places, vec![shrine]);
}

// ---------------------------------------------------------------- property

#[derive(Clone, Debug)]
enum Op {
    Create(u8),
    Fact(u8),
    DropTxn(u8),
    Mark,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..3).prop_map(Op::Create),
        2 => (0u8..16).prop_map(Op::Fact),
        1 => (0u8..3).prop_map(Op::DropTxn),
        1 => Just(Op::Mark),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x4e6f_6465_5363),
        ..ProptestConfig::default()
    })]

    /// `entities_of_type` equals a plain per-type list of what was created, at the
    /// latest canon and at every tx recorded along the way.
    #[test]
    fn prop_scans_match_a_naive_model_at_every_canon(ops in prop::collection::vec(op_strategy(), 0..60)) {
        const TYPES: [&str; 3] = ["CHARACTER", "PLACE", "ITEM"];
        let mut w = World::in_memory(schema()).unwrap();
        // model: per type, (entity, tx it was created at)
        let mut model: [Vec<(EntityId, TxId)>; 3] = Default::default();
        let mut marks: Vec<TxId> = vec![TxId(0)];
        for op in &ops {
            match op {
                Op::Create(k) => {
                    let id = w.create_entity(TYPES[*k as usize], None).unwrap();
                    model[*k as usize].push((id, w.latest_tx()));
                }
                Op::Fact(n) => {
                    let all: Vec<_> = model.iter().flatten().map(|(e, _)| *e).collect();
                    if let Some(e) = all.get(*n as usize % all.len().max(1)) {
                        let _ = w.assert(*e, "label", s(&format!("L{n}")), t(i64::from(*n)));
                    }
                }
                Op::DropTxn(k) => {
                    let mut txn = w.begin();
                    txn.create_entity(TYPES[*k as usize], None).unwrap();
                }
                Op::Mark => marks.push(w.latest_tx()),
            }
        }
        marks.push(w.latest_tx());
        for (i, ty) in TYPES.iter().enumerate() {
            let want: Vec<_> = model[i].iter().map(|(e, _)| *e).collect();
            let got = scan(&w, ty);
            prop_assert!(ascending(&got));
            prop_assert_eq!(&got, &want);
            for c in &marks {
                let want: Vec<_> = model[i].iter().filter(|(_, tx)| tx <= c).map(|(e, _)| *e).collect();
                prop_assert_eq!(scan_at(&w, *c, ty), want, "type {} canon {:?}", ty, c);
            }
        }
        // no entity is in two scans, and none is an edge
        let mut seen = std::collections::HashSet::new();
        for ty in TYPES {
            for e in scan(&w, ty) {
                prop_assert!(!e.is_edge());
                prop_assert!(seen.insert(e), "{:?} listed twice", e);
            }
        }
        let rep = doctor(&w, &DoctorOptions::default());
        prop_assert!(rep.is_clean(), "{:#?}", rep.findings);
    }
}
