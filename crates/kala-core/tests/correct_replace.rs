//! Slice 3, item 3: `correct(entry, Replace { value?, tick?, op? })`
//! (docs/slice-3-decisions.md, "Core model of corrections", "Item 3", and in
//! "Item 4" the kind-8 `Revised` entry). RED until the implementation lands:
//! `Replace` currently returns `Err(Error::Unsupported(..))`, and
//! `EntryBody::Revised` does not exist yet (so this file does not compile
//! until it does).
//!
//! API expected (everything but `EntryBody` / `EntryId` / `read_log` is
//! re-exported at the crate root; those three come from `kala_core::log`):
//!
//! ```text
//! World::correct(&mut self, entry: EntryId, c: Correction) -> Result<WriteResult, Error>
//! Txn::correct(&mut self, entry: EntryId, c: Correction)   -> Result<WriteResult, Error>
//! Correction::Replace { value: Option<Value>, tick: Option<Tick>, op: Option<Op> }
//! EntryBody::Revised { of: EntryId, rev: u32, entity: EntityId, attr: AttrId,
//!                      value: Value, tick: Tick, op: Op }          // kind 8
//! EntryInfo::revision() == Some((of, rev)) for Deleted AND Revised
//! ```
//!
//! WriteResult of a Replace: `tick` = the NEW fact's tick; `entries` =
//! `[Deleted, Revised]` (moved key, `changed` 2), `[Revised]` (same key, op
//! changed, `changed` 1), `[Deleted]` (the new key already holds the same
//! op, `changed` 1) or `[]` (nothing changed, `changed` 0); `edges` = the
//! edge interned for a new ref triple whose new op is Assert.
//!
//! Assumptions that follow the contract's wording (flagged in the report):
//! (1) `TxnKeyConflict`'s key fields for a second correction of one chain
//! are not pinned (only the variant and the entity); (2) a Replace that
//! changes the op to Retract onto a never-asserted ref triple interns no
//! edge ("interns its edge when the new op is Assert").

mod common;

use kala_core::log::{EntryBody, EntryId, read_log};
use kala_core::*;
use std::path::PathBuf;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("ITEM")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "owns",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
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
        .attribute(AttrDef::new(
            "height",
            ValueType::F64,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "bought",
            ValueType::Ref,
            Kind::Event,
            Cardinality::Many,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn s(x: &str) -> Value {
    Value::Str(x.into())
}

struct Ents {
    greg: EntityId,
    edwin: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    sword: EntityId,
}

/// greg < edwin < mara (creation order within one node type).
fn populate(w: &mut World) -> Ents {
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    assert!(greg < edwin && edwin < mara);
    Ents {
        greg,
        edwin,
        mara,
        varrow,
        ashford,
        sword,
    }
}

fn mem() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = populate(&mut w);
    (w, e)
}

fn file() -> (tempfile::TempDir, PathBuf, World, Ents) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let e = populate(&mut w);
    (dir, path, w, e)
}

fn rep(value: Option<Value>, tick: Option<Tick>, op: Option<Op>) -> Correction {
    Correction::Replace {
        value,
        tick,
        op,
        precision: None,
    }
}

fn only_entry(r: &WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1, "{r:?}");
    r.entries[0]
}

fn holds(w: &World, e: EntityId, a: &str, v: &Value, tick: i64) -> bool {
    w.as_of_value(e, a, v, Tick(tick)).unwrap()
}

fn holds_at(v: &View<'_>, e: EntityId, a: &str, val: &Value, tick: i64) -> bool {
    v.as_of_value(e, a, val, Tick(tick)).unwrap()
}

fn hist(v: &View<'_>, e: EntityId, a: &str) -> Vec<(i64, Op)> {
    v.history(e, a)
        .unwrap()
        .iter()
        .map(|c| (c.tick.0, c.op))
        .collect()
}

#[derive(Debug, PartialEq)]
struct Rev {
    of: EntryId,
    rev: u32,
    entity: EntityId,
    attr: AttrId,
    value: Value,
    tick: Tick,
    op: Op,
}

fn revised(w: &World, id: EntryId) -> Rev {
    let info = w.entry(id).unwrap();
    let revision = info.revision();
    match info.body {
        EntryBody::Revised {
            of,
            rev,
            entity,
            attr,
            value,
            tick,
            op,
            precision: _,
        } => {
            assert_eq!(revision, Some((of, rev)));
            Rev {
                of,
                rev,
                entity,
                attr,
                value,
                tick,
                op,
            }
        }
        other => panic!("entry#{} is not Revised: {other:?}", id.0),
    }
}

/// `(of, rev, entity, attr, value, tick)` of a Deleted entry.
fn deleted(w: &World, id: EntryId) -> (EntryId, u32, EntityId, AttrId, Value, Tick) {
    let info = w.entry(id).unwrap();
    let revision = info.revision();
    match info.body {
        EntryBody::Deleted {
            of,
            rev,
            entity,
            attr,
            value,
            tick,
        } => {
            assert_eq!(revision, Some((of, rev)));
            (of, rev, entity, attr, value, tick)
        }
        other => panic!("entry#{} is not Deleted: {other:?}", id.0),
    }
}

fn attr_of(w: &World, id: EntryId) -> AttrId {
    match w.entry(id).unwrap().body {
        EntryBody::Fact { attr, .. } => attr,
        other => panic!("{other:?}"),
    }
}

/// Greg located_in Varrow: assert @-300, retract @-250 (returned: the
/// retract's entry E).
fn greg_walk(w: &mut World, e: &Ents) -> (EntryId, EntryId) {
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let r = only_entry(&w.retract(e.greg, "located_in", v, t(-250)).unwrap());
    (a, r)
}

// ----------------------------------------------------------- Greg example

#[test]
fn greg_left_at_minus_280_not_minus_250() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let located = attr_of(&w, ent);
    let edge = w.edge_id(e.greg, "located_in", e.varrow).unwrap();
    let before = w.latest_tx();
    let original = w.entry(ent).unwrap();

    let r = w.correct(ent, rep(None, t(-280), None)).unwrap();
    assert_eq!(r.changed, 2);
    assert_eq!(r.tick, Tick(-280), "tick = the new fact's tick");
    assert_eq!(r.entries.len(), 2);
    assert!(r.edges.is_empty(), "the triple's edge already exists");
    assert_eq!(w.latest_tx(), TxId(before.0 + 1), "one transaction");
    let (d, rv) = (r.entries[0], r.entries[1]);
    assert_eq!(rv.0, d.0 + 1, "Deleted first, then Revised");

    assert_eq!(
        deleted(&w, d),
        (ent, 1, e.greg, located, v.clone(), Tick(-250))
    );
    assert_eq!(
        revised(&w, rv),
        Rev {
            of: ent,
            rev: 1,
            entity: e.greg,
            attr: located,
            value: v.clone(),
            tick: Tick(-280),
            op: Op::Retract,
        }
    );
    assert_eq!(w.entry(d).unwrap().tx, w.latest_tx());
    assert_eq!(w.entry(rv).unwrap().tx, w.latest_tx());
    assert_eq!(w.entry(ent).unwrap(), original, "original untouched");

    // latest canon
    assert!(holds(&w, e.greg, "located_in", &v, -290));
    assert!(holds(&w, e.greg, "located_in", &v, -281));
    assert!(!holds(&w, e.greg, "located_in", &v, -280));
    assert!(!holds(&w, e.greg, "located_in", &v, -260));
    assert!(!holds(&w, e.greg, "located_in", &v, -250));
    assert!(!holds(&w, e.greg, "located_in", &v, i64::MAX));
    assert!(
        w.as_of(e.greg, "located_in", Tick(-260))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        w.neighbors(e.greg, "located_in", Direction::Out, Tick(-290))
            .unwrap(),
        vec![(e.varrow, edge)]
    );
    assert!(
        w.neighbors(e.varrow, "located_in", Direction::In, Tick(-260))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, Op::Assert), (-280, Op::Retract)]
    );

    // canon before the correction
    let old = w.at_canon(before);
    assert!(holds_at(&old, e.greg, "located_in", &v, -260));
    assert!(holds_at(&old, e.greg, "located_in", &v, -280));
    assert!(!holds_at(&old, e.greg, "located_in", &v, -250));
    assert_eq!(
        hist(&old, e.greg, "located_in"),
        vec![(-300, Op::Assert), (-250, Op::Retract)]
    );
}

