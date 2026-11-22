//! Slice 3, item 2: `correct(entry, Delete)` (docs/slice-3-decisions.md,
//! "Core model of corrections", "Item 2", and in "Item 4" the log format v3 +
//! entry kind 7 `Deleted`). RED until the implementation lands.
//!
//! API expected (all re-exported at the crate root; `EntryBody` / `EntryId` /
//! `read_log` / `LOG_FORMAT_VERSION` come from `kala_core::log`):
//!
//! ```text
//! pub enum Correction { Delete, Replace { value: Option<Value>, tick: Option<Tick>, op: Option<Op> } }
//! World::correct(&mut self, entry: EntryId, c: Correction) -> Result<WriteResult, Error>
//! Txn::correct(&mut self, entry: EntryId, c: Correction)   -> Result<WriteResult, Error>
//! Error::UnknownEntry(EntryId)
//! Error::NotAFact(EntryId)
//! Error::AlreadyDeleted(EntryId)                       // the id the caller passed
//! Error::TxnKeyConflict { entity: EntityId, attr: String, tick: Tick }
//! EntryBody::Deleted { of: EntryId, rev: u32, entity: EntityId, attr: AttrId, value: Value, tick: Tick }
//! EntryInfo::revision(&self) -> Option<(EntryId, u32)>   // Some((of, rev)) for Deleted
//! IndexDump rows end in `Mark` (item 8): `pub enum Mark { Assert, Retract, Deleted }`,
//!     `PartialEq<Op> for Mark`, re-exported at the root. Used here only to see the
//!     DELETED row in `index_dump` (the one place this file leans on item 8).
//! LOG_FORMAT_VERSION == 3
//! ```
//!
//! Assumptions that follow the contract's wording and are flagged in the
//! report: (1) inside a txn, a later assert/retract is judged against the
//! committed world plus the txn's staged entries (including a staged
//! Deleted), as replay rule 0c says; (2) a correction's `WriteResult.tick`
//! is the corrected key's tick.

mod common;

use kala_core::log::{EntryBody, EntryId, LOG_FORMAT_VERSION, read_log};
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
    sword: EntityId,
}

/// edwin < mara by creation order (same node type, ascending sequence).
fn populate(w: &mut World) -> Ents {
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    w.create_entity("PLACE", Some("ashford")).unwrap();
    let sword = w.create_entity("ITEM", Some("sword")).unwrap();
    assert!(edwin < mara);
    Ents {
        greg,
        edwin,
        mara,
        varrow,
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

/// `(entity, attr, value, tick)` of a base Fact entry.
fn key_of(w: &World, id: EntryId) -> (EntityId, AttrId, Value, Tick) {
    match w.entry(id).unwrap().body {
        EntryBody::Fact {
            entity,
            attr,
            value,
            tick,
            ..
        } => (entity, attr, value, tick),
        other => panic!("entry#{} is not a Fact: {other:?}", id.0),
    }
}

fn holds(w: &World, e: EntityId, a: &str, v: &Value, tick: i64) -> bool {
    w.as_of_value(e, a, v, Tick(tick)).unwrap()
}

fn holds_at(v: &View<'_>, e: EntityId, a: &str, val: &Value, tick: i64) -> bool {
    v.as_of_value(e, a, val, Tick(tick)).unwrap()
}

fn only_entry(r: &WriteResult) -> EntryId {
    assert_eq!(r.entries.len(), 1, "{r:?}");
    r.entries[0]
}

/// `(tick, op)` of every change point, for one-value attributes.
fn hist(v: &View<'_>, e: EntityId, a: &str) -> Vec<(i64, Op)> {
    v.history(e, a)
        .unwrap()
        .iter()
        .map(|c| (c.tick.0, c.op))
        .collect()
}

// ------------------------------------------------------- basic delete

#[test]
fn delete_of_a_state_assert_hides_it_from_the_correction_tx_on_and_keeps_old_canon() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let canon_before = w.latest_tx();
    let original = w.entry(a).unwrap();

    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-208), "tick = the corrected key's tick");
    let d = only_entry(&r);
    assert!(d.0 > a.0);
    assert!(r.edges.is_empty());
    assert_eq!(w.latest_tx(), TxId(canon_before.0 + 1));

    // the Deleted entry
    let info = w.entry(d).unwrap();
    assert_eq!(info.tx, w.latest_tx());
    assert_eq!(info.revision(), Some((a, 1)));
    let (entity, attr, value, tick) = key_of(&w, a);
    assert_eq!(
        info.body,
        EntryBody::Deleted {
            of: a,
            rev: 1,
            entity,
            attr,
            value,
            tick
        }
    );
    // the original is untouched
    assert_eq!(w.entry(a).unwrap(), original);
    assert_eq!(original.revision(), None);

    // latest canon: absent everywhere from the correction on
    for tk in [-208, -207, 0, i64::MAX] {
        assert!(!holds(&w, e.greg, "name", &s("Greg"), tk), "tick {tk}");
        assert!(w.as_of(e.greg, "name", Tick(tk)).unwrap().is_empty());
    }
    assert!(w.history(e.greg, "name").unwrap().is_empty());
    // old canon: still there
    let old = w.at_canon(canon_before);
    assert!(holds_at(&old, e.greg, "name", &s("Greg"), -208));
    assert!(holds_at(&old, e.greg, "name", &s("Greg"), 5));
    assert_eq!(old.as_of(e.greg, "name", Tick(0)).unwrap(), vec![s("Greg")]);
    assert_eq!(hist(&old, e.greg, "name"), vec![(-208, Op::Assert)]);
    // a view pinned at exactly the correction tx sees it deleted
    let at_fix = w.at_canon(w.latest_tx());
    assert!(!holds_at(&at_fix, e.greg, "name", &s("Greg"), -208));
    assert!(hist(&at_fix, e.greg, "name").is_empty());
}

