//! Slice 2, item 3: edge IDs (docs/slice-2-decisions.md, "Item 3").
//!
//! Expected API (re-exported at the crate root unless noted):
//!   `pub struct EdgeId(u64)` (Copy, Eq, Ord, Hash, Debug), module `id`
//!   `EdgeId::new(seq: u64) -> Result<EdgeId, IdError>`   (0 or >= 2^63 -> SeqOutOfRange)
//!   `EdgeId::seq(self) -> u64`, `EdgeId::raw(self) -> u64` (bit 63 set),
//!   `EdgeId::to_bytes(self) -> [u8; 8]` (BE), `impl From<EdgeId> for EntityId`
//!   `EntityId::is_edge(self) -> bool`, `EntityId::as_edge(self) -> Option<EdgeId>`
//!   `World::edge_id(&self, EntityId, &str, EntityId) -> Option<EdgeId>`
//!   `World::edge(&self, EdgeId) -> Option<(EntityId, String, EntityId)>`
//!   `Txn::edge_id(&self, EntityId, &str, EntityId) -> Option<EdgeId>`
//!   `log::EntryBody::DefineEdge { id: EdgeId, source: EntityId, attr: AttrId, target: EntityId }`
//!   log kind 6, body `id u64 | source u64 | attr u32 | target u64` (LE)
//! Bidirectional normalisation (item 7) is not tested here.
mod common;

use kala_core::log::{EntryBody, read_log};
use kala_core::*;
use std::path::{Path, PathBuf};

const LOC: u32 = 0; // located_in
const OWNS: u32 = 1;
const BOUGHT: u32 = 3;
const NAME: u32 = 4;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn schema() -> Schema {
    let r = |n, k| AttrDef::new(n, ValueType::Ref, k, Cardinality::Many);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(r("located_in", Kind::State))
        .attribute(r("owns", Kind::State))
        .attribute(r("parent_of", Kind::State).inverse("child_of"))
        .attribute(r("bought", Kind::Event))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "alive",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
}

struct Fx {
    w: World,
    a: EntityId,
    b: EntityId,
    p: EntityId,
    q: EntityId,
    sword: EntityId,
}

fn populate(w: &mut World) -> (EntityId, EntityId, EntityId, EntityId, EntityId) {
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    let q = w.create_entity("PLACE", Some("q")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    (a, b, p, q, sword)
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).unwrap();
    let (a, b, p, q, sword) = populate(&mut w);
    Fx {
        w,
        a,
        b,
        p,
        q,
        sword,
    }
}

fn eid(seq: u64) -> EdgeId {
    EdgeId::new(seq).unwrap()
}

// ---------------------------------------------------------------------------
// EdgeId type
// ---------------------------------------------------------------------------

#[test]
fn edge_id_new_accepts_the_valid_range_ends() {
    assert_eq!(EdgeId::new(1).unwrap().seq(), 1);
    assert_eq!(
        EdgeId::new((1u64 << 63) - 1).unwrap().seq(),
        (1u64 << 63) - 1
    );
}

#[test]
fn edge_id_new_rejects_zero_and_2_pow_63_and_above() {
    for s in [0u64, 1 << 63, (1 << 63) + 1, u64::MAX] {
        assert!(
            matches!(EdgeId::new(s), Err(IdError::SeqOutOfRange(_))),
            "seq {s}"
        );
    }
}

#[test]
fn edge_id_raw_has_bit_63_set_and_low_bits_are_the_seq() {
    assert_eq!(eid(1).raw(), (1u64 << 63) | 1);
    assert_eq!(eid(12345).raw(), (1u64 << 63) | 12345);
    assert_eq!(eid((1 << 63) - 1).raw(), u64::MAX);
}

#[test]
fn edge_id_to_bytes_is_big_endian_of_raw() {
    assert_eq!(eid(1).to_bytes(), ((1u64 << 63) | 1).to_be_bytes());
    assert_eq!(eid(0x0102_0304).to_bytes()[0], 0x80);
    assert_eq!(eid(0x0102_0304).to_bytes()[4..], [1, 2, 3, 4]);
}

#[test]
fn edge_id_order_follows_seq_and_is_copy_hash_debug() {
    use std::collections::HashSet;
    assert!(eid(1) < eid(2));
    assert!(eid(2) < eid(1 << 40));
    let e = eid(3);
    let f = e; // Copy
    assert_eq!(e, f);
    let mut s = HashSet::new();
    s.insert(e);
    s.insert(f);
    s.insert(eid(4));
    assert_eq!(s.len(), 2);
    assert!(!format!("{e:?}").is_empty());
}

#[test]
fn edge_converts_to_entity_id_with_the_same_raw_bits() {
    for s in [1u64, 2, 9, (1 << 63) - 1] {
        let e = eid(s);
        let id: EntityId = e.into();
        assert_eq!(id.raw(), e.raw());
        assert_eq!(id.to_bytes(), e.to_bytes());
        assert!(id.is_edge());
        assert_eq!(id.as_edge(), Some(e));
    }
}

#[test]
fn entity_ids_are_never_edges() {
    let max = EntityId::new(NodeTypeId((1 << 15) - 1), (1 << 48) - 1).unwrap();
    for id in [
        EntityId::new(NodeTypeId(0), 1).unwrap(),
        EntityId::new(NodeTypeId(7), 99).unwrap(),
        max,
    ] {
        assert!(!id.is_edge());
        assert_eq!(id.as_edge(), None);
    }
}