// ------------------------------------------------------- shapes of Replace

#[test]
fn same_key_replace_changing_only_the_op_is_one_revised_entry() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let before = w.latest_tx();
    let r = w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-300)));
    let rv = only_entry(&r);
    assert_eq!(
        revised(&w, rv),
        Rev {
            of: a,
            rev: 1,
            entity: e.greg,
            attr: attr_of(&w, a),
            value: v.clone(),
            tick: Tick(-300),
            op: Op::Retract,
        }
    );
    assert!(r.edges.is_empty());
    // latest-wins at the one tick: the newest tx's retract
    assert!(!holds(&w, e.greg, "located_in", &v, -300));
    assert!(!holds(&w, e.greg, "located_in", &v, 0));
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, Op::Retract)]
    );
    let old = w.at_canon(before);
    assert!(holds_at(&old, e.greg, "located_in", &v, -300));
}

#[test]
fn same_key_replace_back_to_the_old_op_is_rev_2() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    let r = w.correct(a, rep(None, None, Some(Op::Assert))).unwrap();
    assert_eq!(r.changed, 1);
    let rv = revised(&w, only_entry(&r));
    assert_eq!((rv.of, rv.rev, rv.op), (a, 2, Op::Assert));
    assert!(holds(&w, e.greg, "located_in", &v, -300));
}

#[test]
fn replace_value_moves_the_key_and_writes_deleted_then_revised() {
    let (mut w, e) = mem();
    let old_v = Value::Ref(e.varrow);
    let new_v = Value::Ref(e.ashford);
    let a = only_entry(
        &w.assert(e.greg, "located_in", old_v.clone(), t(-300))
            .unwrap(),
    );
    let before = w.latest_tx();
    let r = w.correct(a, rep(Some(new_v.clone()), None, None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-300)));
    assert_eq!(r.entries.len(), 2);
    let (d, rv) = (r.entries[0], r.entries[1]);
    let attr = attr_of(&w, a);
    assert_eq!(
        deleted(&w, d),
        (a, 1, e.greg, attr, old_v.clone(), Tick(-300))
    );
    assert_eq!(
        revised(&w, rv),
        Rev {
            of: a,
            rev: 1,
            entity: e.greg,
            attr,
            value: new_v.clone(),
            tick: Tick(-300),
            op: Op::Assert,
        }
    );
    // the new triple interned an edge, reported in `edges`
    let edge = w.edge_id(e.greg, "located_in", e.ashford).unwrap();
    assert_eq!(r.edges, vec![edge]);
    assert!(!holds(&w, e.greg, "located_in", &old_v, -300));
    assert!(holds(&w, e.greg, "located_in", &new_v, -300));
    assert_eq!(
        w.as_of(e.greg, "located_in", Tick(0)).unwrap(),
        vec![new_v.clone()]
    );
    assert_eq!(
        w.neighbors(e.ashford, "located_in", Direction::In, Tick(0))
            .unwrap(),
        vec![(e.greg, edge)]
    );
    assert!(
        w.neighbors(e.varrow, "located_in", Direction::In, Tick(0))
            .unwrap()
            .is_empty()
    );
    // old canon
    let old = w.at_canon(before);
    assert!(holds_at(&old, e.greg, "located_in", &old_v, -300));
    assert!(!holds_at(&old, e.greg, "located_in", &new_v, -300));
    // the old edge identity is permanent
    assert!(w.edge_id(e.greg, "located_in", e.varrow).is_some());
}

#[test]
fn replace_value_and_tick_and_op_together() {
    let (mut w, e) = mem();
    let (a, _) = greg_walk(&mut w, &e);
    // the base assert @-300 Varrow becomes retract Ashford @-100
    let r = w
        .correct(
            a,
            rep(Some(Value::Ref(e.ashford)), t(-100), Some(Op::Retract)),
        )
        .unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-100)));
    let rv = revised(&w, r.entries[1]);
    assert_eq!(
        (rv.value, rv.tick, rv.op),
        (Value::Ref(e.ashford), Tick(-100), Op::Retract)
    );
    // retract-only new triple: no edge interned
    assert!(r.edges.is_empty());
    assert_eq!(w.edge_id(e.greg, "located_in", e.ashford), None);
}

#[test]
fn replace_of_a_non_ref_value_moves_the_key_without_edges() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let r = w.correct(a, rep(Some(s("Gregor")), None, None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-208)));
    assert!(r.edges.is_empty());
    assert!(!holds(&w, e.greg, "name", &s("Greg"), -208));
    assert!(holds(&w, e.greg, "name", &s("Gregor"), -208));
}

#[test]
fn replace_with_nothing_changed_is_a_no_op() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let tx = w.latest_tx();
    let dump = w.index_dump();
    let next = w.entry(EntryId(ent.0 + 2)); // the retract's Commit is ent+1
    let _ = next;
    for c in [
        rep(None, None, None),
        rep(Some(v.clone()), t(-250), Some(Op::Retract)),
        rep(Some(v.clone()), None, None),
        rep(None, t(-250), None),
        rep(None, None, Some(Op::Retract)),
    ] {
        let r = w.correct(ent, c.clone()).unwrap();
        assert_eq!(r.changed, 0, "{c:?}");
        assert!(r.entries.is_empty(), "{c:?}");
        assert!(r.edges.is_empty(), "{c:?}");
        assert_eq!(r.tick, Tick(-250), "{c:?}");
        assert_eq!(w.latest_tx(), tx, "{c:?}: no tx consumed");
    }
    assert_eq!(w.index_dump(), dump);
    // a real correction afterwards still gets rev 1 and the next entry id
    let r = w.correct(ent, rep(None, t(-280), None)).unwrap();
    assert_eq!(deleted(&w, r.entries[0]).1, 1);
}

// ------------------------------------------------------------- chains

#[test]
fn a_second_correction_via_the_base_id_is_rev_2_on_the_current_revision() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let attr = attr_of(&w, ent);
    w.correct(ent, rep(None, t(-280), None)).unwrap();
    let r = w.correct(ent, rep(None, t(-270), None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-270)));
    // Deleted sits at the CURRENT revision's key (-280), pointing at the base
    assert_eq!(
        deleted(&w, r.entries[0]),
        (ent, 2, e.greg, attr, v.clone(), Tick(-280))
    );
    let rv = revised(&w, r.entries[1]);
    assert_eq!(
        (rv.of, rv.rev, rv.tick, rv.op),
        (ent, 2, Tick(-270), Op::Retract)
    );
    assert!(holds(&w, e.greg, "located_in", &v, -271));
    assert!(!holds(&w, e.greg, "located_in", &v, -270));
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, Op::Assert), (-270, Op::Retract)]
    );
}