#[test]
fn delete_leaves_other_values_and_other_ticks_alone() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(-300)).unwrap();
    w.retract(e.greg, "name", s("Greg"), t(-250)).unwrap();
    let b = only_entry(&w.assert(e.greg, "name", s("Gregor"), t(-250)).unwrap());
    w.correct(b, Correction::Delete).unwrap();
    assert!(holds(&w, e.greg, "name", &s("Greg"), -300));
    assert!(!holds(&w, e.greg, "name", &s("Greg"), -250));
    assert!(!holds(&w, e.greg, "name", &s("Gregor"), -250));
    assert_eq!(
        w.history(e.greg, "name").unwrap().len(),
        2,
        "only the deleted change point is omitted"
    );
}

#[test]
fn delete_at_tick_extremes_works() {
    let (mut w, e) = mem();
    for tk in [i64::MIN, i64::MAX] {
        let a = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(tk)).unwrap());
        assert!(holds(&w, e.greg, "alive", &Value::Bool(true), tk));
        let r = w.correct(a, Correction::Delete).unwrap();
        assert_eq!((r.changed, r.tick), (1, Tick(tk)));
        assert!(!holds(&w, e.greg, "alive", &Value::Bool(true), tk));
        assert!(w.history(e.greg, "alive").unwrap().is_empty());
        // a second, unrelated key at the other extreme, to reset state
        let b = only_entry(&w.assert(e.mara, "alive", Value::Bool(true), t(tk)).unwrap());
        w.correct(b, Correction::Delete).unwrap();
    }
}

// ------------------------------------------------ ^tx ordering (newest first)

#[test]
fn deleted_assert_is_skipped_as_a_whole_tick_not_fallen_through_to_the_older_tx_entry() {
    // X@-208 assert (tx a), delete (tx b). A reader that only skipped the
    // marker itself would fall through to the tx-a assert at the same key and
    // answer true.
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "alive", Value::Bool(true), t(-208))
            .unwrap(),
    );
    let canon_a = w.latest_tx();
    w.correct(a, Correction::Delete).unwrap();
    for tk in [-208, -207, 0, i64::MAX] {
        assert!(
            !holds(&w, e.greg, "alive", &Value::Bool(true), tk),
            "tick {tk}: fell through to the older tx entry"
        );
    }
    assert!(holds_at(
        &w.at_canon(canon_a),
        e.greg,
        "alive",
        &Value::Bool(true),
        -208
    ));
}

#[test]
fn skipped_tick_continues_with_older_ticks_assert_retract_under_a_deleted_assert() {
    // X@-300 assert, X@-250 retract, X@-208 assert (stored: X not held), then
    // delete the -208 assert. At -208 the whole tick is skipped and the
    // reader continues to the older tick: the -250 retract -> absent. A
    // fall-through bug reads the -208 assert -> present.
    let (mut w, e) = mem();
    w.assert(e.greg, "alive", Value::Bool(true), t(-300))
        .unwrap();
    w.retract(e.greg, "alive", Value::Bool(true), t(-250))
        .unwrap();
    let a = only_entry(
        &w.assert(e.greg, "alive", Value::Bool(true), t(-208))
            .unwrap(),
    );
    assert!(holds(&w, e.greg, "alive", &Value::Bool(true), -208));
    w.correct(a, Correction::Delete).unwrap();
    let v = Value::Bool(true);
    assert!(holds(&w, e.greg, "alive", &v, -300));
    assert!(holds(&w, e.greg, "alive", &v, -251));
    assert!(!holds(&w, e.greg, "alive", &v, -250), "retract @-250 wins");
    assert!(!holds(&w, e.greg, "alive", &v, -208), "skipped whole tick");
    assert!(!holds(&w, e.greg, "alive", &v, 0));
}

#[test]
fn skipped_tick_reads_the_older_assert_under_a_deleted_retract() {
    // X@-300 assert (holds), X@-208 retract, then delete the retract: at -208
    // and later the reader skips the whole tick and reads the -300 assert ->
    // holds. A fall-through bug reads the retract -> absent.
    let (mut w, e) = mem();
    w.assert(e.greg, "alive", Value::Bool(true), t(-300))
        .unwrap();
    let r = only_entry(
        &w.retract(e.greg, "alive", Value::Bool(true), t(-208))
            .unwrap(),
    );
    assert!(!holds(&w, e.greg, "alive", &Value::Bool(true), -208));
    w.correct(r, Correction::Delete).unwrap();
    for tk in [-300, -209, -208, -207, 0, i64::MAX] {
        assert!(
            holds(&w, e.greg, "alive", &Value::Bool(true), tk),
            "tick {tk}"
        );
    }
}

#[test]
fn assert_delete_assert_again_at_the_same_tick_is_stored_and_holds_at_latest_canon() {
    let (mut w, e) = mem();
    let v = Value::Bool(true);
    let a = only_entry(&w.assert(e.greg, "alive", v.clone(), t(-208)).unwrap());
    let canon_a = w.latest_tx();
    w.correct(a, Correction::Delete).unwrap();
    let canon_b = w.latest_tx();
    let r = w.assert(e.greg, "alive", v.clone(), t(-208)).unwrap();
    assert_eq!(r.changed, 1, "not held after the delete, so it is stored");
    let c = only_entry(&r);
    assert_ne!(c, a);
    let canon_c = w.latest_tx();

    assert!(holds(&w, e.greg, "alive", &v, -208));
    assert!(holds_at(&w.at_canon(canon_c), e.greg, "alive", &v, -208));
    assert!(
        !holds_at(&w.at_canon(canon_b), e.greg, "alive", &v, -208),
        "between the delete and the re-assert"
    );
    assert!(holds_at(&w.at_canon(canon_a), e.greg, "alive", &v, -208));
    assert_eq!(hist(&w.at_canon(canon_b), e.greg, "alive"), vec![]);
    assert_eq!(
        hist(&w.at_canon(canon_c), e.greg, "alive"),
        vec![(-208, Op::Assert)]
    );
}