#[test]
fn entity_id_from_raw_bytes_with_bit_63_is_an_edge() {
    let id = EntityId::from_bytes(((1u64 << 63) | 5).to_be_bytes());
    assert!(id.is_edge());
    assert_eq!(id.as_edge(), Some(eid(5)));
    // bit 63 set with zero low bits is not a valid edge
    let zero = EntityId::from_bytes((1u64 << 63).to_be_bytes());
    assert_eq!(zero.as_edge(), None);
}

// ---------------------------------------------------------------------------
// Interning
// ---------------------------------------------------------------------------

#[test]
fn first_assert_of_a_ref_fact_interns_an_edge() {
    let mut f = fx();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), None);
    let r =
        f.w.assert(f.a, "located_in", Value::Ref(f.p), t(5))
            .unwrap();
    assert_eq!(r.changed, 1);
    let e = f.w.edge_id(f.a, "located_in", f.p).expect("edge interned");
    assert_eq!(e.seq(), 1);
    assert_eq!(f.w.edge(e), Some((f.a, "located_in".to_string(), f.p)));
}

#[test]
fn edge_seqs_start_at_one_and_increment_per_new_triple_across_attributes() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    f.w.assert(f.a, "owns", Value::Ref(f.sword), t(1)).unwrap();
    f.w.assert(f.b, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    f.w.assert(f.a, "located_in", Value::Ref(f.q), t(1))
        .unwrap();
    let seqs: Vec<u64> = [
        (f.a, "located_in", f.p),
        (f.a, "owns", f.sword),
        (f.b, "located_in", f.p),
        (f.a, "located_in", f.q),
    ]
    .iter()
    .map(|&(s, a, tg)| f.w.edge_id(s, a, tg).unwrap().seq())
    .collect();
    assert_eq!(seqs, vec![1, 2, 3, 4]);
}

#[test]
fn event_ref_attributes_get_edges_too() {
    let mut f = fx();
    f.w.assert(f.a, "bought", Value::Ref(f.sword), t(-208))
        .unwrap();
    let e = f.w.edge_id(f.a, "bought", f.sword).unwrap();
    assert_eq!(e.seq(), 1);
    assert_eq!(f.w.edge(e), Some((f.a, "bought".to_string(), f.sword)));
}

#[test]
fn non_ref_facts_intern_nothing() {
    let mut f = fx();
    f.w.assert(f.a, "name", Value::Str("Greg".into()), t(1))
        .unwrap();
    f.w.assert(f.a, "alive", Value::Bool(true), t(1)).unwrap();
    assert_eq!(f.w.edge(eid(1)), None);
    // the first ref edge afterwards is still seq 1
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p).unwrap().seq(), 1);
}

#[test]
fn same_triple_keeps_its_edge_id_after_retract_and_reassert() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let e = f.w.edge_id(f.a, "located_in", f.p).unwrap();
    f.w.retract(f.a, "located_in", Value::Ref(f.p), t(5))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), Some(e));
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(9))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), Some(e));
    // a different triple afterwards gets the next seq, not a reused one
    f.w.assert(f.a, "located_in", Value::Ref(f.q), t(9))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.q).unwrap().seq(), 2);
}

#[test]
fn a_prequel_assert_of_an_interned_triple_reuses_the_edge() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(100))
        .unwrap();
    let e = f.w.edge_id(f.a, "located_in", f.p).unwrap();
    let r =
        f.w.assert(f.a, "located_in", Value::Ref(f.p), t(-100))
            .unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), Some(e));
    f.w.assert(f.b, "located_in", Value::Ref(f.p), t(0))
        .unwrap();
    assert_eq!(f.w.edge_id(f.b, "located_in", f.p).unwrap().seq(), 2);
}

#[test]
fn a_noop_assert_does_not_intern_a_second_id() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let tx = f.w.latest_tx();
    let r =
        f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.edge(eid(2)), None);
    f.w.assert(f.a, "located_in", Value::Ref(f.q), t(1))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.q).unwrap().seq(), 2);
}

#[test]
fn a_noop_retract_interns_nothing() {
    let mut f = fx();
    let r =
        f.w.retract(f.a, "located_in", Value::Ref(f.p), t(1))
            .unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), None);
}

#[test]
fn a_same_tick_conflict_interns_nothing_new() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    f.w.retract(f.a, "located_in", Value::Ref(f.p), t(2))
        .unwrap();
    assert!(matches!(
        f.w.assert(f.a, "located_in", Value::Ref(f.p), t(2)),
        Err(Error::SameTickConflict { .. })
    ));
    assert_eq!(f.w.edge(eid(2)), None);
}

#[test]
fn directed_a_to_b_and_b_to_a_are_different_edges() {
    let mut f = fx();
    f.w.assert(f.a, "owns", Value::Ref(f.b), t(1)).unwrap();
    f.w.assert(f.b, "owns", Value::Ref(f.a), t(1)).unwrap();
    let ab = f.w.edge_id(f.a, "owns", f.b).unwrap();
    let ba = f.w.edge_id(f.b, "owns", f.a).unwrap();
    assert_ne!(ab, ba);
    assert_eq!(f.w.edge(ab), Some((f.a, "owns".to_string(), f.b)));
    assert_eq!(f.w.edge(ba), Some((f.b, "owns".to_string(), f.a)));
}

#[test]
fn edge_id_for_unknown_or_inverse_attribute_is_none_and_never_an_error() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "no_such_attr", f.p), None);
    assert_eq!(f.w.edge_id(f.p, "child_of", f.a), None);
    assert_eq!(f.w.edge_id(f.a, "child_of", f.p), None);
    // non-ref attribute, undefined entity
    assert_eq!(f.w.edge_id(f.a, "name", f.p), None);
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    assert_eq!(f.w.edge_id(ghost, "located_in", f.p), None);
}