#[test]
fn a_second_correction_via_the_revised_entry_id_resolves_to_the_same_chain() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let attr = attr_of(&w, ent);
    let first = w.correct(ent, rep(None, t(-280), None)).unwrap();
    let rev1 = first.entries[1];
    let r = w.correct(rev1, rep(None, t(-270), None)).unwrap();
    assert_eq!(r.changed, 2);
    assert_eq!(
        deleted(&w, r.entries[0]),
        (ent, 2, e.greg, attr, v.clone(), Tick(-280)),
        "`of` is the base, never the revision entry"
    );
    assert_eq!(revised(&w, r.entries[1]).of, ent);
    // and a third one via the newest Revised id: rev 3
    let r3 = w.correct(r.entries[1], rep(None, t(-260), None)).unwrap();
    assert_eq!(deleted(&w, r3.entries[0]).1, 3);
    assert_eq!(revised(&w, r3.entries[1]).rev, 3);
    assert!(!holds(&w, e.greg, "located_in", &v, -260));
    assert!(holds(&w, e.greg, "located_in", &v, -261));
}

#[test]
fn delete_after_a_replace_deletes_the_current_revision() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let attr = attr_of(&w, ent);
    let first = w.correct(ent, rep(None, t(-280), None)).unwrap();
    let canon = w.latest_tx();
    let r = w.correct(ent, Correction::Delete).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-280)));
    assert_eq!(
        deleted(&w, only_entry(&r)),
        (ent, 2, e.greg, attr, v.clone(), Tick(-280))
    );
    // the retract is gone altogether: Greg is in Varrow for good
    for tk in [-300, -280, -250, 0, i64::MAX] {
        assert!(holds(&w, e.greg, "located_in", &v, tk), "tick {tk}");
    }
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, Op::Assert)]
    );
    // canon just before the delete still sees the replaced retract
    assert!(!holds_at(
        &w.at_canon(canon),
        e.greg,
        "located_in",
        &v,
        -280
    ));
    // already deleted, whichever id of the chain is passed
    assert_eq!(
        w.correct(ent, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(ent)
    );
    assert_eq!(
        w.correct(first.entries[1], rep(None, t(-1), None))
            .unwrap_err(),
        Error::AlreadyDeleted(first.entries[1])
    );
}

#[test]
fn a_replace_of_a_deleted_chain_is_already_deleted_and_appends_nothing() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    w.correct(a, Correction::Delete).unwrap();
    let tx = w.latest_tx();
    let dump = w.index_dump();
    assert_eq!(
        w.correct(a, rep(None, t(-1), None)).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.index_dump(), dump);
}

#[test]
fn replace_of_non_fact_and_unknown_entries_errors_like_delete() {
    let (mut w, _e) = mem();
    assert!(matches!(
        w.correct(EntryId(1_000_000), rep(None, t(1), None)),
        Err(Error::UnknownEntry(EntryId(1_000_000)))
    ));
    assert!(matches!(
        w.correct(EntryId(1), rep(None, t(1), None)),
        Err(Error::NotAFact(EntryId(1)))
    ));
}

// ------------------------------------------------------------ same-tick rule

#[test]
fn moving_an_assert_onto_a_key_with_the_opposite_op_is_a_same_tick_conflict() {
    let (mut w, e) = mem();
    let (a, ent) = greg_walk(&mut w, &e);
    let tx = w.latest_tx();
    let dump = w.index_dump();
    let err = w.correct(a, rep(None, t(-250), None)).unwrap_err();
    assert!(
        matches!(&err, Error::SameTickConflict { entity, attr, tick }
            if *entity == e.greg && attr == "located_in" && *tick == Tick(-250)),
        "{err:?}"
    );
    // and the converse: the retract onto the assert's tick
    let err = w.correct(ent, rep(None, t(-300), None)).unwrap_err();
    assert!(matches!(err, Error::SameTickConflict { .. }), "{err:?}");
    assert_eq!(w.latest_tx(), tx, "nothing appended");
    assert_eq!(w.index_dump(), dump);
    // the chain is still correctable afterwards (the failed call did nothing)
    let r = w.correct(a, rep(None, t(-290), None)).unwrap();
    assert_eq!(r.changed, 2);
}

#[test]
fn a_same_tick_conflict_poisons_a_txn() {
    let (mut w, e) = mem();
    let (a, _) = greg_walk(&mut w, &e);
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    assert!(matches!(
        tx.correct(a, rep(None, t(-250), None)),
        Err(Error::SameTickConflict { .. })
    ));
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
}

#[test]
fn moving_onto_a_key_that_already_holds_the_same_op_writes_only_the_deleted() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r1 = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    w.assert(e.greg, "located_in", v.clone(), t(-200)).unwrap();
    w.retract(e.greg, "located_in", v.clone(), t(-150)).unwrap();
    let before = w.latest_tx();
    let attr = attr_of(&w, r1);

    // the retract @-250 moves onto the retract already at -150
    let r = w.correct(r1, rep(None, t(-150), None)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-150));
    assert_eq!(
        deleted(&w, only_entry(&r)),
        (r1, 1, e.greg, attr, v.clone(), Tick(-250))
    );
    // Greg now stays in Varrow from -300 until -150
    assert!(holds(&w, e.greg, "located_in", &v, -240));
    assert!(holds(&w, e.greg, "located_in", &v, -160));
    assert!(!holds(&w, e.greg, "located_in", &v, -150));
    assert!(!holds_at(
        &w.at_canon(before),
        e.greg,
        "located_in",
        &v,
        -240
    ));
}

#[test]
fn a_key_whose_current_entry_is_deleted_does_not_block_a_move_onto_it() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.sword);
    w.assert(e.greg, "owns", v.clone(), t(-300)).unwrap();
    let r = only_entry(&w.retract(e.greg, "owns", v.clone(), t(-250)).unwrap());
    let a2 = only_entry(&w.assert(e.greg, "owns", v.clone(), t(-200)).unwrap());
    w.correct(a2, Correction::Delete).unwrap();
    // the key (owns sword @-200) is deleted: the retract may move there
    let out = w.correct(r, rep(None, t(-200), None)).unwrap();
    assert_eq!(out.changed, 2);
    assert!(holds(&w, e.greg, "owns", &v, -201));
    assert!(!holds(&w, e.greg, "owns", &v, -200));
}

#[test]
fn hold_based_no_op_rules_do_not_apply_to_corrections() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    // retract moved to -400, where the value isn't held (a plain retract
    // there would be "0 changed"): stored anyway
    let r = w.correct(ent, rep(None, t(-400), None)).unwrap();
    assert_eq!((r.changed, r.entries.len()), (2, 2));
    assert_eq!(revised(&w, r.entries[1]).tick, Tick(-400));
    assert!(holds(&w, e.greg, "located_in", &v, -300));
    assert!(!holds(&w, e.greg, "located_in", &v, -400));

    // an assert moved to a tick where the value is already held: stored too
    let (mut w2, e2) = mem();
    let v2 = Value::Ref(e2.varrow);
    w2.assert(e2.greg, "located_in", v2.clone(), t(-300))
        .unwrap();
    w2.retract(e2.greg, "located_in", v2.clone(), t(-250))
        .unwrap();
    let a2 = only_entry(
        &w2.assert(e2.greg, "located_in", v2.clone(), t(-200))
            .unwrap(),
    );
    let r = w2.correct(a2, rep(None, t(-280), None)).unwrap();
    assert_eq!(r.changed, 2, "assert at -280 while held since -300");
    assert!(holds(&w2, e2.greg, "located_in", &v2, -279));
    assert!(!holds(&w2, e2.greg, "located_in", &v2, -250));
}