#[test]
fn deleting_the_original_after_a_re_assert_is_already_deleted() {
    let (mut w, e) = mem();
    let v = Value::Bool(true);
    let a = only_entry(&w.assert(e.greg, "alive", v.clone(), t(-208)).unwrap());
    w.correct(a, Correction::Delete).unwrap();
    w.assert(e.greg, "alive", v.clone(), t(-208)).unwrap();
    let tx = w.latest_tx();
    assert_eq!(
        w.correct(a, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    assert_eq!(w.latest_tx(), tx);
    assert!(holds(&w, e.greg, "alive", &v, -208), "re-assert untouched");
}

#[test]
fn index_dump_shows_assert_deleted_assert_at_one_key_with_tx_descending() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(-208))
            .unwrap(),
    );
    w.correct(a, Correction::Delete).unwrap();
    w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(-208))
        .unwrap();
    let (_, attr, _, _) = key_of(&w, a);
    let d = w.index_dump();

    let erows: Vec<_> =
        d.e.iter()
            .filter(|r| {
                r.0 == e.greg && r.1 == attr && r.2 == Value::Ref(e.varrow) && r.3 == Tick(-208)
            })
            .collect();
    assert_eq!(erows.len(), 3, "{erows:?}");
    assert!(
        erows[0].4 > erows[1].4 && erows[1].4 > erows[2].4,
        "{erows:?}"
    );
    assert_eq!(erows[0].5, Mark::Assert);
    assert_eq!(erows[1].5, Mark::Deleted);
    assert_eq!(erows[2].5, Mark::Assert);

    let rrows: Vec<_> =
        d.r.iter()
            .filter(|r| r.0 == e.varrow && r.1 == attr && r.2 == e.greg && r.3 == Tick(-208))
            .collect();
    assert_eq!(rrows.len(), 3, "{rrows:?}");
    assert!(
        rrows[0].4 > rrows[1].4 && rrows[1].4 > rrows[2].4,
        "{rrows:?}"
    );
    assert_eq!(
        rrows.iter().map(|r| r.5).collect::<Vec<_>>(),
        vec![Mark::Assert, Mark::Deleted, Mark::Assert]
    );
    // the E and R rows agree tx by tx
    for (x, y) in erows.iter().zip(&rrows) {
        assert_eq!((x.4, x.5), (y.4, y.5));
    }
}

// --------------------------------------------------- retract / event / refs

#[test]
fn deleting_a_retract_reopens_the_state() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let before = w.latest_tx();
    assert!(!holds(&w, e.greg, "located_in", &v, -200));
    w.correct(r, Correction::Delete).unwrap();
    for tk in [-300, -251, -250, -200, 0] {
        assert!(holds(&w, e.greg, "located_in", &v, tk), "tick {tk}");
    }
    assert_eq!(
        hist(&w.at_canon(w.latest_tx()), e.greg, "located_in"),
        vec![(-300, Op::Assert)]
    );
    let old = w.at_canon(before);
    assert!(!holds_at(&old, e.greg, "located_in", &v, -200));
    assert_eq!(
        hist(&old, e.greg, "located_in"),
        vec![(-300, Op::Assert), (-250, Op::Retract)]
    );
    // the re-opened state shows up in neighbors too
    let n = w
        .neighbors(e.greg, "located_in", Direction::Out, Tick(-200))
        .unwrap();
    assert_eq!(n.iter().map(|x| x.0).collect::<Vec<_>>(), vec![e.varrow]);
    // and a later write is judged against the re-opened state
    assert_eq!(
        w.assert(e.greg, "located_in", v.clone(), t(-200))
            .unwrap()
            .changed,
        0,
        "already held at -200"
    );
}

#[test]
fn a_deleted_key_no_longer_blocks_the_same_tick_rule() {
    // retract @-250 deleted: the key holds nothing, so asserting the opposite
    // op there is not a same-tick conflict (it is a no-op here: held).
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    w.correct(r, Correction::Delete).unwrap();
    assert_eq!(
        w.assert(e.greg, "located_in", v.clone(), t(-250))
            .unwrap()
            .changed,
        0
    );
    // retracting again at -250 is stored (held at -250 now)
    let again = w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap();
    assert_eq!(again.changed, 1);
    assert!(!holds(&w, e.greg, "located_in", &v, -250));
}

#[test]
fn deleting_an_event_fact_is_allowed_and_retracting_one_is_still_an_error() {
    let (mut w, e) = mem();
    let v = Value::Ref(e.sword);
    let a = only_entry(&w.assert(e.edwin, "bought", v.clone(), t(-208)).unwrap());
    let canon = w.latest_tx();
    assert!(holds(&w, e.edwin, "bought", &v, 0));
    assert!(matches!(
        w.retract(e.edwin, "bought", v.clone(), t(-100)),
        Err(Error::RetractEvent(_))
    ));
    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-208)));
    assert!(!holds(&w, e.edwin, "bought", &v, -208));
    assert!(!holds(&w, e.edwin, "bought", &v, i64::MAX));
    assert!(w.history(e.edwin, "bought").unwrap().is_empty());
    assert!(holds_at(&w.at_canon(canon), e.edwin, "bought", &v, 0));
    // still an error after the delete
    assert!(matches!(
        w.retract(e.edwin, "bought", v.clone(), t(-100)),
        Err(Error::RetractEvent(_))
    ));
}

#[test]
fn deleting_a_ref_fact_drops_it_from_neighbors_both_ways_and_keeps_the_edge_id() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(-300))
            .unwrap(),
    );
    let edge = w.edge_id(e.greg, "located_in", e.varrow).unwrap();
    let canon = w.latest_tx();
    let out = |w: &World| {
        w.neighbors(e.greg, "located_in", Direction::Out, Tick(-200))
            .unwrap()
    };
    let inn = |w: &World| {
        w.neighbors(e.varrow, "located_in", Direction::In, Tick(-200))
            .unwrap()
    };
    assert_eq!(out(&w), vec![(e.varrow, edge)]);
    assert_eq!(inn(&w), vec![(e.greg, edge)]);

    w.correct(a, Correction::Delete).unwrap();
    assert!(out(&w).is_empty());
    assert!(inn(&w).is_empty());
    assert!(
        w.neighbors(e.greg, "located_in", Direction::Both, Tick(0))
            .unwrap()
            .is_empty()
    );
    // edge identity is permanent
    assert_eq!(w.edge_id(e.greg, "located_in", e.varrow), Some(edge));
    // old canon reads both sides, with the same edge
    let old = w.at_canon(canon);
    assert_eq!(
        old.neighbors(e.greg, "located_in", Direction::Out, Tick(-200))
            .unwrap(),
        vec![(e.varrow, edge)]
    );
    assert_eq!(
        old.neighbors(e.varrow, "located_in", Direction::In, Tick(-200))
            .unwrap(),
        vec![(e.greg, edge)]
    );
    // asserting it again reuses the edge: nothing new interned
    let r = w
        .assert(e.greg, "located_in", Value::Ref(e.varrow), t(-300))
        .unwrap();
    assert_eq!(r.changed, 1);
    assert!(r.edges.is_empty());
    assert_eq!(out(&w), vec![(e.varrow, edge)]);
}