#[test]
fn edge_of_an_undefined_id_is_none() {
    let mut f = fx();
    assert_eq!(f.w.edge(eid(1)), None);
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert!(f.w.edge(eid(1)).is_some());
    assert_eq!(f.w.edge(eid(2)), None);
    assert_eq!(f.w.edge(eid((1 << 63) - 1)), None);
}

#[test]
fn format_id_of_an_edge_is_edge_hash_seq() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    f.w.assert(f.a, "located_in", Value::Ref(f.q), t(1))
        .unwrap();
    let e1: EntityId = f.w.edge_id(f.a, "located_in", f.p).unwrap().into();
    let e2: EntityId = f.w.edge_id(f.a, "located_in", f.q).unwrap().into();
    assert_eq!(f.w.format_id(e1), "edge#1");
    assert_eq!(f.w.format_id(e2), "edge#2");
}

#[test]
fn interning_is_one_transaction_with_the_fact() {
    let mut f = fx();
    let before = f.w.latest_tx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert_eq!(f.w.latest_tx(), TxId(before.0 + 1));
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

#[test]
fn txn_edge_id_sees_an_edge_staged_earlier_in_the_same_txn() {
    let mut f = fx();
    let mut txn = f.w.begin();
    assert_eq!(txn.edge_id(f.a, "located_in", f.p), None);
    txn.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let e = txn.edge_id(f.a, "located_in", f.p).expect("staged edge");
    assert_eq!(e.seq(), 1);
    txn.assert(f.a, "owns", Value::Ref(f.sword), t(1)).unwrap();
    assert_eq!(txn.edge_id(f.a, "owns", f.sword).unwrap().seq(), 2);
    txn.commit().unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), Some(e));
    assert_eq!(f.w.edge_id(f.a, "owns", f.sword).unwrap().seq(), 2);
}

#[test]
fn txn_sees_committed_edges_and_does_not_reintern_them() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let e = f.w.edge_id(f.a, "located_in", f.p).unwrap();
    let mut txn = f.w.begin();
    assert_eq!(txn.edge_id(f.a, "located_in", f.p), Some(e));
    txn.assert(f.a, "located_in", Value::Ref(f.p), t(-5))
        .unwrap();
    txn.assert(f.a, "located_in", Value::Ref(f.q), t(1))
        .unwrap();
    assert_eq!(txn.edge_id(f.a, "located_in", f.q).unwrap().seq(), 2);
    txn.commit().unwrap();
}

#[test]
fn a_staged_edge_is_a_defined_ref_value_and_subject_inside_the_txn() {
    let mut f = fx();
    let mut txn = f.w.begin();
    txn.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let e: EntityId = txn.edge_id(f.a, "located_in", f.p).unwrap().into();
    // Ref value pointing at the staged edge
    txn.assert(f.b, "owns", Value::Ref(e), t(1)).unwrap();
    // a fact on the staged edge (item 4 behaviour, "defined" is item 3)
    txn.assert(e, "alive", Value::Bool(true), t(1)).unwrap();
    txn.commit().unwrap();
}

#[test]
fn a_dropped_txn_does_not_consume_edge_seqs() {
    let mut f = fx();
    {
        let mut txn = f.w.begin();
        txn.assert(f.a, "located_in", Value::Ref(f.p), t(1))
            .unwrap();
        txn.assert(f.a, "located_in", Value::Ref(f.q), t(1))
            .unwrap();
    }
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), None);
    assert_eq!(f.w.edge(eid(1)), None);
    f.w.assert(f.b, "located_in", Value::Ref(f.q), t(1))
        .unwrap();
    assert_eq!(f.w.edge_id(f.b, "located_in", f.q).unwrap().seq(), 1);
}

#[test]
fn a_poisoned_txn_does_not_consume_edge_seqs() {
    let mut f = fx();
    let mut txn = f.w.begin();
    txn.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert!(
        txn.assert(f.a, "no_such_attr", Value::Bool(true), t(1))
            .is_err()
    );
    assert_eq!(txn.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p), None);
    f.w.assert(f.b, "owns", Value::Ref(f.a), t(1)).unwrap();
    assert_eq!(f.w.edge_id(f.b, "owns", f.a).unwrap().seq(), 1);
}