// ------------------------------------------------------------ validation

fn assert_untouched(w: &World, tx: TxId, dump: &IndexDump) {
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(&w.index_dump(), dump);
}

#[test]
fn replace_with_a_wrong_typed_value_is_wrong_value_type() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(1))
            .unwrap(),
    );
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    let err = w
        .correct(a, rep(Some(Value::Bool(true)), None, None))
        .unwrap_err();
    assert!(
        matches!(&err, Error::WrongValueType { attr, expected: ValueType::Ref, got: ValueType::Bool }
            if attr == "located_in"),
        "{err:?}"
    );
    assert_untouched(&w, tx, &dump);
}

#[test]
fn replace_with_nan_is_rejected() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "height", Value::F64(1.5), t(1)).unwrap());
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    let err = w
        .correct(a, rep(Some(Value::F64(f64::NAN)), None, None))
        .unwrap_err();
    assert_eq!(err, Error::Value(ValueError::NaN));
    assert_untouched(&w, tx, &dump);
}

#[test]
fn replace_with_an_indexed_string_over_256_bytes_is_rejected_but_257_vs_256_is_the_edge() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(1)).unwrap());
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    let err = w
        .correct(a, rep(Some(Value::Str("a".repeat(257))), None, None))
        .unwrap_err();
    assert!(
        matches!(err, Error::Value(ValueError::StringTooLong { len: 257 })),
        "{err:?}"
    );
    assert_untouched(&w, tx, &dump);
    // exactly 256 bytes is fine
    let r = w
        .correct(a, rep(Some(Value::Str("a".repeat(256))), None, None))
        .unwrap();
    assert_eq!(r.changed, 2);
}

#[test]
fn replace_with_a_long_string_on_an_unindexed_attribute_is_fine() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "bio", s("short"), t(1)).unwrap());
    let long = Value::Str("b".repeat(5000));
    let r = w.correct(a, rep(Some(long.clone()), None, None)).unwrap();
    assert_eq!(r.changed, 2);
    assert!(holds(&w, e.greg, "bio", &long, 1));
}

#[test]
fn replace_with_a_ref_to_an_undefined_entity_is_unknown_entity() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(1))
            .unwrap(),
    );
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    let ghost = EntityId::new(NodeTypeId(0), 999).unwrap();
    let err = w
        .correct(a, rep(Some(Value::Ref(ghost)), None, None))
        .unwrap_err();
    assert_eq!(err, Error::UnknownEntity(ghost));
    assert_untouched(&w, tx, &dump);
}

#[test]
fn replace_to_op_retract_on_an_event_is_retract_event() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "bought", Value::Ref(e.sword), t(-208))
            .unwrap(),
    );
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    let err = w
        .correct(a, rep(None, None, Some(Op::Retract)))
        .unwrap_err();
    assert_eq!(err, Error::RetractEvent("bought".into()));
    // also combined with a move
    let err = w
        .correct(a, rep(None, t(-300), Some(Op::Retract)))
        .unwrap_err();
    assert_eq!(err, Error::RetractEvent("bought".into()));
    assert_untouched(&w, tx, &dump);
}

#[test]
fn replace_on_an_event_moving_its_tick_is_allowed() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.sword);
    let a = only_entry(&w.assert(e.greg, "bought", v.clone(), t(-208)).unwrap());
    let before = w.latest_tx();
    let r = w.correct(a, rep(None, t(-210), None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-210)));
    assert!(holds(&w, e.greg, "bought", &v, -210));
    assert!(holds(&w, e.greg, "bought", &v, 0));
    assert!(!holds(&w, e.greg, "bought", &v, -211));
    let old = w.at_canon(before);
    assert!(!holds_at(&old, e.greg, "bought", &v, -209));
    assert!(holds_at(&old, e.greg, "bought", &v, -208));
    // events may also have the value replaced
    let b = w
        .correct(a, rep(Some(Value::Ref(e.varrow)), None, None))
        .unwrap();
    assert_eq!(b.changed, 2);
}

#[test]
fn a_failed_replace_in_a_txn_poisons_it() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(1)).unwrap());
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    assert!(matches!(
        tx.correct(a, rep(Some(Value::Bool(false)), None, None)),
        Err(Error::WrongValueType { .. })
    ));
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
    assert!(holds(&w, e.greg, "name", &s("Greg"), 1));
}

// ----------------------------------------------------------- bidirectional

fn sides_agree_on(w: &World, x: EntityId, y: EntityId, tick: i64, expect: bool) {
    for (p, q) in [(x, y), (y, x)] {
        assert_eq!(
            holds(w, p, "ally_of", &Value::Ref(q), tick),
            expect,
            "{p:?}->{q:?} @{tick}"
        );
        let ns = w
            .neighbors(p, "ally_of", Direction::Both, Tick(tick))
            .unwrap();
        assert_eq!(
            ns.iter().any(|n| n.0 == q),
            expect,
            "neighbors {p:?} @{tick}"
        );
        let asof = w.as_of(p, "ally_of", Tick(tick)).unwrap();
        assert_eq!(asof.contains(&Value::Ref(q)), expect);
    }
}

#[test]
fn bidirectional_replace_moving_the_tick_keeps_the_min_max_order() {
    let (mut w, e) = mem();
    // written reversed: stored (edwin, mara)
    let a = only_entry(
        &w.assert(e.mara, "ally_of", Value::Ref(e.edwin), t(-208))
            .unwrap(),
    );
    let edge = w.edge_id(e.edwin, "ally_of", e.mara).unwrap();
    let before = w.latest_tx();
    let r = w.correct(a, rep(None, t(-210), None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-210)));
    assert!(r.edges.is_empty());
    let (_, _, de, _, dv, dt) = deleted(&w, r.entries[0]);
    assert_eq!((de, dv, dt), (e.edwin, Value::Ref(e.mara), Tick(-208)));
    let rv = revised(&w, r.entries[1]);
    assert_eq!(
        (rv.entity, rv.value, rv.tick, rv.op),
        (e.edwin, Value::Ref(e.mara), Tick(-210), Op::Assert)
    );
    sides_agree_on(&w, e.edwin, e.mara, -211, false);
    sides_agree_on(&w, e.edwin, e.mara, -210, true);
    sides_agree_on(&w, e.edwin, e.mara, 0, true);
    for x in [e.edwin, e.mara] {
        assert_eq!(
            hist(&w.at_canon(w.latest_tx()), x, "ally_of"),
            vec![(-210, Op::Assert)]
        );
    }
    assert_eq!(
        w.neighbors(e.mara, "ally_of", Direction::Both, Tick(0))
            .unwrap(),
        vec![(e.edwin, edge)]
    );
    // old canon: the old tick on both sides
    let old = w.at_canon(before);
    for x in [e.edwin, e.mara] {
        assert_eq!(hist(&old, x, "ally_of"), vec![(-208, Op::Assert)]);
    }
}

#[test]
fn bidirectional_replace_changing_the_op_keeps_the_order() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.mara, "ally_of", Value::Ref(e.edwin), t(-208))
            .unwrap(),
    );
    let r = w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    assert_eq!(r.changed, 1);
    let rv = revised(&w, only_entry(&r));
    assert_eq!(
        (rv.entity, rv.value, rv.tick, rv.op),
        (e.edwin, Value::Ref(e.mara), Tick(-208), Op::Retract)
    );
    sides_agree_on(&w, e.edwin, e.mara, -208, false);
    sides_agree_on(&w, e.edwin, e.mara, 0, false);
    for x in [e.edwin, e.mara] {
        assert_eq!(
            hist(&w.at_canon(w.latest_tx()), x, "ally_of"),
            vec![(-208, Op::Retract)]
        );
    }
}