#[test]
fn deleting_one_of_two_values_leaves_the_other_neighbor() {
    let (mut w, e) = mem();
    w.assert(e.greg, "owns", Value::Ref(e.sword), t(0)).unwrap();
    let a = only_entry(
        &w.assert(e.greg, "owns", Value::Ref(e.varrow), t(0))
            .unwrap(),
    );
    w.correct(a, Correction::Delete).unwrap();
    let n = w
        .neighbors(e.greg, "owns", Direction::Out, Tick(5))
        .unwrap();
    assert_eq!(n.iter().map(|x| x.0).collect::<Vec<_>>(), vec![e.sword]);
}

// ------------------------------------------------------------ bidirectional

#[test]
fn delete_of_a_reversed_bidirectional_fact_is_stored_normalised_and_clears_both_sides() {
    let (mut w, e) = mem();
    // written reversed: mara (larger) first
    let a = only_entry(
        &w.assert(e.mara, "ally_of", Value::Ref(e.edwin), t(-208))
            .unwrap(),
    );
    match w.entry(a).unwrap().body {
        EntryBody::Fact {
            entity, ref value, ..
        } => {
            assert_eq!(
                (entity, value),
                (e.edwin, &Value::Ref(e.mara)),
                "normalised"
            );
        }
        ref b => panic!("{b:?}"),
    }
    let edge = w.edge_id(e.edwin, "ally_of", e.mara).unwrap();
    assert_eq!(w.edge_id(e.mara, "ally_of", e.edwin), Some(edge));
    let canon = w.latest_tx();

    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-208)));
    let d = only_entry(&r);
    match w.entry(d).unwrap().body {
        EntryBody::Deleted {
            of,
            rev,
            entity,
            ref value,
            tick,
            ..
        } => {
            assert_eq!((of, rev, tick), (a, 1, Tick(-208)));
            assert_eq!(entity, e.edwin, "stored normalised: entity < value");
            assert_eq!(value, &Value::Ref(e.mara));
        }
        ref b => panic!("{b:?}"),
    }
    // both sides agree: gone
    for (x, y) in [(e.edwin, e.mara), (e.mara, e.edwin)] {
        assert!(w.as_of(x, "ally_of", Tick(0)).unwrap().is_empty());
        assert!(!holds(&w, x, "ally_of", &Value::Ref(y), -208));
        assert!(w.history(x, "ally_of").unwrap().is_empty());
        for dir in [Direction::Out, Direction::In, Direction::Both] {
            assert!(
                w.neighbors(x, "ally_of", dir, Tick(0)).unwrap().is_empty(),
                "{x:?} {dir:?}"
            );
        }
    }
    assert_eq!(w.edge_id(e.edwin, "ally_of", e.mara), Some(edge));
    // old canon: both sides present, same edge
    let old = w.at_canon(canon);
    for (x, y) in [(e.edwin, e.mara), (e.mara, e.edwin)] {
        assert_eq!(
            old.as_of(x, "ally_of", Tick(0)).unwrap(),
            vec![Value::Ref(y)]
        );
        assert_eq!(hist(&old, x, "ally_of"), vec![(-208, Op::Assert)]);
        assert_eq!(
            old.neighbors(x, "ally_of", Direction::Both, Tick(0))
                .unwrap(),
            vec![(y, edge)]
        );
    }
    // the R mirror carries the DELETED row too
    let (_, attr, _, _) = key_of(&w, a);
    let dump = w.index_dump();
    let r_marks: Vec<_> = dump
        .r
        .iter()
        .filter(|row| row.0 == e.mara && row.1 == attr && row.2 == e.edwin)
        .map(|row| row.5)
        .collect();
    assert_eq!(r_marks, vec![Mark::Deleted, Mark::Assert]);
    let e_marks: Vec<_> = dump
        .e
        .iter()
        .filter(|row| row.0 == e.edwin && row.1 == attr && row.2 == Value::Ref(e.mara))
        .map(|row| row.5)
        .collect();
    assert_eq!(e_marks, vec![Mark::Deleted, Mark::Assert]);
    // writing the pair again (either spelling) works and reuses the edge
    let again = w
        .assert(e.edwin, "ally_of", Value::Ref(e.mara), t(-208))
        .unwrap();
    assert_eq!(again.changed, 1);
    assert!(again.edges.is_empty());
    assert_eq!(
        w.neighbors(e.mara, "ally_of", Direction::Both, Tick(0))
            .unwrap(),
        vec![(e.edwin, edge)]
    );
}

// ------------------------------------------------------------------- errors

#[test]
fn unknown_entry_ids_are_unknown_entry_and_append_nothing() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(1)).unwrap());
    let beyond = EntryId(a.0 + 2 + 1); // fact, commit, then nothing
    let tx = w.latest_tx();
    for id in [EntryId(0), beyond, EntryId(9999), EntryId(u64::MAX)] {
        assert_eq!(
            w.correct(id, Correction::Delete).unwrap_err(),
            Error::UnknownEntry(id)
        );
    }
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.entry(beyond), None);
    assert!(holds(&w, e.greg, "name", &s("Greg"), 1));
}