#[test]
fn a_failed_auto_commit_write_does_not_consume_an_edge_seq() {
    let mut f = fx();
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    assert_eq!(
        f.w.assert(f.a, "located_in", Value::Ref(ghost), t(1))
            .unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    assert_eq!(f.w.edge_id(f.a, "located_in", f.p).unwrap().seq(), 1);
}

#[test]
fn a_txn_interning_many_edges_advances_latest_tx_by_exactly_one() {
    let mut f = fx();
    let before = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    txn.assert(f.b, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    txn.assert(f.a, "owns", Value::Ref(f.sword), t(1)).unwrap();
    let res = txn.commit().unwrap();
    assert_eq!(res.changed, 3);
    assert_eq!(res.tx, TxId(before.0 + 1));
    assert_eq!(f.w.latest_tx(), TxId(before.0 + 1));
}

// ---------------------------------------------------------------------------
// Undefined edge IDs stay unknown (no regression)
// ---------------------------------------------------------------------------

#[test]
fn undefined_edge_id_is_unknown_entity_as_subject_and_as_ref_value() {
    let mut f = fx();
    let ghost: EntityId = eid(1).into();
    assert_eq!(
        f.w.assert(ghost, "alive", Value::Bool(true), t(1))
            .unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    assert_eq!(
        f.w.assert(f.a, "owns", Value::Ref(ghost), t(1))
            .unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    assert!(matches!(
        f.w.as_of(ghost, "alive", Tick(1)),
        Err(Error::UnknownEntity(_))
    ));
    // one edge defined: seq 2 is still undefined
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let ghost2: EntityId = eid(2).into();
    assert_eq!(
        f.w.assert(ghost2, "alive", Value::Bool(true), t(1))
            .unwrap_err(),
        Error::UnknownEntity(ghost2)
    );
    assert_eq!(
        f.w.assert(f.a, "owns", Value::Ref(ghost2), t(1))
            .unwrap_err(),
        Error::UnknownEntity(ghost2)
    );
}

#[test]
fn a_defined_edge_is_accepted_as_a_ref_value_and_as_a_subject() {
    let mut f = fx();
    f.w.assert(f.a, "located_in", Value::Ref(f.p), t(1))
        .unwrap();
    let e: EntityId = f.w.edge_id(f.a, "located_in", f.p).unwrap().into();
    f.w.assert(f.b, "owns", Value::Ref(e), t(1)).unwrap();
    assert_eq!(
        f.w.as_of(f.b, "owns", Tick(1)).unwrap(),
        vec![Value::Ref(e)]
    );
    // this ref fact interned edge#2 = (b, owns, edge#1)
    assert_eq!(f.w.edge(eid(2)), Some((f.b, "owns".to_string(), e)));
    f.w.assert(e, "alive", Value::Bool(true), t(1)).unwrap();
    assert_eq!(
        f.w.as_of(e, "alive", Tick(1)).unwrap(),
        vec![Value::Bool(true)]
    );
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

fn file_world() -> (tempfile::TempDir, PathBuf, World, [EntityId; 5]) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let (a, b, pl, q, sword) = populate(&mut w);
    (dir, p, w, [a, b, pl, q, sword])
}

#[test]
fn edge_ids_survive_reopen_twice_and_the_counter_continues() {
    let (_d, p, mut w, [a, b, pl, q, sword]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(1)).unwrap();
    w.assert(a, "bought", Value::Ref(sword), t(2)).unwrap();
    w.retract(a, "located_in", Value::Ref(pl), t(9)).unwrap();
    drop(w);
    for _ in 0..2 {
        let w = World::open(&p).unwrap();
        assert_eq!(w.edge_id(a, "located_in", pl).unwrap().seq(), 1);
        assert_eq!(w.edge_id(a, "bought", sword).unwrap().seq(), 2);
        assert_eq!(w.edge(eid(2)), Some((a, "bought".to_string(), sword)));
        assert_eq!(w.edge(eid(3)), None);
        drop(w);
    }
    let mut w = World::open(&p).unwrap();
    // re-assert after reopen reuses the id; a new triple continues at 3
    w.assert(a, "located_in", Value::Ref(pl), t(20)).unwrap();
    assert_eq!(w.edge_id(a, "located_in", pl).unwrap().seq(), 1);
    w.assert(b, "located_in", Value::Ref(q), t(1)).unwrap();
    assert_eq!(w.edge_id(b, "located_in", q).unwrap().seq(), 3);
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(b, "located_in", q).unwrap().seq(), 3);
    assert_eq!(w.format_id(eid(3).into()), "edge#3");
}

#[test]
fn a_dropped_txn_on_a_file_world_consumes_no_edge_seq_even_after_reopen() {
    let (_d, p, mut w, [a, _b, pl, q, _s]) = file_world();
    {
        let mut txn = w.begin();
        txn.assert(a, "located_in", Value::Ref(pl), t(1)).unwrap();
    }
    w.assert(a, "located_in", Value::Ref(q), t(1)).unwrap();
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(a, "located_in", q).unwrap().seq(), 1);
    assert_eq!(w.edge_id(a, "located_in", pl), None);
}

// ---------------------------------------------------------------------------
// Log entries
// ---------------------------------------------------------------------------

/// Entries of the last committed txn (after the previous Commit).
fn last_txn(p: &Path) -> Vec<EntryBody> {
    let all = read_log(p).unwrap();
    let mut out = Vec::new();
    for e in all.into_iter().rev() {
        let is_commit = matches!(e.body, EntryBody::Commit { .. });
        if is_commit && !out.is_empty() {
            break;
        }
        out.push(e.body);
    }
    out.reverse();
    out
}

#[test]
fn first_ref_assert_logs_define_edge_before_the_fact_in_the_same_tx() {
    let (_d, p, mut w, [a, _b, pl, _q, _s]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(5)).unwrap();
    let txn = last_txn(&p);
    assert_eq!(txn.len(), 3, "{txn:?}");
    match &txn[0] {
        EntryBody::DefineEdge {
            id,
            source,
            attr,
            target,
        } => {
            assert_eq!(*id, eid(1));
            assert_eq!(*source, a);
            assert_eq!(*attr, AttrId(LOC));
            assert_eq!(*target, pl);
        }
        other => panic!("expected DefineEdge, got {other:?}"),
    }
    assert!(
        matches!(&txn[1], EntryBody::Fact { entity, attr, op: Op::Assert, .. }
        if *entity == a && *attr == AttrId(LOC))
    );
    assert!(matches!(&txn[2], EntryBody::Commit { .. }));
}

#[test]
fn second_assert_of_the_same_triple_logs_no_define_edge() {
    let (_d, p, mut w, [a, _b, pl, _q, _s]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(5)).unwrap();
    w.retract(a, "located_in", Value::Ref(pl), t(7)).unwrap();
    w.assert(a, "located_in", Value::Ref(pl), t(9)).unwrap();
    let defs = read_log(&p)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.body, EntryBody::DefineEdge { .. }))
        .count();
    assert_eq!(defs, 1);
}

