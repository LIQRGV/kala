//! Item 9: tick defaulting. A write's tick is: its own tick, else the
//! transaction default, else the world's latest tick captured when the
//! transaction began (0 for an empty world). Atomicity, poisoning and canon
//! pinning are item 11 and are not tested here.

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
}

struct Fx {
    w: World,
    greg: EntityId,
    mara: EntityId,
    varrow: EntityId,
    ashford: EntityId,
    carnath: EntityId,
}

fn fx() -> Fx {
    let mut w = World::in_memory(schema()).expect("world");
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = w.create_entity("PLACE", Some("ashford")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    Fx {
        w,
        greg,
        mara,
        varrow,
        ashford,
        carnath,
    }
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn r(e: EntityId) -> Value {
    Value::Ref(e)
}

fn holds(f: &Fx, who: EntityId, place: EntityId, tick: i64) -> bool {
    f.w.as_of_value(who, "located_in", &r(place), Tick(tick))
        .unwrap()
}

// ---------- latest_tick ----------

#[test]
fn empty_world_latest_tick_is_zero() {
    let f = fx();
    assert_eq!(f.w.latest_tick(), Tick(0));
    let w = World::in_memory(schema()).unwrap();
    assert_eq!(w.latest_tick(), Tick(0));
}

#[test]
fn creating_entities_does_not_move_latest_tick() {
    let f = fx();
    assert_eq!(f.w.latest_tick(), Tick(0));
}

#[test]
fn tickless_write_in_empty_world_lands_at_tick_zero() {
    let mut f = fx();
    let res = f.w.assert(f.greg, "located_in", r(f.varrow), None).unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(res.tick, Tick(0));
    assert!(!holds(&f, f.greg, f.varrow, -1));
    assert!(holds(&f, f.greg, f.varrow, 0));
    assert_eq!(f.w.latest_tick(), Tick(0));
}

#[test]
fn latest_tick_is_highest_tick_of_any_fact_assert_or_retract() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(-300));
    f.w.retract(f.greg, "located_in", r(f.varrow), t(-250))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(-250));
}

#[test]
fn all_negative_world_has_negative_latest_tick_not_zero() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(-300));
    f.w.assert(f.mara, "located_in", r(f.varrow), t(i64::MIN))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(-300));
}

#[test]
fn latest_tick_at_i64_extremes() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(i64::MIN))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(i64::MIN));
    f.w.assert(f.mara, "located_in", r(f.varrow), t(i64::MAX))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(i64::MAX));
}

#[test]
fn prequel_write_does_not_lower_latest_tick() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(601))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.ashford), t(-500))
        .unwrap();
    assert_eq!(f.w.latest_tick(), Tick(601));
}

#[test]
fn noop_write_does_not_move_latest_tick() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    // assert of a value already holding at -100: 0 changed
    let res =
        f.w.assert(f.greg, "located_in", r(f.varrow), t(-100))
            .unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(f.w.latest_tick(), Tick(-300));
    // retract of a value not asserted at 900: 0 changed
    let res =
        f.w.retract(f.greg, "located_in", r(f.ashford), t(900))
            .unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(f.w.latest_tick(), Tick(-300));
}

#[test]
fn failed_write_does_not_move_latest_tick() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    // wrong type
    assert!(
        f.w.assert(f.greg, "located_in", Value::Bool(true), t(900))
            .is_err()
    );
    // unknown attribute
    assert!(f.w.assert(f.greg, "nope", r(f.varrow), t(900)).is_err());
    // same-tick conflict
    assert!(
        f.w.retract(f.greg, "located_in", r(f.varrow), t(-300))
            .is_err()
    );
    assert_eq!(f.w.latest_tick(), Tick(-300));
}

// ---------- auto-commit defaulting ----------

#[test]
fn tickless_auto_commit_write_uses_latest_tick_and_reports_it() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(200))
        .unwrap();
    f.w.assert(f.mara, "located_in", r(f.varrow), t(601))
        .unwrap();
    let res = f.w.assert(f.greg, "owns", r(f.carnath), None).unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(res.tick, Tick(601));
    let owns = |tick| {
        f.w.as_of_value(f.greg, "owns", &r(f.carnath), Tick(tick))
            .unwrap()
    };
    assert!(!owns(600));
    assert!(owns(601));
    assert_eq!(f.w.latest_tick(), Tick(601));
}