/// First entry of each non-fact kind in a world.
fn non_fact_ids(w: &World) -> Vec<(&'static str, EntryId)> {
    let mut found: Vec<(&'static str, EntryId)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 1;
    while let Some(info) = w.entry(EntryId(i)) {
        let name = match info.body {
            EntryBody::DefineNodeType { .. } => "DefineNodeType",
            EntryBody::DefineAttr { .. } => "DefineAttr",
            EntryBody::DefineEntity { .. } => "DefineEntity",
            EntryBody::DefineEdge { .. } => "DefineEdge",
            EntryBody::Commit { .. } => "Commit",
            _ => "",
        };
        if !name.is_empty() && seen.insert(name) {
            found.push((name, info.id));
        }
        i += 1;
    }
    found
}

#[test]
fn define_and_commit_entries_are_not_a_fact() {
    let (mut w, e) = mem();
    w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(1))
        .unwrap(); // interns an edge
    let ids = non_fact_ids(&w);
    let names: Vec<_> = ids.iter().map(|x| x.0).collect();
    for k in [
        "DefineNodeType",
        "DefineAttr",
        "DefineEntity",
        "DefineEdge",
        "Commit",
    ] {
        assert!(names.contains(&k), "{k} missing from the world: {names:?}");
    }
    let tx = w.latest_tx();
    for (name, id) in ids {
        assert_eq!(
            w.correct(id, Correction::Delete).unwrap_err(),
            Error::NotAFact(id),
            "{name}"
        );
    }
    assert_eq!(w.latest_tx(), tx, "nothing appended");
    assert!(holds(&w, e.greg, "located_in", &Value::Ref(e.varrow), 1));
}

#[test]
fn the_genesis_define_attr_entries_are_not_a_fact() {
    let (mut w, _e) = mem();
    let genesis: Vec<EntryId> = (1..)
        .map(EntryId)
        .map_while(|id| w.entry(id))
        .take_while(|i| i.tx == TxId(0))
        .filter(|i| matches!(i.body, EntryBody::DefineAttr { .. }))
        .map(|i| i.id)
        .collect();
    assert_eq!(genesis.len(), 6, "one DefineAttr per schema attribute");
    for id in genesis {
        assert_eq!(
            w.correct(id, Correction::Delete).unwrap_err(),
            Error::NotAFact(id)
        );
    }
}

#[test]
fn deleting_twice_is_already_deleted_and_appends_nothing() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(1)).unwrap());
    let r = w.correct(a, Correction::Delete).unwrap();
    let d = only_entry(&r);
    let tx = w.latest_tx();
    assert_eq!(
        w.correct(a, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    assert_eq!(w.latest_tx(), tx);
    let next = d.0 + 2; // the Deleted + its Commit are the last committed entries
    assert_eq!(w.entry(EntryId(next)), None);
    assert!(matches!(
        w.entry(EntryId(next - 1)).unwrap().body,
        EntryBody::Commit { .. }
    ));
    // the Deleted marker's own id is not a fact
    assert_eq!(
        w.correct(d, Correction::Delete).unwrap_err(),
        Error::NotAFact(d)
    );
    assert_eq!(w.latest_tx(), tx);
    // the world is still usable
    w.assert(e.greg, "name", s("Gregor"), t(2)).unwrap();
}

#[test]
fn a_correction_of_a_no_longer_held_fact_is_still_just_a_delete() {
    // entry is a retract of a value; deleting it needs no "held" check
    let (mut w, e) = mem();
    w.assert(e.greg, "alive", Value::Bool(true), t(0)).unwrap();
    let r = only_entry(&w.retract(e.greg, "alive", Value::Bool(true), t(5)).unwrap());
    let r2 = w.correct(r, Correction::Delete).unwrap();
    assert_eq!(r2.changed, 1);
}

// ------------------------------------------------------------ transactions

#[test]
fn txn_correct_returns_empty_entries_and_the_commit_result_lists_the_deleted_id() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let before = w.latest_tx();
    let mut tx = w.begin();
    let r = tx.correct(a, Correction::Delete).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(r.tick, Tick(-208));
    assert!(r.entries.is_empty(), "entry IDs are assigned at commit");
    assert!(r.edges.is_empty());
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 1);
    assert_eq!(c.tx, TxId(before.0 + 1));
    assert_eq!(c.entries.len(), 1);
    let info = w.entry(c.entries[0]).unwrap();
    assert!(matches!(info.body, EntryBody::Deleted { of, rev: 1, .. } if of == a));
    assert_eq!(info.tx, c.tx);
    assert!(!holds(&w, e.greg, "name", &s("Greg"), -208));
}

#[test]
fn txn_mixing_an_assert_and_a_delete_is_atomic_on_commit() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let before = w.latest_tx();
    let mut tx = w.begin();
    tx.assert(e.mara, "name", s("Mara"), t(-100)).unwrap();
    tx.correct(a, Correction::Delete).unwrap();
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 2);
    assert_eq!(c.tx, TxId(before.0 + 1), "one tx for both");
    assert_eq!(c.entries.len(), 2);
    assert!(c.entries[0].0 < c.entries[1].0, "write order");
    assert!(matches!(
        w.entry(c.entries[0]).unwrap().body,
        EntryBody::Fact { .. }
    ));
    assert!(matches!(
        w.entry(c.entries[1]).unwrap().body,
        EntryBody::Deleted { .. }
    ));
    assert!(holds(&w, e.mara, "name", &s("Mara"), -100));
    assert!(!holds(&w, e.greg, "name", &s("Greg"), -208));
    // the pre-txn canon sees neither effect
    let old = w.at_canon(before);
    assert!(!holds_at(&old, e.mara, "name", &s("Mara"), -100));
    assert!(holds_at(&old, e.greg, "name", &s("Greg"), -208));
}

#[test]
fn a_dropped_txn_with_a_delete_changes_nothing() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let tx0 = w.latest_tx();
    {
        let mut tx = w.begin();
        tx.correct(a, Correction::Delete).unwrap();
    }
    assert_eq!(w.latest_tx(), tx0);
    assert!(holds(&w, e.greg, "name", &s("Greg"), -208));
    // and the entry is still correctable afterwards
    w.correct(a, Correction::Delete).unwrap();
}