#[test]
fn entry_order_in_one_txn_follows_operations_with_definitions_before_use() {
    let (_d, p, mut w, [a, _b, pl, _q, sword]) = file_world();
    let mut txn = w.begin();
    let c = txn.create_entity("CHARACTER", Some("c")).unwrap();
    txn.assert(c, "located_in", Value::Ref(pl), t(1)).unwrap();
    txn.assert(a, "name", Value::Str("A".into()), t(1)).unwrap();
    txn.assert(a, "owns", Value::Ref(sword), t(1)).unwrap();
    txn.commit().unwrap();
    let body = last_txn(&p);
    let kinds: Vec<&str> = body
        .iter()
        .map(|b| match b {
            EntryBody::DefineEntity { .. } => "entity",
            EntryBody::DefineEdge { .. } => "edge",
            EntryBody::Fact { .. } => "fact",
            EntryBody::Commit { .. } => "commit",
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["entity", "edge", "fact", "fact", "edge", "fact", "commit"]
    );
    let ids: Vec<EdgeId> = body
        .iter()
        .filter_map(|b| match b {
            EntryBody::DefineEdge { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![eid(1), eid(2)]);
}

#[test]
fn define_edge_body_layout_is_id_source_attr_target_little_endian() {
    let (_d, p, mut w, [a, _b, pl, _q, sword]) = file_world();
    w.assert(a, "owns", Value::Ref(sword), t(5)).unwrap();
    w.assert(a, "located_in", Value::Ref(pl), t(5)).unwrap();
    let file = std::fs::read(&p).unwrap();
    let mut found = Vec::new();
    for s in common::spans(&file) {
        for (start, end, kind) in common::frame_spans(&file, s) {
            if kind == 6 {
                found.push(file[start + 4 + 1 + 8..end].to_vec());
            }
        }
    }
    assert_eq!(found.len(), 2);
    let mut want = ((1u64 << 63) | 1).to_le_bytes().to_vec();
    want.extend_from_slice(&a.raw().to_le_bytes());
    want.extend_from_slice(&OWNS.to_le_bytes());
    want.extend_from_slice(&sword.raw().to_le_bytes());
    assert_eq!(found[0], want);
    assert_eq!(found[0].len(), 28);
    let mut want = ((1u64 << 63) | 2).to_le_bytes().to_vec();
    want.extend_from_slice(&a.raw().to_le_bytes());
    want.extend_from_slice(&LOC.to_le_bytes());
    want.extend_from_slice(&pl.raw().to_le_bytes());
    assert_eq!(found[1], want);
}

// ---------------------------------------------------------------------------
// Replay: hand-built transactions
// ---------------------------------------------------------------------------

struct Hand {
    _dir: tempfile::TempDir,
    path: PathBuf,
    a: EntityId,
    b: EntityId,
    p: EntityId,
}

/// A file world with entities a, b, p and no facts.
fn hand() -> Hand {
    let (dir, path, w, [a, b, p, _q, _s]) = file_world();
    drop(w);
    Hand {
        _dir: dir,
        path,
        a,
        b,
        p,
    }
}

/// Same, plus edge#1 = (a, located_in, p) interned through the API.
fn hand_with_edge() -> Hand {
    let (dir, path, mut w, [a, b, p, _q, _s]) = file_world();
    w.assert(a, "located_in", Value::Ref(p), t(1)).unwrap();
    drop(w);
    Hand {
        _dir: dir,
        path,
        a,
        b,
        p,
    }
}

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
    b.push(0); // precision byte (v4): Exact
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

fn assert_corrupt(h: &Hand, entries: &[(u8, Vec<u8>)], label: &str) {
    append_txn(&h.path, entries);
    let before = std::fs::read(&h.path).unwrap();
    match World::open(&h.path) {
        Err(Error::Corrupt(_)) => {}
        Err(e) => panic!("{label}: expected Corrupt, got {e:?}"),
        Ok(_) => panic!("{label}: expected Corrupt, open succeeded"),
    }
    assert_eq!(std::fs::read(&h.path).unwrap(), before, "{label}: touched");
    let torn = format!("{}.torn-", h.path.file_name().unwrap().to_string_lossy());
    let quarantined = std::fs::read_dir(h.path.parent().unwrap())
        .unwrap()
        .any(|e| e.unwrap().file_name().to_string_lossy().starts_with(&torn));
    assert!(!quarantined, "{label}: quarantine file made");
}

#[test]
fn control_hand_built_define_edge_plus_fact_opens() {
    let h = hand();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
            fact(h.a.raw(), LOC, 5, false, &Value::Ref(h.p)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(w.edge_id(h.a, "located_in", h.p), Some(eid(1)));
    assert_eq!(w.edge(eid(1)), Some((h.a, "located_in".to_string(), h.p)));
    assert_eq!(
        w.as_of(h.a, "located_in", Tick(5)).unwrap(),
        vec![Value::Ref(h.p)]
    );
}

#[test]
fn control_define_edge_then_a_fact_on_that_edge_opens() {
    let h = hand();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
            fact(h.a.raw(), LOC, 5, false, &Value::Ref(h.p)),
            fact(edge_raw(1), 5, 5, false, &Value::Bool(true)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(
        w.as_of(eid(1).into(), "alive", Tick(5)).unwrap(),
        vec![Value::Bool(true)]
    );
}

#[test]
fn control_second_define_edge_continues_the_sequence() {
    let h = hand_with_edge();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(2), h.b.raw(), LOC, h.p.raw()),
            fact(h.b.raw(), LOC, 5, false, &Value::Ref(h.p)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(w.edge_id(h.b, "located_in", h.p), Some(eid(2)));
}

#[test]
fn replay_define_edge_with_a_skipped_or_repeated_or_zero_id_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(2), h.a.raw(), LOC, h.p.raw())],
        "first edge is #2",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(0), h.a.raw(), LOC, h.p.raw())],
        "edge seq 0",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(h.a.raw(), h.a.raw(), LOC, h.p.raw())],
        "id without the edge tag",
    );
    let h = hand_with_edge();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.b.raw(), LOC, h.p.raw())],
        "id repeats #1 for a new triple",
    );
    let h = hand_with_edge();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(3), h.b.raw(), LOC, h.p.raw())],
        "id skips #2",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
            define_edge(edge_raw(3), h.b.raw(), LOC, h.p.raw()),
        ],
        "skip inside one txn",
    );
}