#[test]
fn bidirectional_replace_of_the_value_with_a_smaller_entity_renormalises() {
    let (mut w, e) = mem();
    // stored (edwin, mara); replacing the value with greg (< edwin) gives
    // the pair {edwin, greg}, stored (greg, edwin): entity and value swap.
    let a = only_entry(
        &w.assert(e.edwin, "ally_of", Value::Ref(e.mara), t(-208))
            .unwrap(),
    );
    let before = w.latest_tx();
    let r = w
        .correct(a, rep(Some(Value::Ref(e.greg)), None, None))
        .unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-208)));
    let (_, _, de, _, dv, _) = deleted(&w, r.entries[0]);
    assert_eq!((de, dv), (e.edwin, Value::Ref(e.mara)));
    let rv = revised(&w, r.entries[1]);
    assert_eq!((rv.entity, rv.value.clone()), (e.greg, Value::Ref(e.edwin)));
    assert!(
        rv.entity
            <= match rv.value {
                Value::Ref(x) => x,
                _ => unreachable!(),
            }
    );
    // the new triple interned a new edge, listed in `edges`; either spelling
    let edge = w.edge_id(e.greg, "ally_of", e.edwin).unwrap();
    assert_eq!(w.edge_id(e.edwin, "ally_of", e.greg), Some(edge));
    assert_eq!(r.edges, vec![edge]);
    // both sides agree
    sides_agree_on(&w, e.greg, e.edwin, -208, true);
    sides_agree_on(&w, e.edwin, e.mara, -208, false);
    assert_eq!(
        w.neighbors(e.edwin, "ally_of", Direction::Both, Tick(0))
            .unwrap(),
        vec![(e.greg, edge)]
    );
    assert!(
        w.neighbors(e.mara, "ally_of", Direction::Both, Tick(0))
            .unwrap()
            .is_empty()
    );
    for x in [e.greg, e.edwin] {
        assert_eq!(
            hist(&w.at_canon(w.latest_tx()), x, "ally_of"),
            vec![(-208, Op::Assert)]
        );
    }
    assert!(w.history(e.mara, "ally_of").unwrap().is_empty());
    // old canon unchanged: edwin & mara
    let old = w.at_canon(before);
    assert_eq!(hist(&old, e.mara, "ally_of"), vec![(-208, Op::Assert)]);
    assert!(hist(&old, e.greg, "ally_of").is_empty());
}

#[test]
fn bidirectional_replace_of_the_value_with_a_larger_entity_keeps_the_entity() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "ally_of", Value::Ref(e.edwin), t(5))
            .unwrap(),
    );
    let r = w
        .correct(a, rep(Some(Value::Ref(e.mara)), None, None))
        .unwrap();
    let rv = revised(&w, r.entries[1]);
    assert_eq!((rv.entity, rv.value), (e.greg, Value::Ref(e.mara)));
    assert_eq!(r.edges.len(), 1);
    sides_agree_on(&w, e.greg, e.mara, 5, true);
    sides_agree_on(&w, e.greg, e.edwin, 5, false);
}

#[test]
fn bidirectional_replace_to_a_self_pair_and_back() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.edwin, "ally_of", Value::Ref(e.mara), t(5))
            .unwrap(),
    );
    let r = w
        .correct(a, rep(Some(Value::Ref(e.edwin)), None, None))
        .unwrap();
    let rv = revised(&w, r.entries[1]);
    assert_eq!((rv.entity, rv.value), (e.edwin, Value::Ref(e.edwin)));
    assert!(holds(&w, e.edwin, "ally_of", &Value::Ref(e.edwin), 5));
    assert!(w.as_of(e.mara, "ally_of", Tick(5)).unwrap().is_empty());
    let back = w
        .correct(a, rep(Some(Value::Ref(e.mara)), None, None))
        .unwrap();
    assert_eq!(back.changed, 2);
    assert!(
        back.edges.is_empty(),
        "the (edwin, mara) edge already exists"
    );
    sides_agree_on(&w, e.edwin, e.mara, 5, true);
}

#[test]
fn bidirectional_replace_onto_an_existing_pair_obeys_the_same_tick_rule() {
    let (mut w, e) = mem();
    // greg-edwin allied @5, edwin-mara allied @5 then retracted @9
    w.assert(e.greg, "ally_of", Value::Ref(e.edwin), t(5))
        .unwrap();
    let a = only_entry(
        &w.assert(e.edwin, "ally_of", Value::Ref(e.mara), t(5))
            .unwrap(),
    );
    // moving edwin-mara to greg-edwin @5: the same op already current there
    let r = w
        .correct(a, rep(Some(Value::Ref(e.greg)), None, None))
        .unwrap();
    assert_eq!(r.changed, 1, "only the Deleted is written");
    assert!(matches!(
        w.entry(only_entry(&r)).unwrap().body,
        EntryBody::Deleted { .. }
    ));
    sides_agree_on(&w, e.greg, e.edwin, 5, true);
    sides_agree_on(&w, e.edwin, e.mara, 5, false);
}

// ------------------------------------------------------------ tx ordering

#[test]
fn index_rows_at_a_key_with_base_and_revised_entries_are_tx_descending() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let attr = attr_of(&w, a);
    w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    w.correct(a, rep(None, None, Some(Op::Assert))).unwrap();
    let d = w.index_dump();
    let erows: Vec<_> =
        d.e.iter()
            .filter(|r| r.0 == e.greg && r.1 == attr && r.2 == v && r.3 == Tick(-300))
            .collect();
    assert_eq!(erows.len(), 3, "{erows:?}");
    assert!(
        erows[0].4 > erows[1].4 && erows[1].4 > erows[2].4,
        "{erows:?}"
    );
    assert_eq!(
        erows.iter().map(|r| r.5).collect::<Vec<_>>(),
        vec![Mark::Assert, Mark::Retract, Mark::Assert]
    );
    let rrows: Vec<_> =
        d.r.iter()
            .filter(|r| r.0 == e.varrow && r.1 == attr && r.2 == e.greg && r.3 == Tick(-300))
            .collect();
    assert_eq!(
        rrows.iter().map(|r| (r.4, r.5)).collect::<Vec<_>>(),
        erows.iter().map(|r| (r.4, r.5)).collect::<Vec<_>>()
    );
}

#[test]
fn old_canon_reads_the_base_at_a_key_with_revised_entries() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let c0 = w.latest_tx();
    w.correct(a, rep(None, None, Some(Op::Retract))).unwrap();
    let c1 = w.latest_tx();
    w.correct(a, rep(None, None, Some(Op::Assert))).unwrap();
    let c2 = w.latest_tx();
    assert!(holds_at(&w.at_canon(c0), e.greg, "located_in", &v, -300));
    assert!(!holds_at(&w.at_canon(c1), e.greg, "located_in", &v, -300));
    assert!(holds_at(&w.at_canon(c2), e.greg, "located_in", &v, -300));
    assert_eq!(
        hist(&w.at_canon(c1), e.greg, "located_in"),
        vec![(-300, Op::Retract)]
    );
}