#[test]
fn a_failed_correct_poisons_the_txn_and_nothing_is_visible() {
    for bad in [
        EntryId(0),
        EntryId(99_999),
        EntryId(1), // a DefineNodeType
    ] {
        let (mut w, e) = mem();
        let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
        let tx0 = w.latest_tx();
        let mut tx = w.begin();
        tx.assert(e.mara, "name", s("Mara"), t(-100)).unwrap();
        tx.correct(a, Correction::Delete).unwrap();
        let err = tx.correct(bad, Correction::Delete).unwrap_err();
        assert!(
            matches!(err, Error::UnknownEntry(_) | Error::NotAFact(_)),
            "{err:?}"
        );
        // poisoned: further writes and the commit fail
        assert_eq!(
            tx.assert(e.mara, "alive", Value::Bool(true), t(1))
                .unwrap_err(),
            Error::TransactionAborted
        );
        assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
        assert_eq!(w.latest_tx(), tx0);
        assert!(
            holds(&w, e.greg, "name", &s("Greg"), -208),
            "delete not applied"
        );
        assert!(
            !holds(&w, e.mara, "name", &s("Mara"), -100),
            "assert not applied"
        );
    }
}

#[test]
fn a_failed_delete_error_kinds_in_a_txn_match_the_auto_commit_ones() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let d = only_entry(&w.correct(a, Correction::Delete).unwrap());
    let mut tx = w.begin();
    assert_eq!(
        tx.correct(EntryId(0), Correction::Delete).unwrap_err(),
        Error::UnknownEntry(EntryId(0))
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    let mut tx = w.begin();
    assert_eq!(
        tx.correct(a, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    drop(tx);
    let mut tx = w.begin();
    assert_eq!(
        tx.correct(d, Correction::Delete).unwrap_err(),
        Error::NotAFact(d)
    );
}

#[test]
fn correcting_the_same_entry_twice_in_one_txn_is_txn_key_conflict() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    tx.correct(a, Correction::Delete).unwrap();
    let err = tx.correct(a, Correction::Delete).unwrap_err();
    assert_eq!(
        err,
        Error::TxnKeyConflict {
            entity: e.greg,
            attr: "name".into(),
            tick: Tick(-208)
        }
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
    assert!(holds(&w, e.greg, "name", &s("Greg"), -208));
}

#[test]
fn a_stored_write_at_a_key_this_txn_already_deleted_is_txn_key_conflict() {
    // X@-300 assert, X@-250 retract (entry r). In one txn: delete r (so X is
    // held at -250 again), then retract X @-250: judged against the staged
    // delete it would be stored at the same key -> TxnKeyConflict.
    let (mut w, e) = mem();
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    tx.correct(r, Correction::Delete).unwrap();
    let err = tx
        .retract(e.greg, "located_in", v.clone(), t(-250))
        .unwrap_err();
    assert_eq!(
        err,
        Error::TxnKeyConflict {
            entity: e.greg,
            attr: "located_in".into(),
            tick: Tick(-250)
        }
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
    assert!(
        !holds(&w, e.greg, "located_in", &v, -250),
        "retract still current"
    );
}

#[test]
fn an_assert_stored_at_a_key_this_txn_already_deleted_is_txn_key_conflict() {
    // X@-208 assert (entry a, X not held before). In one txn: delete a (X no
    // longer held), then assert X @-208 again: stored at the same key.
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "alive", Value::Bool(true), t(-208))
            .unwrap(),
    );
    let tx0 = w.latest_tx();
    let mut tx = w.begin();
    tx.correct(a, Correction::Delete).unwrap();
    let err = tx
        .assert(e.greg, "alive", Value::Bool(true), t(-208))
        .unwrap_err();
    assert_eq!(
        err,
        Error::TxnKeyConflict {
            entity: e.greg,
            attr: "alive".into(),
            tick: Tick(-208)
        }
    );
    assert_eq!(tx.commit().unwrap_err(), Error::TransactionAborted);
    assert_eq!(w.latest_tx(), tx0);
    assert!(holds(&w, e.greg, "alive", &Value::Bool(true), -208));
}

#[test]
fn writes_at_other_keys_after_a_delete_in_the_same_txn_are_fine() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "alive", Value::Bool(true), t(-208))
            .unwrap(),
    );
    let mut tx = w.begin();
    tx.correct(a, Correction::Delete).unwrap();
    // same value, other tick: not the deleted key
    assert_eq!(
        tx.assert(e.greg, "alive", Value::Bool(true), t(-207))
            .unwrap()
            .changed,
        1
    );
    let c = tx.commit().unwrap();
    assert_eq!(c.changed, 2);
    assert!(!holds(&w, e.greg, "alive", &Value::Bool(true), -208));
    assert!(holds(&w, e.greg, "alive", &Value::Bool(true), -207));
}

// ------------------------------------------------------------ latest_tick

#[test]
fn deleting_the_highest_tick_fact_lowers_latest_tick_to_the_next_highest() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    let hi = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    assert_eq!(w.latest_tick(), Tick(20));
    w.correct(hi, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(10));
}

#[test]
fn deleting_a_retract_at_the_highest_tick_lowers_latest_tick() {
    let (mut w, e) = mem();
    w.assert(e.greg, "alive", Value::Bool(true), t(10)).unwrap();
    let r = only_entry(
        &w.retract(e.greg, "alive", Value::Bool(true), t(30))
            .unwrap(),
    );
    assert_eq!(w.latest_tick(), Tick(30));
    w.correct(r, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(10));
}

#[test]
fn latest_tick_is_unchanged_when_another_key_shares_the_deleted_tick() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(5)).unwrap();
    let a = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    w.assert(e.edwin, "name", s("Edwin"), t(20)).unwrap();
    w.correct(a, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(20));
}

#[test]
fn deleting_a_lower_tick_fact_leaves_latest_tick_alone() {
    let (mut w, e) = mem();
    let lo = only_entry(&w.assert(e.greg, "name", s("Greg"), t(10)).unwrap());
    w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap();
    w.correct(lo, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(20));
}

#[test]
fn latest_tick_is_restored_by_a_re_assert_at_the_deleted_key() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    let hi = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    w.correct(hi, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(10));
    let r = w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap();
    assert_eq!(r.changed, 1);
    assert_eq!(w.latest_tick(), Tick(20));
}