#[test]
fn replay_define_edge_with_an_undefined_source_or_target_is_corrupt() {
    let h = hand();
    let ghost_char = EntityId::new(NodeTypeId(0), 99).unwrap().raw();
    let ghost_place = EntityId::new(NodeTypeId(1), 99).unwrap().raw();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), ghost_char, LOC, h.p.raw())],
        "undefined source",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.a.raw(), LOC, ghost_place)],
        "undefined target",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.a.raw(), LOC, edge_raw(7))],
        "undefined edge as target",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), edge_raw(7), LOC, h.p.raw())],
        "undefined edge as source",
    );
}

#[test]
fn replay_define_edge_with_an_undefined_or_non_ref_attribute_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.a.raw(), 99, h.p.raw())],
        "undefined attr",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.a.raw(), NAME, h.p.raw())],
        "non-ref attr (name)",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(1), h.a.raw(), 5, h.p.raw())],
        "non-ref attr (alive)",
    );
}

#[test]
fn replay_define_edge_for_an_already_interned_triple_is_corrupt() {
    let h = hand_with_edge();
    assert_corrupt(
        &h,
        &[define_edge(edge_raw(2), h.a.raw(), LOC, h.p.raw())],
        "same triple again",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
            define_edge(edge_raw(2), h.a.raw(), LOC, h.p.raw()),
        ],
        "same triple twice in one txn",
    );
}

#[test]
fn replay_define_edge_differing_only_in_attribute_is_a_new_triple_and_opens() {
    let h = hand_with_edge();
    append_txn(
        &h.path,
        &[
            define_edge(edge_raw(2), h.a.raw(), BOUGHT, h.p.raw()),
            fact(h.a.raw(), BOUGHT, 5, false, &Value::Ref(h.p)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(w.edge_id(h.a, "bought", h.p), Some(eid(2)));
}

#[test]
fn replay_ref_fact_without_an_edge_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[fact(h.a.raw(), LOC, 5, false, &Value::Ref(h.p))],
        "assert, no edge",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[fact(h.a.raw(), LOC, 5, true, &Value::Ref(h.p))],
        "retract, no edge",
    );
    let h = hand();
    assert_corrupt(
        &h,
        &[fact(h.a.raw(), BOUGHT, 5, false, &Value::Ref(h.p))],
        "event assert, no edge",
    );
}

#[test]
fn replay_ref_fact_for_a_different_triple_than_the_defined_edge_is_corrupt() {
    let h = hand_with_edge();
    // edge#1 is (a, located_in, p); this fact is (b, located_in, p)
    assert_corrupt(
        &h,
        &[fact(h.b.raw(), LOC, 5, false, &Value::Ref(h.p))],
        "other source",
    );
    let h = hand_with_edge();
    // same endpoints, other attribute
    assert_corrupt(
        &h,
        &[fact(h.a.raw(), OWNS, 5, false, &Value::Ref(h.p))],
        "other attribute",
    );
    let h = hand_with_edge();
    // reversed direction
    assert_corrupt(
        &h,
        &[fact(h.p.raw(), LOC, 5, false, &Value::Ref(h.a))],
        "reversed",
    );
}

#[test]
fn replay_ref_fact_before_its_define_edge_in_the_same_txn_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[
            fact(h.a.raw(), LOC, 5, false, &Value::Ref(h.p)),
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
        ],
        "fact before define edge",
    );
}

#[test]
fn replay_ref_fact_on_an_already_interned_triple_needs_no_new_define_edge() {
    // positive control for the rule above: an edge from an earlier txn is enough
    let h = hand_with_edge();
    append_txn(
        &h.path,
        &[
            fact(h.a.raw(), LOC, 9, true, &Value::Ref(h.p)),
            fact(h.a.raw(), LOC, -5, false, &Value::Ref(h.p)),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(w.edge_id(h.a, "located_in", h.p), Some(eid(1)));
    assert_eq!(
        w.as_of(h.a, "located_in", Tick(0)).unwrap(),
        vec![Value::Ref(h.p)]
    );
}

#[test]
fn replay_fact_on_an_undefined_edge_subject_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[fact(edge_raw(1), 5, 5, false, &Value::Bool(true))],
        "no edges at all",
    );
    let h = hand_with_edge();
    assert_corrupt(
        &h,
        &[fact(edge_raw(2), 5, 5, false, &Value::Bool(true))],
        "edge#2 undefined",
    );
    let h = hand_with_edge();
    assert_corrupt(
        &h,
        &[fact(edge_raw(0), 5, 5, false, &Value::Bool(true))],
        "edge seq 0",
    );
}

