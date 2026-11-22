//! Item 11: transactions. Atomic visibility, poisoning on error, entity
//! creation inside a txn, tx as a counter, and readers pinned to a canon tx.
//!
//! Expected API (contract): `Txn::create_entity(&mut self, node_type: &str,
//! handle: Option<&str>) -> Result<EntityId, Error>`, `Error::TransactionAborted`,
//! `World::at_canon(&self, tx: TxId) -> View<'_>` with
//! `View::as_of_value(e, attr, &Value, Tick) -> Result<bool, Error>` and
//! `View::as_of(e, attr, Tick) -> Result<Vec<Value>, Error>`.
//! A write error inside a Txn poisons it: later writes (assert, retract,
//! create_entity) return `Err(Error::TransactionAborted)` and so does commit.
//! A dropped or aborted txn's entity IDs are reusable.

use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
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
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
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

struct Fx {
    w: World,
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    Fx {
        w,
        greg,
        mara,
        varrow,
        ashford,
    }
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn tx_plus(tx: TxId, n: u64) -> TxId {
    TxId(tx.0 + n)
}

// ---------- grouping / drop ----------

#[test]
fn dropped_txn_discards_all_writes_and_created_entities() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let (tx, tick) = (f.w.latest_tx(), f.w.latest_tick());
    {
        let mut txn = f.w.begin();
        txn.assert(greg, "located_in", r(varrow), t(900)).unwrap();
        txn.assert(greg, "owns", r(varrow), t(901)).unwrap();
        let e = txn.create_entity("CHARACTER", Some("lisa")).unwrap();
        txn.assert(e, "name", Value::Str("Lisa".into()), t(5))
            .unwrap();
    }
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.latest_tick(), tick);
    assert_eq!(f.w.entity_by_handle("lisa"), None);
    assert!(
        !f.w.as_of_value(greg, "located_in", &r(varrow), Tick(900))
            .unwrap()
    );
    assert!(f.w.as_of(greg, "owns", Tick(901)).unwrap().is_empty());
}

#[test]
fn dropped_txn_entity_ids_are_reusable() {
    let mut f = fx();
    let dropped = {
        let mut txn = f.w.begin();
        txn.create_entity("CHARACTER", Some("lisa")).unwrap()
    };
    // greg=1, mara=2 were committed; the dropped one took seq 3
    assert_eq!(dropped.seq(), 3);
    let again = f.w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(again, dropped);
    assert_eq!(f.w.entity_by_handle("lisa"), Some(again));
}

#[test]
fn aborted_txn_entity_ids_are_reusable() {
    let mut f = fx();
    let mut txn = f.w.begin();
    let e = txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert!(txn.assert(e, "nope", r(e), t(1)).is_err());
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
    let again = f.w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(again, e);
}

// ---------- all-or-nothing visibility with one tx ----------

#[test]
fn committed_writes_share_one_tx_and_are_invisible_to_the_previous_canon() {
    let mut f = fx();
    let (greg, mara, varrow, ashford) = (f.greg, f.mara, f.varrow, f.ashford);
    let before = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(varrow), t(-300)).unwrap();
    txn.retract(greg, "located_in", r(varrow), t(-250)).unwrap();
    txn.assert(mara, "located_in", r(ashford), t(-100)).unwrap();
    let res = txn.commit().unwrap();
    assert_eq!(res.tx, tx_plus(before, 1));
    assert_eq!(res.changed, 3);
    assert_eq!(f.w.latest_tx(), res.tx);

    let old = f.w.at_canon(before);
    assert!(
        !old.as_of_value(greg, "located_in", &r(varrow), Tick(-300))
            .unwrap()
    );
    assert!(
        !old.as_of_value(greg, "located_in", &r(varrow), Tick(-260))
            .unwrap()
    );
    assert!(
        !old.as_of_value(mara, "located_in", &r(ashford), Tick(-100))
            .unwrap()
    );

    let new = f.w.at_canon(res.tx);
    assert!(
        new.as_of_value(greg, "located_in", &r(varrow), Tick(-300))
            .unwrap()
    );
    assert!(
        !new.as_of_value(greg, "located_in", &r(varrow), Tick(-250))
            .unwrap()
    );
    assert!(
        new.as_of_value(mara, "located_in", &r(ashford), Tick(-100))
            .unwrap()
    );
    assert_eq!(
        new.as_of(mara, "located_in", Tick(-100)).unwrap(),
        vec![r(ashford)]
    );
}