#[test]
fn tick_less_writes_land_on_the_lowered_latest_tick() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    let hi = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    w.correct(hi, Correction::Delete).unwrap();
    let r = w.assert(e.greg, "owns", Value::Ref(e.sword), None).unwrap();
    assert_eq!(r.tick, Tick(10));
    assert!(holds(&w, e.greg, "owns", &Value::Ref(e.sword), 10));
    assert!(!holds(&w, e.greg, "owns", &Value::Ref(e.sword), 9));
}

#[test]
fn a_world_empty_after_the_delete_has_latest_tick_zero() {
    let (mut w, e) = mem();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(50)).unwrap());
    assert_eq!(w.latest_tick(), Tick(50));
    w.correct(a, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(0));
    let r = w.assert(e.greg, "name", s("Gregor"), None).unwrap();
    assert_eq!(r.tick, Tick(0));
}

#[test]
fn negative_latest_tick_drops_to_the_next_negative_one() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(-300)).unwrap();
    let hi = only_entry(
        &w.assert(e.greg, "alive", Value::Bool(true), t(-100))
            .unwrap(),
    );
    w.correct(hi, Correction::Delete).unwrap();
    assert_eq!(w.latest_tick(), Tick(-300));
}

#[test]
fn a_tick_less_write_after_a_delete_in_the_same_txn_uses_the_tick_from_txn_start() {
    let (mut w, e) = mem();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    let hi = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    let mut tx = w.begin();
    tx.correct(hi, Correction::Delete).unwrap();
    let r = tx
        .assert(e.greg, "owns", Value::Ref(e.sword), None)
        .unwrap();
    assert_eq!(r.tick, Tick(20), "latest tick is captured at txn start");
    tx.commit().unwrap();
    assert_eq!(w.latest_tick(), Tick(20), "the owns fact now sits at 20");
}

#[test]
fn latest_tick_after_a_delete_survives_reopen() {
    let (_d, path, mut w, e) = file();
    w.assert(e.greg, "name", s("Greg"), t(10)).unwrap();
    let hi = only_entry(&w.assert(e.greg, "alive", Value::Bool(true), t(20)).unwrap());
    w.correct(hi, Correction::Delete).unwrap();
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.latest_tick(), Tick(10));
}

// ------------------------------------------------- delete at canon boundaries

#[test]
fn delete_at_canon_boundaries_for_in_neighbors() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.greg, "located_in", Value::Ref(e.varrow), t(-300))
            .unwrap(),
    );
    let edge = w.edge_id(e.greg, "located_in", e.varrow).unwrap();
    let before = w.latest_tx();
    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!(r.changed, 1);
    let after = w.latest_tx();
    assert_eq!(after.0, before.0 + 1);

    let inn = |v: &View<'_>| {
        v.neighbors(e.varrow, "located_in", Direction::In, Tick(0))
            .unwrap()
    };
    // canon = the tx just before the correction: still there
    assert_eq!(inn(&w.at_canon(before)), vec![(e.greg, edge)]);
    // canon = the correction's own tx: gone
    assert!(inn(&w.at_canon(after)).is_empty());
    // canon far in the future behaves like latest
    assert!(inn(&w.at_canon(TxId(after.0 + 100))).is_empty());
    // canon 0 (genesis): nothing, as before any write
    assert!(inn(&w.at_canon(TxId(0))).is_empty());
    // the R side of Greg's Out too
    assert!(
        w.at_canon(after)
            .neighbors(e.greg, "located_in", Direction::Out, Tick(0))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_of_a_bidirectional_self_pair_at_canon_boundaries() {
    let (mut w, e) = mem();
    let a = only_entry(
        &w.assert(e.edwin, "ally_of", Value::Ref(e.edwin), t(-208))
            .unwrap(),
    );
    let edge = w.edge_id(e.edwin, "ally_of", e.edwin).unwrap();
    let before = w.latest_tx();
    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-208)));
    let after = w.latest_tx();
    let v = Value::Ref(e.edwin);

    for dir in [Direction::Out, Direction::In, Direction::Both] {
        let at = |c: TxId| {
            w.at_canon(c)
                .neighbors(e.edwin, "ally_of", dir, Tick(0))
                .unwrap()
        };
        assert!(at(after).is_empty(), "{dir:?} after");
        assert!(at(TxId(after.0 + 5)).is_empty(), "{dir:?} future canon");
        let old = at(before);
        assert!(
            old.iter().all(|x| *x == (e.edwin, edge)),
            "{dir:?} before: {old:?}"
        );
    }
    assert_eq!(
        w.at_canon(before)
            .neighbors(e.edwin, "ally_of", Direction::Both, Tick(0))
            .unwrap(),
        vec![(e.edwin, edge)],
        "self pair appears once"
    );
    assert!(w.as_of(e.edwin, "ally_of", Tick(0)).unwrap().is_empty());
    assert!(!holds(&w, e.edwin, "ally_of", &v, -208));
    assert!(holds_at(&w.at_canon(before), e.edwin, "ally_of", &v, -208));
    assert!(w.history(e.edwin, "ally_of").unwrap().is_empty());
    assert_eq!(
        hist(&w.at_canon(before), e.edwin, "ally_of"),
        vec![(-208, Op::Assert)]
    );
    // second delete is AlreadyDeleted; re-assert works and reuses the edge
    assert_eq!(
        w.correct(a, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    let again = w.assert(e.edwin, "ally_of", v, t(-208)).unwrap();
    assert_eq!(again.changed, 1);
    assert!(again.edges.is_empty());
}

// ------------------------------------------------------------- file-backed

fn probe(w: &World, e: &Ents, canon: TxId) -> String {
    let v = w.at_canon(canon);
    let mut out = String::new();
    for tk in [i64::MIN, -300, -250, -208, -207, 0, i64::MAX] {
        out += &format!(
            "{tk}: loc={:?} name={:?} ally={:?} n_out={:?} n_in={:?}\n",
            v.as_of(e.greg, "located_in", Tick(tk)).unwrap(),
            v.as_of(e.greg, "name", Tick(tk)).unwrap(),
            v.as_of(e.mara, "ally_of", Tick(tk)).unwrap(),
            v.neighbors(e.greg, "located_in", Direction::Out, Tick(tk))
                .unwrap(),
            v.neighbors(e.varrow, "located_in", Direction::In, Tick(tk))
                .unwrap(),
        );
    }
    out += &format!(
        "{:?}\n{:?}\n{:?}\n",
        v.history(e.greg, "located_in").unwrap(),
        v.history(e.greg, "name").unwrap(),
        v.history(e.edwin, "ally_of").unwrap()
    );
    out
}

/// A scripted world with three corrections, in any world kind.
fn script(w: &mut World, e: &Ents) -> Vec<EntryId> {
    let v = Value::Ref(e.varrow);
    w.assert(e.greg, "located_in", v.clone(), t(-300)).unwrap();
    let r = only_entry(&w.retract(e.greg, "located_in", v.clone(), t(-250)).unwrap());
    let n = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let al = only_entry(
        &w.assert(e.mara, "ally_of", Value::Ref(e.edwin), t(-208))
            .unwrap(),
    );
    let mut deleted = Vec::new();
    for id in [r, n, al] {
        deleted.push(only_entry(&w.correct(id, Correction::Delete).unwrap()));
    }
    deleted
}

#[test]
fn log_format_version_is_three_and_the_header_says_so() {
    assert_eq!(LOG_FORMAT_VERSION, 3);
    let (_d, path, _w, _e) = file();
    let b = std::fs::read(&path).unwrap();
    assert_eq!(&b[8..12], &3u32.to_le_bytes());
}

#[test]
fn file_log_holds_the_deleted_entries_and_reads_match_entry() {
    let (_d, path, mut w, e) = file();
    let deleted = script(&mut w, &e);
    let log = read_log(&path).unwrap();
    for (i, id) in deleted.iter().enumerate() {
        let from_log = &log[(id.0 - 1) as usize];
        assert_eq!(from_log.id, *id);
        match &from_log.body {
            EntryBody::Deleted { of, rev, .. } => {
                assert_eq!(*rev, 1, "#{i}");
                assert!(of.0 < id.0);
            }
            b => panic!("#{i}: {b:?}"),
        }
        assert_eq!(w.entry(*id).unwrap().body, from_log.body);
    }
    // each Deleted sits in its own txn, followed by that txn's Commit
    for id in &deleted {
        assert!(matches!(log[id.0 as usize].body, EntryBody::Commit { .. }));
    }
    for (i, le) in log.iter().enumerate() {
        assert_eq!(le.id.0, i as u64 + 1, "ids are gap-free");
    }
}

#[test]
fn reopen_gives_the_same_reads_at_latest_and_old_canon_and_the_same_entries() {
    let (_d, path, mut w, e) = file();
    let before_script = w.latest_tx();
    let deleted = script(&mut w, &e);
    let latest = w.latest_tx();
    let last_entry = read_log(&path).unwrap().last().unwrap().id.0;
    let canons: Vec<TxId> = (before_script.0..=latest.0).map(TxId).collect();
    let reads: Vec<String> = canons.iter().map(|c| probe(&w, &e, *c)).collect();
    let entries: Vec<_> = (0..=last_entry + 1).map(|i| w.entry(EntryId(i))).collect();
    drop(w);

    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), latest);
    assert_eq!(w.recovery().bytes_cut, 0);
    for (c, r) in canons.iter().zip(&reads) {
        assert_eq!(&probe(&w, &e, *c), r, "canon {c:?}");
    }
    let again: Vec<_> = (0..=last_entry + 1).map(|i| w.entry(EntryId(i))).collect();
    assert_eq!(again, entries);
    for id in &deleted {
        assert!(matches!(
            w.entry(*id).unwrap().body,
            EntryBody::Deleted { .. }
        ));
    }
    // new writes continue the ids
    let r = w.assert(e.greg, "alive", Value::Bool(true), t(1)).unwrap();
    assert_eq!(only_entry(&r), EntryId(last_entry + 1));
}