#[test]
fn replay_fact_on_an_edge_defined_later_in_the_same_txn_is_corrupt() {
    let h = hand();
    assert_corrupt(
        &h,
        &[
            fact(edge_raw(1), 5, 5, false, &Value::Bool(true)),
            define_edge(edge_raw(1), h.a.raw(), LOC, h.p.raw()),
            fact(h.a.raw(), LOC, 5, false, &Value::Ref(h.p)),
        ],
        "edge subject used before its define",
    );
}

#[test]
fn replay_ref_value_pointing_at_an_undefined_edge_is_corrupt() {
    let h = hand_with_edge();
    // edge#2 would be (b, owns, edge#9) - target undefined
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(2), h.b.raw(), OWNS, edge_raw(9)),
            fact(h.b.raw(), OWNS, 5, false, &Value::Ref(eid(9).into())),
        ],
        "ref to undefined edge",
    );
}

// ---------------------------------------------------------------------------
// Item 3 confirm: edge cases
// ---------------------------------------------------------------------------

#[test]
fn edge_to_an_entity_staged_in_the_same_txn_logs_entity_before_edge_and_reopens() {
    let (_d, p, mut w, [a, ..]) = file_world();
    let mut txn = w.begin();
    let c = txn.create_entity("CHARACTER", Some("c")).unwrap();
    txn.assert(a, "owns", Value::Ref(c), t(3)).unwrap();
    txn.commit().unwrap();
    let body = last_txn(&p);
    assert!(
        matches!(body[0], EntryBody::DefineEntity { id, .. } if id == c),
        "{body:?}"
    );
    assert!(
        matches!(body[1], EntryBody::DefineEdge { source, target, .. } if source == a && target == c),
        "{body:?}"
    );
    assert!(matches!(body[2], EntryBody::Fact { .. }));
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(a, "owns", c), Some(eid(1)));
    assert_eq!(w.edge(eid(1)), Some((a, "owns".to_string(), c)));
}

#[test]
fn self_ref_edge_is_interned_and_survives_reopen() {
    let (_d, p, mut w, [a, ..]) = file_world();
    let r = w.assert(a, "owns", Value::Ref(a), t(1)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.edge_id(a, "owns", a), Some(eid(1)));
    assert_eq!(w.edge(eid(1)), Some((a, "owns".to_string(), a)));
    assert_eq!(w.as_of(a, "owns", Tick(1)).unwrap(), vec![Value::Ref(a)]);
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(a, "owns", a), Some(eid(1)));
    assert_eq!(w.as_of(a, "owns", Tick(1)).unwrap(), vec![Value::Ref(a)]);
}

#[test]
fn edge_on_an_edge_is_interned_once_edge_1_exists() {
    let (_d, p, mut w, [a, _b, pl, ..]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(1)).unwrap();
    let e1: EntityId = eid(1).into();
    let r = w.assert(e1, "owns", Value::Ref(e1), t(2)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.edge_id(e1, "owns", e1), Some(eid(2)));
    assert_eq!(w.edge(eid(2)), Some((e1, "owns".to_string(), e1)));
    assert_eq!(w.as_of(e1, "owns", Tick(2)).unwrap(), vec![Value::Ref(e1)]);
    // an ordinary entity may also point at the edge
    w.assert(a, "owns", Value::Ref(e1), t(2)).unwrap();
    assert_eq!(w.edge_id(a, "owns", e1), Some(eid(3)));
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(e1, "owns", e1), Some(eid(2)));
    assert_eq!(w.edge_id(a, "owns", e1), Some(eid(3)));
    let _ = p;
}

#[test]
fn edge_on_an_edge_before_edge_1_exists_is_unknown_entity() {
    let (_d, _p, mut w, [..]) = file_world();
    let e1: EntityId = eid(1).into();
    assert_eq!(
        w.assert(e1, "owns", Value::Ref(e1), t(2)).unwrap_err(),
        Error::UnknownEntity(e1)
    );
    assert_eq!(w.edge(eid(1)), None);
}

#[test]
fn replay_define_edge_naming_an_entity_defined_later_in_the_same_txn_is_corrupt() {
    let h = hand();
    let new = h.b.raw() + 1; // next CHARACTER, not defined yet
    let mut def = new.to_le_bytes().to_vec();
    def.push(0);
    assert_corrupt(
        &h,
        &[
            define_edge(edge_raw(1), h.a.raw(), OWNS, new),
            (3, def),
            fact(
                h.a.raw(),
                OWNS,
                5,
                false,
                &Value::Ref(EntityId::from_bytes(new.to_be_bytes())),
            ),
        ],
        "DefineEdge before the DefineEntity of its target",
    );
}

#[test]
fn control_define_entity_then_define_edge_to_it_opens() {
    let h = hand();
    let new = h.b.raw() + 1;
    let mut def = new.to_le_bytes().to_vec();
    def.push(0);
    append_txn(
        &h.path,
        &[
            (3, def),
            define_edge(edge_raw(1), h.a.raw(), OWNS, new),
            fact(
                h.a.raw(),
                OWNS,
                5,
                false,
                &Value::Ref(EntityId::from_bytes(new.to_be_bytes())),
            ),
        ],
    );
    let w = World::open(&h.path).unwrap();
    assert_eq!(w.edge(eid(1)).unwrap().0, h.a);
}