#[test]
fn tickless_write_in_all_negative_world_lands_on_negative_latest_tick() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    let res = f.w.assert(f.mara, "located_in", r(f.varrow), None).unwrap();
    assert_eq!(res.tick, Tick(-300));
    assert!(!holds(&f, f.mara, f.varrow, -301));
    assert!(holds(&f, f.mara, f.varrow, -300));
}

#[test]
fn tickless_retract_uses_latest_tick_and_reports_it() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    f.w.assert(f.greg, "located_in", r(f.ashford), t(-250))
        .unwrap();
    let res =
        f.w.retract(f.greg, "located_in", r(f.varrow), None)
            .unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(res.tick, Tick(-250));
    assert!(holds(&f, f.greg, f.varrow, -251));
    assert!(!holds(&f, f.greg, f.varrow, -250));
}

#[test]
fn tickless_noop_reports_the_latest_tick_it_was_judged_at() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(10))
        .unwrap();
    // already holds at the latest tick 10: 0 changed, tick still reported
    let res = f.w.assert(f.greg, "located_in", r(f.varrow), None).unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(res.tick, Tick(10));
    // retract of a value that isn't asserted: 0 changed at tick 10
    let res =
        f.w.retract(f.mara, "located_in", r(f.varrow), None)
            .unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(res.tick, Tick(10));
}

#[test]
fn explicit_tick_wins_over_latest_tick_in_auto_commit() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(601))
        .unwrap();
    let res =
        f.w.assert(f.mara, "located_in", r(f.varrow), t(-5))
            .unwrap();
    assert_eq!(res.tick, Tick(-5));
}

#[test]
fn two_tickless_opposite_ops_on_the_same_value_conflict() {
    let mut f = fx();
    // the latest tick is set by another fact
    f.w.assert(f.greg, "located_in", r(f.varrow), t(601))
        .unwrap();
    let res =
        f.w.assert(f.greg, "located_in", r(f.ashford), None)
            .unwrap();
    assert_eq!((res.changed, res.tick), (1, Tick(601)));
    let tx = f.w.latest_tx();
    let err = f.w.retract(f.greg, "located_in", r(f.ashford), None);
    assert_eq!(
        err,
        Err(Error::SameTickConflict {
            entity: f.greg,
            attr: "located_in".to_string(),
            tick: Tick(601),
        })
    );
    assert_eq!(f.w.latest_tx(), tx);
    assert!(holds(&f, f.greg, f.ashford, 601));
    assert_eq!(f.w.latest_tick(), Tick(601));
}

#[test]
fn two_tickless_opposite_ops_conflict_in_an_empty_world_at_zero() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), None).unwrap();
    let err = f.w.retract(f.greg, "located_in", r(f.varrow), None);
    assert_eq!(
        err,
        Err(Error::SameTickConflict {
            entity: f.greg,
            attr: "located_in".to_string(),
            tick: Tick(0),
        })
    );
}

#[test]
fn tickless_repeat_of_the_same_op_is_a_noop() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(601))
        .unwrap();
    let a = f.w.assert(f.mara, "located_in", r(f.varrow), None).unwrap();
    assert_eq!(a.changed, 1);
    let b = f.w.assert(f.mara, "located_in", r(f.varrow), None).unwrap();
    assert_eq!((b.changed, b.tick), (0, Tick(601)));
}

// ---------- transactions ----------

#[test]
fn txn_explicit_tick_wins_over_txn_default() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let mut txn = f.w.begin();
    txn.set_default_tick(Tick(-208));
    let res = txn.assert(greg, "located_in", r(varrow), t(-100)).unwrap();
    assert_eq!(res.tick, Tick(-100));
    txn.commit().unwrap();
    assert!(!holds(&f, f.greg, f.varrow, -101));
    assert!(holds(&f, f.greg, f.varrow, -100));
}