// ---------- poisoning ----------

#[derive(Clone, Copy)]
struct Ids {
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
}

fn poison_checks(f: &mut Fx, poison: impl FnOnce(&mut Txn<'_>, Ids) -> Result<(), Error>) {
    let (greg, mara, varrow) = (f.greg, f.mara, f.varrow);
    let ids = Ids { greg, mara, varrow };
    let mara = ids.mara;
    let (tx, tick) = (f.w.latest_tx(), f.w.latest_tick());
    let mut txn = f.w.begin();
    let created = txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    txn.assert(greg, "located_in", r(varrow), t(900)).unwrap();
    let err = poison(&mut txn, ids);
    assert!(err.is_err(), "the poisoning write must fail");
    // everything afterwards is TransactionAborted, even a valid write
    assert_eq!(
        txn.assert(mara, "located_in", r(varrow), t(1)),
        Err(Error::TransactionAborted)
    );
    assert_eq!(
        txn.retract(greg, "located_in", r(varrow), t(1)),
        Err(Error::TransactionAborted)
    );
    assert_eq!(
        txn.create_entity("PLACE", Some("carnath")),
        Err(Error::TransactionAborted)
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
    // nothing visible
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.latest_tick(), tick);
    assert_eq!(f.w.entity_by_handle("lisa"), None);
    assert_eq!(f.w.entity_by_handle("carnath"), None);
    assert!(f.w.as_of(greg, "located_in", Tick(900)).unwrap().is_empty());
    assert!(f.w.as_of(mara, "located_in", Tick(1)).unwrap().is_empty());
    // the handle and seq are free again
    let again = f.w.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(again, created);
}

#[test]
fn unknown_attribute_poisons_the_txn() {
    let mut f = fx();
    poison_checks(&mut f, |txn, ids| {
        txn.assert(ids.greg, "nope", r(ids.varrow), t(1))
            .map(|_| ())
    });
}

#[test]
fn wrong_value_type_poisons_the_txn() {
    let mut f = fx();
    poison_checks(&mut f, |txn, ids| {
        txn.assert(ids.greg, "located_in", Value::Bool(true), t(1))
            .map(|_| ())
    });
}

#[test]
fn same_tick_conflict_poisons_the_txn() {
    let mut f = fx();
    poison_checks(&mut f, |txn, ids| {
        // the txn already asserted greg located_in varrow @900
        txn.retract(ids.greg, "located_in", r(ids.varrow), t(900))
            .map(|_| ())
    });
}

#[test]
fn retract_event_poisons_the_txn() {
    let mut f = fx();
    poison_checks(&mut f, |txn, ids| {
        txn.retract(ids.greg, "bought", r(ids.varrow), t(1))
            .map(|_| ())
    });
}

#[test]
fn string_too_long_for_indexed_attribute_poisons_the_txn() {
    let mut f = fx();
    poison_checks(&mut f, |txn, ids| {
        txn.assert(ids.greg, "name", Value::Str("x".repeat(257)), t(1))
            .map(|_| ())
    });
}

#[test]
fn poisoned_txn_error_is_the_first_error_not_aborted() {
    let mut f = fx();
    let greg = f.greg;
    let mut txn = f.w.begin();
    assert_eq!(
        txn.assert(greg, "nope", Value::Bool(true), t(1)),
        Err(Error::UnknownAttribute("nope".to_string()))
    );
    assert_eq!(
        txn.assert(greg, "nope", Value::Bool(true), t(1)),
        Err(Error::TransactionAborted)
    );
}

#[test]
fn a_noop_is_not_an_error_and_does_not_poison() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let mut txn = f.w.begin();
    let res = txn.retract(greg, "located_in", r(varrow), t(1)).unwrap();
    assert_eq!(res.changed, 0);
    txn.assert(greg, "located_in", r(varrow), t(2)).unwrap();
    assert_eq!(txn.commit().unwrap().changed, 1);
}

#[test]
fn a_failed_auto_commit_write_does_not_affect_later_writes() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    assert!(f.w.assert(greg, "nope", r(varrow), t(1)).is_err());
    assert_eq!(
        f.w.assert(greg, "located_in", r(varrow), t(1))
            .unwrap()
            .changed,
        1
    );
}

// ---------- create_entity inside a txn ----------