#[test]
fn torn_tail_mid_txn_with_a_define_edge_drops_the_edge_with_its_fact_and_reuses_the_seq() {
    let (_d, p, mut w, [a, b, pl, q, sword]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(1)).unwrap(); // edge#1
    let committed = std::fs::metadata(&p).unwrap().len();
    w.assert(a, "owns", Value::Ref(sword), t(2)).unwrap(); // edge#2, to be torn
    drop(w);
    let full = std::fs::metadata(&p).unwrap().len();
    assert!(full > committed + 10);
    let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
    f.set_len(full - 7).unwrap();
    drop(f);

    let mut w = World::open(&p).unwrap();
    assert!(w.recovery().bytes_cut > 0);
    assert_eq!(std::fs::metadata(&p).unwrap().len(), committed);
    assert_eq!(w.edge_id(a, "owns", sword), None);
    assert_eq!(w.edge(eid(2)), None);
    assert_eq!(w.as_of(a, "owns", Tick(2)).unwrap(), vec![]);
    assert_eq!(w.edge_id(a, "located_in", pl), Some(eid(1)));
    // the seq is reused by the next new triple
    w.assert(b, "located_in", Value::Ref(q), t(3)).unwrap();
    assert_eq!(w.edge_id(b, "located_in", q), Some(eid(2)));
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.edge_id(b, "located_in", q), Some(eid(2)));
    assert_eq!(w.edge_id(a, "owns", sword), None);
}

#[test]
fn a_dropped_txn_that_would_intern_an_edge_leaves_the_log_byte_identical() {
    let (_d, p, mut w, [a, _b, pl, _q, sword]) = file_world();
    w.assert(a, "located_in", Value::Ref(pl), t(1)).unwrap();
    let before = std::fs::read(&p).unwrap();
    {
        let mut txn = w.begin();
        txn.assert(a, "owns", Value::Ref(sword), t(2)).unwrap();
        txn.assert(a, "owns", Value::Ref(a), t(2)).unwrap();
    }
    assert_eq!(std::fs::read(&p).unwrap(), before);
    // a poisoned txn too
    {
        let mut txn = w.begin();
        txn.assert(a, "owns", Value::Ref(sword), t(2)).unwrap();
        assert!(txn.assert(a, "nope", Value::Bool(true), t(2)).is_err());
        assert!(txn.commit().is_err());
    }
    assert_eq!(std::fs::read(&p).unwrap(), before);
}

mod prop {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashMap;

    #[derive(Clone, Debug)]
    struct Op1 {
        src: usize,
        attr: usize, // 0 = located_in, 1 = owns
        dst: usize,
        tick: i64,
        retract: bool,
    }

    fn op() -> impl Strategy<Value = Op1> {
        (0..3usize, 0..2usize, 0..3usize, -3i64..4, any::<bool>()).prop_map(
            |(src, attr, dst, tick, retract)| Op1 {
                src,
                attr,
                dst,
                tick,
                retract,
            },
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        #[allow(clippy::needless_range_loop)]
        fn edge_ids_are_dense_one_per_triple_first_stored_asserted_and_stable_across_reopen(
            ops in proptest::collection::vec(op(), 0..40)
        ) {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("w.kala");
            let mut w = World::create(&p, schema()).unwrap();
            // a, b are characters, c is a place; all can be ref targets.
            let ents = [
                w.create_entity("CHARACTER", Some("a")).unwrap(),
                w.create_entity("CHARACTER", Some("b")).unwrap(),
                w.create_entity("PLACE", Some("c")).unwrap(),
            ];
            let names = ["located_in", "owns"];
            let mut expected: HashMap<(usize, usize, usize), u64> = HashMap::new();
            for o in &ops {
                let (s, d) = (ents[o.src], ents[o.dst]);
                let v = Value::Ref(d);
                let res = if o.retract {
                    w.retract(s, names[o.attr], v, t(o.tick))
                } else {
                    w.assert(s, names[o.attr], v, t(o.tick))
                };
                let key = (o.src, o.attr, o.dst);
                if let Ok(r) = res
                    && !o.retract
                    && r.changed == 1
                    && !expected.contains_key(&key)
                {
                    let n = expected.len() as u64 + 1;
                    expected.insert(key, n);
                }
                // after every write: exactly the expected edges exist
                for (&(si, ai, di), &seq) in &expected {
                    prop_assert_eq!(
                        w.edge_id(ents[si], names[ai], ents[di]).map(|e| e.seq()),
                        Some(seq)
                    );
                }
                prop_assert_eq!(w.edge(eid(expected.len() as u64 + 1)), None);
            }
            // every triple never stored-asserted has no edge
            for si in 0..3 { for ai in 0..2 { for di in 0..3 {
                if !expected.contains_key(&(si, ai, di)) {
                    prop_assert_eq!(w.edge_id(ents[si], names[ai], ents[di]), None);
                }
            }}}
            drop(w);
            let w = World::open(&p).unwrap();
            for (&(si, ai, di), &seq) in &expected {
                prop_assert_eq!(
                    w.edge_id(ents[si], names[ai], ents[di]).map(|e| e.seq()),
                    Some(seq)
                );
                prop_assert_eq!(
                    w.edge(eid(seq)),
                    Some((ents[si], names[ai].to_string(), ents[di]))
                );
            }
            prop_assert_eq!(w.edge(eid(expected.len() as u64 + 1)), None);
        }
    }
}