#[test]
fn txn_default_tick_wins_over_latest_tick() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(601))
        .unwrap();
    let (mara, ashford, carnath) = (f.mara, f.ashford, f.carnath);
    let mut txn = f.w.begin();
    txn.set_default_tick(Tick(-208));
    let a = txn.assert(mara, "located_in", r(ashford), None).unwrap();
    let b = txn.assert(mara, "owns", r(carnath), None).unwrap();
    assert_eq!(a.tick, Tick(-208));
    assert_eq!(b.tick, Tick(-208));
    txn.commit().unwrap();
    assert!(!holds(&f, f.mara, f.ashford, -209));
    assert!(holds(&f, f.mara, f.ashford, -208));
    // a prequel default does not lower the latest tick
    assert_eq!(f.w.latest_tick(), Tick(601));
}

#[test]
fn txn_default_tick_applies_to_retracts_too() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(-300))
        .unwrap();
    let (greg, varrow) = (f.greg, f.varrow);
    let mut txn = f.w.begin();
    txn.set_default_tick(Tick(-250));
    let res = txn.retract(greg, "located_in", r(varrow), None).unwrap();
    assert_eq!((res.changed, res.tick), (1, Tick(-250)));
    txn.commit().unwrap();
    assert!(holds(&f, f.greg, f.varrow, -251));
    assert!(!holds(&f, f.greg, f.varrow, -250));
}

#[test]
fn txn_without_default_uses_latest_tick_captured_at_begin() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(100))
        .unwrap();
    let (mara, ashford, carnath) = (f.mara, f.ashford, f.carnath);
    let mut txn = f.w.begin();
    let a = txn.assert(mara, "located_in", r(ashford), None).unwrap();
    assert_eq!(a.tick, Tick(100));
    // a later explicit tick inside the txn does not move the default
    let b = txn.assert(mara, "owns", r(carnath), t(900)).unwrap();
    assert_eq!(b.tick, Tick(900));
    let c = txn.assert(mara, "owns", r(ashford), None).unwrap();
    assert_eq!(c.tick, Tick(100));
    txn.commit().unwrap();
    // after commit the world's latest tick reflects the @900 write
    assert_eq!(f.w.latest_tick(), Tick(900));
    assert!(holds(&f, f.mara, f.ashford, 100));
    assert!(!holds(&f, f.mara, f.ashford, 99));
}

#[test]
fn every_tickless_write_in_one_txn_lands_on_the_same_tick() {
    let mut f = fx();
    // greg is in varrow from 10; the latest tick (50) is set by another fact
    f.w.assert(f.greg, "located_in", r(f.varrow), t(10))
        .unwrap();
    f.w.assert(f.mara, "located_in", r(f.ashford), t(50))
        .unwrap();
    let (greg, mara, varrow, ashford, carnath) = (f.greg, f.mara, f.varrow, f.ashford, f.carnath);
    let mut txn = f.w.begin();
    let ticks = [
        txn.assert(mara, "located_in", r(varrow), None)
            .unwrap()
            .tick,
        txn.assert(mara, "owns", r(ashford), None).unwrap().tick,
        txn.assert(mara, "owns", r(carnath), t(700)).unwrap().tick,
        txn.assert(greg, "owns", r(carnath), None).unwrap().tick,
        txn.retract(greg, "located_in", r(varrow), None)
            .unwrap()
            .tick,
    ];
    assert_eq!(ticks, [Tick(50), Tick(50), Tick(700), Tick(50), Tick(50)]);
    let res = txn.commit().unwrap();
    // all five writes really changed something (the retract is not a no-op)
    assert_eq!(res.changed, 5);
    assert!(holds(&f, greg, varrow, 49));
    assert!(!holds(&f, greg, varrow, 50));
}

#[test]
fn txn_in_empty_world_without_default_lands_at_zero() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    let mut txn = f.w.begin();
    assert_eq!(
        txn.assert(greg, "located_in", r(varrow), None)
            .unwrap()
            .tick,
        Tick(0)
    );
    assert_eq!(
        txn.assert(greg, "owns", r(ashford), t(-5)).unwrap().tick,
        Tick(-5)
    );
    assert_eq!(
        txn.assert(greg, "owns", r(varrow), None).unwrap().tick,
        Tick(0)
    );
    txn.commit().unwrap();
}