#[test]
fn a_moved_replace_writes_deleted_rows_at_the_old_key_and_a_row_at_the_new_one() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let attr = attr_of(&w, ent);
    w.correct(ent, rep(None, t(-280), None)).unwrap();
    let d = w.index_dump();
    let at = |tick: i64| -> Vec<Mark> {
        d.e.iter()
            .filter(|r| r.0 == e.greg && r.1 == attr && r.2 == v && r.3 == Tick(tick))
            .map(|r| r.5)
            .collect()
    };
    assert_eq!(at(-250), vec![Mark::Deleted, Mark::Retract]);
    assert_eq!(at(-280), vec![Mark::Retract]);
    assert_eq!(at(-300), vec![Mark::Assert]);
    let rat = |tick: i64| -> Vec<Mark> {
        d.r.iter()
            .filter(|r| r.0 == e.varrow && r.1 == attr && r.2 == e.greg && r.3 == Tick(tick))
            .map(|r| r.5)
            .collect()
    };
    assert_eq!(rat(-250), vec![Mark::Deleted, Mark::Retract]);
    assert_eq!(rat(-280), vec![Mark::Retract]);
}

// ---------------------------------------------------------- transactions

#[test]
fn txn_replace_reports_changed_and_tick_and_the_commit_lists_deleted_then_revised() {
    let (mut w, e) = mem();
    let (_, ent) = greg_walk(&mut w, &e);
    let before = w.latest_tx();
    let mut tx = w.begin();
    let r = tx.correct(ent, rep(None, t(-280), None)).unwrap();
    assert_eq!((r.changed, r.tick), (2, Tick(-280)));
    assert!(r.entries.is_empty(), "entry IDs are assigned at commit");
    assert!(r.edges.is_empty());
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 2);
    assert_eq!(c.tx, TxId(before.0 + 1));
    assert_eq!(c.entries.len(), 2);
    assert!(matches!(
        w.entry(c.entries[0]).unwrap().body,
        EntryBody::Deleted { of, rev: 1, .. } if of == ent
    ));
    assert!(matches!(
        w.entry(c.entries[1]).unwrap().body,
        EntryBody::Revised { of, rev: 1, .. } if of == ent
    ));
    assert!(c.edges.is_empty());
}

#[test]
fn txn_replace_with_a_new_triple_lists_the_edge_in_the_commit_result() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(1))
            .unwrap(),
    );
    let mut tx = w.begin();
    tx.correct(a, rep(Some(Value::Ref(e.ashford)), None, None))
        .unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(
        c.edges,
        vec![w.edge_id(e.greg, "located_in", e.ashford).unwrap()]
    );
}

#[test]
fn txn_mixing_an_assert_a_replace_and_another_assert_is_atomic() {
    let (mut w, e) = mem();
    let (_, ent) = greg_walk(&mut w, &e);
    let before = w.latest_tx();
    let mut tx = w.begin();
    tx.assert(e.mara, "name", s("Mara"), t(1)).unwrap();
    tx.correct(ent, rep(None, t(-280), None)).unwrap();
    tx.assert(e.edwin, "name", s("Edwin"), t(2)).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!((c.changed, c.tx), (4, TxId(before.0 + 1)));
    assert_eq!(c.entries.len(), 4);
    assert!(c.entries.windows(2).all(|p| p[0].0 < p[1].0), "write order");
    assert!(matches!(
        w.entry(c.entries[0]).unwrap().body,
        EntryBody::Fact { .. }
    ));
    assert!(matches!(
        w.entry(c.entries[2]).unwrap().body,
        EntryBody::Revised { .. }
    ));
}

#[test]
fn correcting_the_same_chain_twice_in_one_txn_is_txn_key_conflict() {
    let (mut w, e) = mem();
    let (_, ent) = greg_walk(&mut w, &e);
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    tx.correct(ent, rep(None, t(-280), None)).unwrap();
    let err = tx.correct(ent, rep(None, t(-270), None)).unwrap_err();
    assert!(
        matches!(&err, Error::TxnKeyConflict { entity, attr, .. }
            if *entity == e.greg && attr == "located_in"),
        "{err:?}"
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
    // aborted: the world is exactly greg_walk's (assert @-300, retract @-250)
    let v = Value::Ref(e.varrow);
    assert!(holds(&w, e.greg, "located_in", &v, -280));
    assert!(holds(&w, e.greg, "located_in", &v, -260));
    assert!(!holds(&w, e.greg, "located_in", &v, -250));
}

#[test]
fn a_replace_then_a_delete_or_a_delete_then_a_replace_of_one_chain_in_a_txn_conflicts() {
    for delete_first in [false, true] {
        let (mut w, e) = mem();
        let (_, ent) = greg_walk(&mut w, &e);
        let tx0 = w.latest_tx();
        let mut tx = w.begin();
        let second = if delete_first {
            tx.correct(ent, Correction::Delete).unwrap();
            tx.correct(ent, rep(None, t(-280), None))
        } else {
            tx.correct(ent, rep(None, t(-280), None)).unwrap();
            tx.correct(ent, Correction::Delete)
        };
        assert!(
            matches!(second, Err(Error::TxnKeyConflict { .. })),
            "delete_first={delete_first}: {second:?}"
        );
        assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
        assert_eq!(w.latest_tx(), tx0);
    }
}

#[test]
fn a_dropped_txn_with_a_replace_changes_nothing() {
    let (mut w, e) = mem();
    let (_, ent) = greg_walk(&mut w, &e);
    let (tx0, dump) = (w.latest_tx(), w.index_dump());
    {
        let mut tx = w.begin();
        tx.correct(ent, rep(None, t(-280), None)).unwrap();
    }
    assert_untouched(&w, tx0, &dump);
    // and the chain is still at rev 0: the next real correction is rev 1
    let r = w.correct(ent, rep(None, t(-280), None)).unwrap();
    assert_eq!(deleted(&w, r.entries[0]).1, 1);
}

// ------------------------------------------------------------ file-backed

fn probe(w: &World, e: &Ents, canon: TxId) -> String {
    let v = w.at_canon(canon);
    let mut out = String::new();
    for tk in [
        i64::MIN,
        -300,
        -290,
        -280,
        -270,
        -260,
        -250,
        -208,
        0,
        i64::MAX,
    ] {
        out += &format!(
            "{tk}: loc={:?} ally_e={:?} ally_m={:?} name={:?} out={:?} in={:?}\n",
            v.as_of(e.greg, "located_in", Tick(tk)),
            v.as_of(e.edwin, "ally_of", Tick(tk)),
            v.as_of(e.mara, "ally_of", Tick(tk)),
            v.as_of(e.greg, "name", Tick(tk)),
            v.neighbors(e.greg, "located_in", Direction::Out, Tick(tk)),
            v.neighbors(e.varrow, "located_in", Direction::In, Tick(tk)),
        );
    }
    out += &format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n",
        v.history(e.greg, "located_in"),
        v.history(e.greg, "name"),
        v.history(e.edwin, "ally_of"),
        v.history(e.greg, "ally_of")
    );
    out
}

/// Greg's walk with the retract moved twice (rev 1, rev 2), a renamed name,
/// and an ally pair whose value is replaced with a smaller entity. Returns
/// every Revised entry id.
fn script(w: &mut World, e: &Ents) -> Vec<EntryId> {
    let (_, ent) = greg_walk(w, e);
    let mut revised_ids = Vec::new();
    let r1 = w.correct(ent, rep(None, t(-280), None)).unwrap();
    revised_ids.push(r1.entries[1]);
    let r2 = w.correct(r1.entries[1], rep(None, t(-270), None)).unwrap();
    revised_ids.push(r2.entries[1]);
    let n = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let r3 = w.correct(n, rep(Some(s("Gregor")), None, None)).unwrap();
    revised_ids.push(r3.entries[1]);
    let ally = only_entry(
        &w.assert(e.mara, "ally_of", Value::Ref(e.edwin), t(-100))
            .unwrap(),
    );
    let r4 = w
        .correct(ally, rep(Some(Value::Ref(e.greg)), t(-110), None))
        .unwrap();
    revised_ids.push(r4.entries[1]);
    let r5 = w.correct(ally, rep(None, None, Some(Op::Retract))).unwrap();
    revised_ids.push(only_entry(&r5));
    revised_ids
}