#[test]
fn entity_created_in_a_txn_is_usable_as_subject_and_ref_value() {
    let mut f = fx();
    let greg = f.greg;
    let mut txn = f.w.begin();
    let lisa = txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    let carnath = txn.create_entity("PLACE", Some("carnath")).unwrap();
    txn.assert(lisa, "located_in", r(carnath), t(10)).unwrap();
    txn.assert(greg, "owns", r(carnath), t(10)).unwrap();
    txn.assert(lisa, "name", Value::Str("Lisa".into()), t(10))
        .unwrap();
    txn.commit().unwrap();
    assert_eq!(f.w.entity_by_handle("lisa"), Some(lisa));
    assert_eq!(f.w.entity_by_handle("carnath"), Some(carnath));
    assert_eq!(f.w.format_id(lisa), "CHARACTER#3");
    assert_eq!(f.w.format_id(carnath), "PLACE#3");
    assert!(
        f.w.as_of_value(lisa, "located_in", &r(carnath), Tick(10))
            .unwrap()
    );
    assert!(
        f.w.as_of_value(greg, "owns", &r(carnath), Tick(10))
            .unwrap()
    );
}

#[test]
fn per_type_sequence_continues_after_a_txn_commit() {
    let mut f = fx();
    let mut txn = f.w.begin();
    let a = txn.create_entity("CHARACTER", None).unwrap();
    let b = txn.create_entity("CHARACTER", None).unwrap();
    let p = txn.create_entity("PLACE", None).unwrap();
    txn.commit().unwrap();
    assert_eq!((a.seq(), b.seq(), p.seq()), (3, 4, 3));
    let c = f.w.create_entity("CHARACTER", None).unwrap();
    let q = f.w.create_entity("PLACE", None).unwrap();
    assert_eq!((c.seq(), q.seq()), (5, 4));
}

#[test]
fn duplicate_handle_within_a_txn_errors_and_poisons() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(
        txn.create_entity("PLACE", Some("lisa")),
        Err(Error::DuplicateHandle("lisa".to_string()))
    );
    assert_eq!(
        txn.create_entity("PLACE", None),
        Err(Error::TransactionAborted)
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.entity_by_handle("lisa"), None);
}

#[test]
fn duplicate_handle_of_a_committed_entity_errors_and_poisons() {
    let mut f = fx();
    let mut txn = f.w.begin();
    assert_eq!(
        txn.create_entity("CHARACTER", Some("greg")),
        Err(Error::DuplicateHandle("greg".to_string()))
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
}

#[test]
fn unknown_node_type_in_a_txn_poisons() {
    let mut f = fx();
    let mut txn = f.w.begin();
    assert_eq!(
        txn.create_entity("DRAGON", None),
        Err(Error::UnknownNodeType("DRAGON".to_string()))
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
}

// ---------- tx is a counter ----------

#[test]
fn each_committed_txn_with_changes_advances_tx_by_exactly_one() {
    let mut f = fx();
    let (greg, mara, varrow, ashford) = (f.greg, f.mara, f.varrow, f.ashford);
    let t0 = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    txn.assert(mara, "located_in", r(varrow), t(1)).unwrap();
    txn.assert(greg, "owns", r(ashford), t(1)).unwrap();
    assert_eq!(txn.commit().unwrap().tx, tx_plus(t0, 1));
    let mut txn = f.w.begin();
    txn.retract(greg, "located_in", r(varrow), t(2)).unwrap();
    assert_eq!(txn.commit().unwrap().tx, tx_plus(t0, 2));
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 2));
}

#[test]
fn a_txn_that_only_creates_an_entity_advances_tx_by_one() {
    let mut f = fx();
    let t0 = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    let res = txn.commit().unwrap();
    assert_eq!(res.tx, tx_plus(t0, 1));
    assert_eq!(res.changed, 0);
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 1));
}

#[test]
fn auto_commit_assert_and_create_entity_are_each_one_tx() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let t0 = f.w.latest_tx();
    f.w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 1));
    f.w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 2));
    // a no-op auto-commit write appends nothing
    f.w.assert(greg, "located_in", r(varrow), t(5)).unwrap();
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 2));
}

#[test]
fn failed_txns_do_not_consume_tx_numbers() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let t0 = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    let _ = txn.assert(greg, "nope", r(varrow), t(1));
    assert!(txn.commit().is_err());
    let res = f.w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 1));
}

// ---------- readers pin a tx ----------