#[test]
fn txn_tickless_opposite_ops_on_one_value_conflict() {
    let mut f = fx();
    f.w.assert(f.mara, "located_in", r(f.varrow), t(601))
        .unwrap();
    let (greg, ashford) = (f.greg, f.ashford);
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(ashford), None).unwrap();
    let err = txn.retract(greg, "located_in", r(ashford), None);
    assert_eq!(
        err,
        Err(Error::SameTickConflict {
            entity: greg,
            attr: "located_in".to_string(),
            tick: Tick(601),
        })
    );
}

#[test]
fn txn_writes_are_visible_after_commit() {
    let mut f = fx();
    let (greg, mara, varrow, ashford) = (f.greg, f.mara, f.varrow, f.ashford);
    let before = f.w.latest_tx();
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(varrow), t(-300)).unwrap();
    txn.retract(greg, "located_in", r(varrow), t(-250)).unwrap();
    txn.assert(mara, "located_in", r(ashford), t(-100)).unwrap();
    let res = txn.commit().unwrap();
    assert_eq!(res.changed, 3);
    assert_eq!(res.tx, TxId(before.0 + 1));
    assert_eq!(f.w.latest_tx(), res.tx);
    assert!(!holds(&f, greg, varrow, -301));
    assert!(holds(&f, greg, varrow, -300));
    assert!(holds(&f, greg, varrow, -251));
    assert!(!holds(&f, greg, varrow, -250));
    assert!(holds(&f, mara, ashford, -100));
    assert_eq!(f.w.latest_tick(), Tick(-100));
}

#[test]
fn commit_changed_counts_only_real_changes() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(varrow), t(0)).unwrap();
    let mut txn = f.w.begin();
    // 1 change
    assert_eq!(
        txn.assert(greg, "located_in", r(ashford), t(10))
            .unwrap()
            .changed,
        1
    );
    // no-op: already holds at 10 (and repeated within the same txn)
    assert_eq!(
        txn.assert(greg, "located_in", r(ashford), t(10))
            .unwrap()
            .changed,
        0
    );
    // no-op: varrow holds at 5, but this asserts at 5 something already holding
    assert_eq!(
        txn.assert(greg, "located_in", r(varrow), t(5))
            .unwrap()
            .changed,
        0
    );
    // 1 change
    assert_eq!(
        txn.retract(greg, "located_in", r(varrow), t(10))
            .unwrap()
            .changed,
        1
    );
    let res = txn.commit().unwrap();
    assert_eq!(res.changed, 2);
}

#[test]
fn dropping_a_txn_without_commit_discards_its_writes() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let tx = f.w.latest_tx();
    {
        let mut txn = f.w.begin();
        txn.assert(greg, "located_in", r(varrow), t(900)).unwrap();
    }
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.latest_tick(), Tick(0));
    assert!(!holds(&f, greg, varrow, 900));
}

// ---------- item 9 confirm: staged writes, defaults, no-op commits ----------

#[test]
fn txn_sees_its_own_staged_writes_for_noop_judgement() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let mut txn = f.w.begin();
    assert_eq!(
        txn.assert(greg, "located_in", r(varrow), t(10))
            .unwrap()
            .changed,
        1
    );
    // holds at 20 thanks to the staged assert @10
    assert_eq!(
        txn.retract(greg, "located_in", r(varrow), t(20))
            .unwrap()
            .changed,
        1
    );
    // not holding at 5 (staged assert is @10): no-op
    assert_eq!(
        txn.retract(greg, "located_in", r(varrow), t(5))
            .unwrap()
            .changed,
        0
    );
    let res = txn.commit().unwrap();
    assert_eq!(res.changed, 2);
    assert!(!holds(&f, greg, varrow, 9));
    assert!(holds(&f, greg, varrow, 10));
    assert!(holds(&f, greg, varrow, 19));
    assert!(!holds(&f, greg, varrow, 20));
}

#[test]
fn staged_write_conflicts_with_committed_fact_at_same_tick() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    f.w.assert(greg, "located_in", r(varrow), t(10)).unwrap();
    let mut txn = f.w.begin();
    let err = txn.retract(greg, "located_in", r(varrow), t(10));
    assert_eq!(
        err,
        Err(Error::SameTickConflict {
            entity: greg,
            attr: "located_in".to_string(),
            tick: Tick(10),
        })
    );
}