#[test]
fn file_log_holds_kind_8_revised_entries_and_reads_match_entry() {
    let (_d, path, mut w, e) = file();
    let ids = script(&mut w, &e);
    assert_eq!(ids.len(), 5);
    let log = read_log(&path).unwrap();
    for id in &ids {
        let from_log = &log[(id.0 - 1) as usize];
        assert_eq!(from_log.id, *id);
        assert!(
            matches!(from_log.body, EntryBody::Revised { .. }),
            "{:?}",
            from_log.body
        );
        assert_eq!(w.entry(*id).unwrap().body, from_log.body);
    }
    // a moved replace: Deleted directly before its Revised, same of/rev
    for id in &ids[..4] {
        match (
            &log[(id.0 - 2) as usize].body,
            &log[(id.0 - 1) as usize].body,
        ) {
            (
                EntryBody::Deleted {
                    of: o1, rev: r1, ..
                },
                EntryBody::Revised {
                    of: o2, rev: r2, ..
                },
            ) => assert_eq!((o1, r1), (o2, r2)),
            other => panic!("{other:?}"),
        }
    }
    for (i, le) in log.iter().enumerate() {
        assert_eq!(le.id.0, i as u64 + 1, "ids are gap-free");
    }
}

#[test]
fn reopen_gives_the_same_reads_at_latest_and_old_canon_and_the_same_entries() {
    let (_d, path, mut w, e) = file();
    let before_script = w.latest_tx();
    let ids = script(&mut w, &e);
    let latest = w.latest_tx();
    let last_entry = read_log(&path).unwrap().last().unwrap().id.0;
    let canons: Vec<TxId> = (before_script.0..=latest.0).map(TxId).collect();
    let reads: Vec<String> = canons.iter().map(|c| probe(&w, &e, *c)).collect();
    let entries: Vec<_> = (0..=last_entry + 1).map(|i| w.entry(EntryId(i))).collect();
    let dump = w.index_dump();
    drop(w);

    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), latest);
    assert_eq!(w.recovery().bytes_cut, 0);
    for (c, r) in canons.iter().zip(&reads) {
        assert_eq!(&probe(&w, &e, *c), r, "canon {c:?}");
    }
    let again: Vec<_> = (0..=last_entry + 1).map(|i| w.entry(EntryId(i))).collect();
    assert_eq!(again, entries);
    assert_eq!(w.index_dump(), dump);
    for id in &ids {
        assert!(matches!(
            w.entry(*id).unwrap().body,
            EntryBody::Revised { .. }
        ));
    }
    // the chains continue after reopen: a further correction is the next rev
    let base = match w.entry(ids[1]).unwrap().body {
        EntryBody::Revised { of, rev, .. } => {
            assert_eq!(rev, 2);
            of
        }
        b => panic!("{b:?}"),
    };
    let r = w.correct(base, rep(None, t(-265), None)).unwrap();
    assert_eq!(deleted(&w, r.entries[0]).1, 3);
    assert_eq!(r.entries[0], EntryId(last_entry + 1));
}

#[test]
fn in_memory_and_file_worlds_give_identical_results_and_entries_for_a_replace_script() {
    let (_d, path, mut fw, fe) = file();
    let (mut mw, me) = mem();
    let fi = script(&mut fw, &fe);
    let mi = script(&mut mw, &me);
    assert_eq!(fi, mi);
    let n = read_log(&path).unwrap().last().unwrap().id.0;
    for i in 0..=n + 1 {
        assert_eq!(fw.entry(EntryId(i)), mw.entry(EntryId(i)), "entry#{i}");
    }
    assert_eq!(fw.latest_tx(), mw.latest_tx());
    for c in 0..=fw.latest_tx().0 {
        assert_eq!(
            probe(&fw, &fe, TxId(c)),
            probe(&mw, &me, TxId(c)),
            "canon {c}"
        );
    }
    assert_eq!(fw.index_dump(), mw.index_dump());
}

#[test]
fn an_auto_commit_replace_survives_a_failed_sync_as_nothing() {
    let (_d, path, mut w, e) = file();
    let (_, ent) = greg_walk(&mut w, &e);
    let tx = w.latest_tx();
    let n = read_log(&path).unwrap().last().unwrap().id.0;
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(matches!(
        w.correct(ent, rep(None, t(-280), None)),
        Err(Error::Io { .. })
    ));
    assert_eq!(w.entry(EntryId(n + 1)), None);
    drop(w);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), tx);
    assert!(holds(&w, e.greg, "located_in", &Value::Ref(e.varrow), -260));
    let r = w.correct(ent, rep(None, t(-280), None)).unwrap();
    assert_eq!(r.entries, vec![EntryId(n + 1), EntryId(n + 2)]);
}

// ------------------------------------------------- item 3 confirm: live edges

/// assert @-300 (a), retract @-250 (r), assert @-200 (a2), retract @-150 (r2)
fn greg_two_stays(w: &mut World, e: &Ents) -> [EntryId; 4] {
    let v = Value::Ref(e.varrow);
    let a = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap());
    let r = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let a2 = only_entry(&w.assert(e.greg, "located_in", v.clone(), t(-200)).unwrap());
    let r2 = only_entry(&w.retract(e.greg, "located_in", v, t(-150)).unwrap());
    [a, r, a2, r2]
}

#[test]
fn a_staged_plain_fact_then_a_move_onto_its_key_with_the_opposite_op_is_a_same_tick_conflict() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let (_, ent) = greg_walk(&mut w, &e);
    let tx0 = w.latest_tx();
    let dump = w.index_dump();
    let mut tx = w.begin();
    // not held at -100 (retracted @-250), so this assert is stored
    tx.assert(e.greg, "located_in", v, t(-100)).unwrap();
    let err = tx.correct(ent, rep(None, t(-100), None)).unwrap_err();
    assert!(
        matches!(&err, Error::SameTickConflict { entity, attr, tick }
            if *entity == e.greg && attr == "located_in" && *tick == Tick(-100)),
        "{err:?}"
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_untouched(&w, tx0, &dump);
}

#[test]
fn a_staged_plain_fact_then_a_move_onto_its_key_with_the_same_op_writes_only_the_deleted() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    let [_, r, _, _] = greg_two_stays(&mut w, &e);
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    // Greg holds Varrow at -100 (assert @-200), so this retract is stored
    tx.retract(e.greg, "located_in", v.clone(), t(-160))
        .unwrap();
    let wr = tx.correct(r, rep(None, t(-160), None)).unwrap();
    assert_eq!((wr.changed, wr.tick), (1, Tick(-160)));
    let c = tx.commit().unwrap();
    assert_eq!(c.tx, TxId(tx0.0 + 1));
    assert_eq!(c.changed, 2, "the plain retract + the Deleted");
    assert_eq!(c.entries.len(), 2);
    assert!(matches!(
        w.entry(c.entries[0]).unwrap().body,
        EntryBody::Fact { .. }
    ));
    assert_eq!(deleted(&w, c.entries[1]).0, r);
    assert!(
        holds(&w, e.greg, "located_in", &v, -240),
        "-250 retract is gone"
    );
    assert!(!holds(&w, e.greg, "located_in", &v, -160));
}