#[test]
fn an_old_canon_answers_as_before_later_commits() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(varrow), t(-300)).unwrap();
    let c = f.w.latest_tx();
    // later retract, a later prequel assert at an earlier tick, a later new value
    f.w.retract(greg, "located_in", r(varrow), t(-250)).unwrap();
    f.w.assert(greg, "located_in", r(ashford), t(-500)).unwrap();
    f.w.assert(greg, "located_in", r(ashford), t(-100)).unwrap(); // no-op: holds
    f.w.assert(greg, "owns", r(varrow), t(0)).unwrap();

    let old = f.w.at_canon(c);
    assert!(
        old.as_of_value(greg, "located_in", &r(varrow), Tick(-200))
            .unwrap()
    );
    assert!(
        !old.as_of_value(greg, "located_in", &r(ashford), Tick(-500))
            .unwrap()
    );
    assert_eq!(
        old.as_of(greg, "located_in", Tick(-400)).unwrap(),
        Vec::<Value>::new()
    );
    assert_eq!(
        old.as_of(greg, "located_in", Tick(-200)).unwrap(),
        vec![r(varrow)]
    );
    assert!(old.as_of(greg, "owns", Tick(0)).unwrap().is_empty());

    // latest canon sees everything
    assert!(
        !f.w.as_of_value(greg, "located_in", &r(varrow), Tick(-200))
            .unwrap()
    );
    assert!(
        f.w.as_of_value(greg, "located_in", &r(ashford), Tick(-500))
            .unwrap()
    );
    assert_eq!(f.w.as_of(greg, "owns", Tick(0)).unwrap(), vec![r(varrow)]);
}

#[test]
fn canon_zero_has_no_entities_so_reads_are_unknown_entity() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    let v = f.w.at_canon(TxId(0));
    // greg isn't defined at canon 0, so reading him there is an error
    assert_eq!(
        v.as_of_value(greg, "located_in", &r(varrow), Tick(1)),
        Err(kala_core::Error::UnknownEntity(greg))
    );
    assert_eq!(
        v.as_of(greg, "located_in", Tick(1)),
        Err(kala_core::Error::UnknownEntity(greg))
    );
}

#[test]
fn canon_beyond_latest_is_treated_as_latest() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    let far = TxId(f.w.latest_tx().0 + 1000);
    let v = f.w.at_canon(far);
    assert!(
        v.as_of_value(greg, "located_in", &r(varrow), Tick(1))
            .unwrap()
    );
    assert_eq!(
        v.as_of(greg, "located_in", Tick(1)).unwrap(),
        vec![r(varrow)]
    );
    let max = f.w.at_canon(TxId(u64::MAX));
    assert!(
        max.as_of_value(greg, "located_in", &r(varrow), Tick(1))
            .unwrap()
    );
}

#[test]
fn every_canon_from_zero_to_latest_matches_the_writes_up_to_it() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    let base = f.w.latest_tx();
    f.w.assert(greg, "located_in", r(varrow), t(10)).unwrap(); // base+1
    f.w.assert(greg, "located_in", r(ashford), t(5)).unwrap(); // base+2
    f.w.retract(greg, "located_in", r(varrow), t(20)).unwrap(); // base+3
    let at = |n: u64, tick: i64| {
        let v = f.w.at_canon(tx_plus(base, n));
        (
            v.as_of_value(greg, "located_in", &r(varrow), Tick(tick))
                .unwrap(),
            v.as_of_value(greg, "located_in", &r(ashford), Tick(tick))
                .unwrap(),
        )
    };
    assert_eq!(at(0, 15), (false, false));
    assert_eq!(at(1, 15), (true, false));
    assert_eq!(at(2, 15), (true, true));
    assert_eq!(at(2, 25), (true, true));
    assert_eq!(at(3, 15), (true, true));
    assert_eq!(at(3, 25), (false, true));
}

// ---------- more edge cases ----------

#[test]
fn forward_id_guess_used_as_ref_before_create_entity_poisons() {
    let mut f = fx();
    let (greg, tx) = (f.greg, f.w.latest_tx());
    // CHARACTER seq 1, 2 exist; guess seq 3 before creating it
    let guess = EntityId::new(greg.node_type(), 3).unwrap();
    let mut txn = f.w.begin();
    assert_eq!(
        txn.assert(greg, "owns", r(guess), t(1)),
        Err(Error::UnknownEntity(guess))
    );
    // creating it afterwards does not help: the txn is poisoned
    assert_eq!(
        txn.create_entity("CHARACTER", None),
        Err(Error::TransactionAborted)
    );
    assert_eq!(txn.commit(), Err(Error::TransactionAborted));
    assert_eq!(f.w.latest_tx(), tx);
    // seq 3 is still free
    assert_eq!(f.w.create_entity("CHARACTER", None).unwrap(), guess);
}