#[test]
fn a_deleted_chain_is_still_already_deleted_after_reopen() {
    let (_d, path, mut w, e) = file();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    w.correct(a, Correction::Delete).unwrap();
    drop(w);
    let mut w = World::open(&path).unwrap();
    assert_eq!(
        w.correct(a, Correction::Delete).unwrap_err(),
        Error::AlreadyDeleted(a)
    );
    assert!(!holds(&w, e.greg, "name", &s("Greg"), -208));
}

#[test]
fn in_memory_and_file_worlds_give_identical_results_and_entries_for_a_delete_script() {
    let (_d, path, mut fw, fe) = file();
    let (mut mw, me) = mem();
    let fd = script(&mut fw, &fe);
    let md = script(&mut mw, &me);
    assert_eq!(fd, md);
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
fn an_auto_commit_delete_survives_a_failed_sync_as_nothing() {
    let (_d, path, mut w, e) = file();
    let a = only_entry(&w.assert(e.greg, "name", s("Greg"), t(-208)).unwrap());
    let tx = w.latest_tx();
    let n = read_log(&path).unwrap().last().unwrap().id.0;
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(matches!(
        w.correct(a, Correction::Delete),
        Err(Error::Io { .. })
    ));
    assert_eq!(w.entry(EntryId(n + 1)), None);
    drop(w);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.latest_tx(), tx);
    assert!(holds(&w, e.greg, "name", &s("Greg"), -208));
    let r = w.correct(a, Correction::Delete).unwrap();
    assert_eq!(only_entry(&r), EntryId(n + 1));
}

#[test]
fn a_version_2_file_is_unsupported_for_open_and_read_log() {
    let (_d, path, w, _e) = file();
    drop(w);
    let mut b = std::fs::read(&path).unwrap();
    b[8..12].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &b).unwrap();
    assert!(matches!(
        World::open(&path),
        Err(Error::UnsupportedVersion(2))
    ));
    assert_eq!(read_log(&path), Err(Error::UnsupportedVersion(2)));
    assert_eq!(std::fs::read(&path).unwrap(), b, "file untouched");
}

#[test]
fn byte_level_helpers_write_the_current_version() {
    // the crafted-log helpers in tests/common take the version explicitly;
    // this pins that the current one is what real files carry.
    let (_d, path, _w, _e) = file();
    let b = std::fs::read(&path).unwrap();
    assert_eq!(&b[..12], &common::header(LOG_FORMAT_VERSION)[..]);
}