#[test]
fn staged_write_conflicts_with_earlier_staged_write_at_same_tick() {
    let mut f = fx();
    let (greg, varrow) = (f.greg, f.varrow);
    let mut txn = f.w.begin();
    txn.assert(greg, "located_in", r(varrow), t(10)).unwrap();
    let err = txn.retract(greg, "located_in", r(varrow), t(10));
    assert_eq!(
        err,
        Err(Error::SameTickConflict {
            entity: greg,
            attr: "located_in".to_string(),
            tick: Tick(10),
        })
    );
}

#[test]
fn set_default_tick_midway_applies_to_later_writes_only() {
    let mut f = fx();
    f.w.assert(f.greg, "located_in", r(f.varrow), t(100))
        .unwrap();
    let (mara, ashford, carnath, varrow) = (f.mara, f.ashford, f.carnath, f.varrow);
    let mut txn = f.w.begin();
    let a = txn.assert(mara, "owns", r(ashford), None).unwrap();
    txn.set_default_tick(Tick(-5));
    let b = txn.assert(mara, "owns", r(carnath), None).unwrap();
    txn.set_default_tick(Tick(-9));
    let c = txn.assert(mara, "owns", r(varrow), None).unwrap();
    assert_eq!(a.tick, Tick(100));
    assert_eq!(b.tick, Tick(-5));
    assert_eq!(c.tick, Tick(-9));
    txn.commit().unwrap();
    let owns = |p: EntityId, tick| f.w.as_of_value(mara, "owns", &r(p), Tick(tick)).unwrap();
    assert!(owns(ashford, 100) && !owns(ashford, 99));
    assert!(owns(carnath, -5) && !owns(carnath, -6));
    assert!(owns(varrow, -9) && !owns(varrow, -10));
}

#[test]
fn default_tick_at_extremes_and_zero() {
    for tick in [i64::MIN, 0, i64::MAX] {
        let mut f = fx();
        f.w.assert(f.greg, "located_in", r(f.varrow), t(77))
            .unwrap();
        let (mara, ashford) = (f.mara, f.ashford);
        let mut txn = f.w.begin();
        txn.set_default_tick(Tick(tick));
        let res = txn.assert(mara, "located_in", r(ashford), None).unwrap();
        assert_eq!((res.changed, res.tick), (1, Tick(tick)));
        txn.commit().unwrap();
        assert!(holds(&f, mara, ashford, tick));
        if tick != i64::MIN {
            assert!(!holds(&f, mara, ashford, tick - 1));
        }
        assert_eq!(f.w.latest_tick(), Tick(77.max(tick)));
    }
}

#[test]
fn all_noop_commit_changes_nothing_and_does_not_advance_tx() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(varrow), t(10)).unwrap();
    let (tx, tick) = (f.w.latest_tx(), f.w.latest_tick());
    let mut txn = f.w.begin();
    // assert of a value already holding; retract of a value never asserted
    assert_eq!(
        txn.assert(greg, "located_in", r(varrow), t(50))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        txn.retract(greg, "located_in", r(ashford), t(900))
            .unwrap()
            .changed,
        0
    );
    let res = txn.commit().unwrap();
    assert_eq!(res.changed, 0);
    assert_eq!(res.tx, tx);
    assert_eq!(f.w.latest_tx(), tx);
    assert_eq!(f.w.latest_tick(), tick);
}

#[test]
fn empty_commit_does_not_advance_tx() {
    let mut f = fx();
    let tx = f.w.latest_tx();
    let res = f.w.begin().commit().unwrap();
    assert_eq!((res.changed, res.tx), (0, tx));
    assert_eq!(f.w.latest_tx(), tx);
}

#[test]
fn prequel_txn_does_not_move_latest_tick() {
    let mut f = fx();
    let (greg, varrow, ashford) = (f.greg, f.varrow, f.ashford);
    f.w.assert(greg, "located_in", r(varrow), t(601)).unwrap();
    let mut txn = f.w.begin();
    txn.set_default_tick(Tick(-400));
    txn.assert(greg, "located_in", r(ashford), None).unwrap();
    txn.commit().unwrap();
    assert_eq!(f.w.latest_tick(), Tick(601));
}