#[test]
fn deleting_chain_a_then_moving_chain_b_onto_its_key_is_a_txn_key_conflict() {
    let (mut w, e) = mem();
    let [_, r, _, r2] = greg_two_stays(&mut w, &e);
    let tx0 = w.latest_tx();
    let dump = w.index_dump();
    let mut tx = w.begin();
    tx.correct(r, Correction::Delete).unwrap(); // Deleted @(varrow, -250)
    let err = tx.correct(r2, rep(None, t(-250), None)).unwrap_err();
    assert!(
        matches!(&err, Error::TxnKeyConflict { entity, attr, tick }
            if *entity == e.greg && attr == "located_in" && *tick == Tick(-250)),
        "{err:?}"
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_untouched(&w, tx0, &dump);

    // positive control: the same two corrections in separate commits work
    w.correct(r, Correction::Delete).unwrap();
    let out = w.correct(r2, rep(None, t(-250), None)).unwrap();
    assert_eq!(
        out.changed, 2,
        "key (varrow,-250) is Deleted, so it is free"
    );
}

#[test]
fn a_revised_retract_on_a_triple_with_no_edge_interns_the_edge_when_later_made_an_assert() {
    let (_d, path, mut w, e) = file();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(-300))
            .unwrap(),
    );
    assert_eq!(w.edge_id(e.greg, "located_in", e.ashford), None);
    // value -> Ashford AND op -> retract: a retract of a never-asserted triple
    let r1 = w
        .correct(a, rep(Some(Value::Ref(e.ashford)), None, Some(Op::Retract)))
        .unwrap();
    assert_eq!(r1.changed, 2);
    assert!(r1.edges.is_empty(), "a retract interns no edge");
    assert_eq!(w.edge_id(e.greg, "located_in", e.ashford), None);

    // now the op becomes Assert: the edge is interned then
    let r2 = w.correct(a, rep(None, None, Some(Op::Assert))).unwrap();
    assert_eq!(r2.changed, 1);
    assert_eq!(r2.edges.len(), 1);
    let edge = w.edge_id(e.greg, "located_in", e.ashford).unwrap();
    assert_eq!(r2.edges, vec![edge]);
    assert_eq!(
        w.edge(edge),
        Some((e.greg, "located_in".to_string(), e.ashford))
    );
    assert!(holds(
        &w,
        e.greg,
        "located_in",
        &Value::Ref(e.ashford),
        -300
    ));
    drop(w);

    // replay: the DefineEdge is satisfied by the Revised assert alone
    let w2 = World::open(&path).unwrap();
    assert_eq!(w2.edge_id(e.greg, "located_in", e.ashford), Some(edge));
    assert!(holds(
        &w2,
        e.greg,
        "located_in",
        &Value::Ref(e.ashford),
        -300
    ));
}

#[test]
fn moves_to_the_tick_extremes_and_zero_keep_reads_and_canon_correct() {
    for target in [i64::MIN, i64::MAX, 0] {
        for move_assert in [false, true] {
            let (mut w, e) = mem();
            let v = Value::Ref(e.varrow);
            let (a, r) = greg_walk(&mut w, &e);
            let old = w.latest_tx();
            let victim = if move_assert { a } else { r };
            let out = w.correct(victim, rep(None, t(target), None)).unwrap();
            assert_eq!((out.changed, out.tick), (2, Tick(target)), "{target}");
            let ticks = |w: &World| -> Vec<(i64, Op)> {
                hist(&w.at_canon(w.latest_tx()), e.greg, "located_in")
            };
            let expect_op = if move_assert { Op::Assert } else { Op::Retract };
            let other = if move_assert {
                (-250, Op::Retract)
            } else {
                (-300, Op::Assert)
            };
            let mut want = vec![other, (target, expect_op)];
            want.sort_by_key(|(tk, _)| *tk);
            let mut got = ticks(&w);
            got.sort_by_key(|(tk, _)| *tk);
            assert_eq!(got, want, "target {target} move_assert {move_assert}");
            // reads at the extremes follow latest-wins on the new layout
            for probe_tick in [i64::MIN, -300, -250, -1, 0, 1, i64::MAX] {
                let mut latest: Option<Op> = None;
                let mut best = i64::MIN;
                for (tk, op) in &want {
                    if *tk <= probe_tick && (latest.is_none() || *tk >= best) {
                        best = *tk;
                        latest = Some(*op);
                    }
                }
                assert_eq!(
                    holds(&w, e.greg, "located_in", &v, probe_tick),
                    latest == Some(Op::Assert),
                    "target {target} move_assert {move_assert} probe {probe_tick}"
                );
            }
            // the old canon still sees the original walk
            let oldv = w.at_canon(old);
            assert!(holds_at(&oldv, e.greg, "located_in", &v, -260));
            assert!(!holds_at(&oldv, e.greg, "located_in", &v, -250));
        }
    }
}

#[test]
fn latest_tick_follows_a_move_of_the_newest_fact_earlier_or_later() {
    let (mut w, e) = mem();
    let (_, r) = greg_walk(&mut w, &e);
    assert_eq!(w.latest_tick(), Tick(-250));
    // later
    let r1 = w.correct(r, rep(None, t(500), None)).unwrap();
    assert_eq!(w.latest_tick(), Tick(500));
    // earlier, but still above the assert @-300
    w.correct(r, rep(None, t(-280), None)).unwrap();
    assert_eq!(w.latest_tick(), Tick(-280));
    // earlier than every other fact: falls to the assert's tick
    w.correct(r, rep(None, t(-400), None)).unwrap();
    assert_eq!(w.latest_tick(), Tick(-300));
    // a tick-less write now lands there
    let out = w
        .assert(e.greg, "located_in", Value::Ref(e.ashford), None)
        .unwrap();
    assert_eq!(out.tick, Tick(-300));
    // and the older canon's data is unaffected by the reads of latest_tick
    assert_eq!(r1.tick, Tick(500));
}

#[test]
fn latest_tick_after_a_move_that_writes_only_the_deleted() {
    let (mut w, e) = mem();
    let [_, r, _, _] = greg_two_stays(&mut w, &e);
    assert_eq!(w.latest_tick(), Tick(-150));
    // moving the retract @-250 onto @-150 leaves the highest tick as it was
    w.correct(r, rep(None, t(-150), None)).unwrap();
    assert_eq!(w.latest_tick(), Tick(-150));
}

#[test]
fn a_chain_whose_move_wrote_only_the_deleted_is_already_deleted_for_every_correction() {
    let (mut w, e) = mem();
    let [_, r, _, _] = greg_two_stays(&mut w, &e);
    let out = w.correct(r, rep(None, t(-150), None)).unwrap();
    assert_eq!((out.changed, out.entries.len()), (1, 1));
    let (tx, dump) = (w.latest_tx(), w.index_dump());
    for c in [
        Correction::Delete,
        rep(None, t(-100), None),
        rep(None, None, Some(Op::Assert)),
        rep(None, None, None),
    ] {
        assert_eq!(
            w.correct(r, c.clone()).unwrap_err(),
            Error::AlreadyDeleted(r),
            "{c:?}"
        );
    }
    // also through a txn, which poisons it
    let mut t1 = w.begin();
    assert_eq!(
        t1.correct(r, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(r)
    );
    assert_eq!(t1.commit().unwrap_err(), Error::TransactionAborted);
    assert_untouched(&w, tx, &dump);
}