#[test]
fn txn_creating_entities_plus_all_noop_writes_advances_tx_by_one_changed_zero() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(1)).unwrap();
    let t0 = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.create_entity("CHARACTER", Some("lisa")).unwrap();
    assert_eq!(
        txn.assert(greg, "located_in", r(varrow), t(5))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        txn.retract(greg, "owns", r(varrow), t(5)).unwrap().changed,
        0
    );
    let res = txn.commit().unwrap();
    assert_eq!(res.tx, tx_plus(t0, 1));
    assert_eq!(res.changed, 0);
    assert_eq!(f.w.latest_tx(), tx_plus(t0, 1));
    assert!(f.w.entity_by_handle("lisa").is_some());
}

#[test]
fn view_canon_reports_the_clamped_value() {
    let f = fx();
    let latest = f.w.latest_tx();
    assert_eq!(f.w.at_canon(TxId(u64::MAX)).canon(), latest);
    assert_eq!(f.w.at_canon(tx_plus(latest, 1)).canon(), latest);
    assert_eq!(f.w.at_canon(latest).canon(), latest);
    assert_eq!(f.w.at_canon(TxId(0)).canon(), TxId(0));
}

#[test]
fn old_canon_skips_a_long_chain_of_newer_entries_above_and_below_t() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(0)).unwrap();
    let a = f.w.latest_tx();
    f.w.assert(greg, "located_in", r(varrow), t(-20)).unwrap();
    f.w.retract(greg, "located_in", r(varrow), t(-10)).unwrap();
    f.w.assert(greg, "located_in", r(varrow), t(-5)).unwrap();
    f.w.retract(greg, "located_in", r(varrow), t(5)).unwrap();
    f.w.assert(greg, "located_in", r(varrow), t(10)).unwrap();
    f.w.retract(greg, "located_in", r(varrow), t(20)).unwrap();
    assert_eq!(f.w.latest_tx(), tx_plus(a, 6));

    let old = f.w.at_canon(a);
    let ticks = [
        i64::MIN,
        -100,
        -20,
        -10,
        -5,
        -1,
        0,
        3,
        5,
        10,
        20,
        100,
        i64::MAX,
    ];
    for x in ticks {
        let expect_old = x >= 0;
        assert_eq!(
            old.as_of_value(greg, "located_in", &r(varrow), Tick(x))
                .unwrap(),
            expect_old,
            "old canon at {x}"
        );
        let expect_new = (-20..-10).contains(&x) || (-5..5).contains(&x) || (10..20).contains(&x);
        assert_eq!(
            f.w.as_of_value(greg, "located_in", &r(varrow), Tick(x))
                .unwrap(),
            expect_new,
            "latest canon at {x}"
        );
        assert_eq!(
            old.as_of(greg, "located_in", Tick(x)).unwrap(),
            if expect_old { vec![r(varrow)] } else { vec![] },
            "old as_of at {x}"
        );
    }
    // an intermediate canon: a + 2 has assert@-20 and retract@-10 too
    let mid = f.w.at_canon(tx_plus(a, 2));
    assert!(
        mid.as_of_value(greg, "located_in", &r(varrow), Tick(-15))
            .unwrap()
    );
    assert!(
        !mid.as_of_value(greg, "located_in", &r(varrow), Tick(-10))
            .unwrap()
    );
    assert!(
        !mid.as_of_value(greg, "located_in", &r(varrow), Tick(-2))
            .unwrap()
    );
    assert!(
        mid.as_of_value(greg, "located_in", &r(varrow), Tick(0))
            .unwrap()
    );
}

#[test]
fn later_retract_after_the_pinned_tick_does_not_affect_old_canon() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(10)).unwrap();
    let a = f.w.latest_tx();
    f.w.retract(greg, "located_in", r(varrow), t(11)).unwrap();
    assert_eq!(f.w.latest_tx(), tx_plus(a, 1));
    assert!(
        f.w.at_canon(a)
            .as_of_value(greg, "located_in", &r(varrow), Tick(11))
            .unwrap()
    );
    assert!(
        !f.w.as_of_value(greg, "located_in", &r(varrow), Tick(11))
            .unwrap()
    );
}
