//! Slice 2, item 4: edge properties (docs/slice-2-decisions.md, "Item 4").
//! A property of an edge is an ordinary fact whose entity is the edge ID
//! (`EdgeId` converted with `.into()`), with the normal as_of / history /
//! no-op / same-tick rules. CLAUDE.md "Edges" is the model:
//!
//! ```text
//! edge#1 = (Edwin, present, Carnath)   assert @-300 ... retract @-250
//! edge#1 disguised true  @-300 assert
//! edge#1 disguised true  @-280 retract, disguised false @-280 assert
//! ```
use kala_core::*;
use std::path::PathBuf;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn schema() -> Schema {
    let r = |n, k| AttrDef::new(n, ValueType::Ref, k, Cardinality::Many);
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(r("present", Kind::State))
        .attribute(r("witness", Kind::State))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "note",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

#[derive(Clone, Copy)]
struct Ents {
    edwin: EntityId,
    mara: EntityId,
    carnath: EntityId,
    edge: EntityId,
}

/// edge#1 = (edwin, present, carnath), asserted @-300 and retracted @-250.
fn setup(w: &mut World) -> Ents {
    let edwin = w.create_entity("CHARACTER", Some("edwin")).unwrap();
    let mara = w.create_entity("CHARACTER", Some("mara")).unwrap();
    let carnath = w.create_entity("PLACE", Some("carnath")).unwrap();
    w.assert(edwin, "present", Value::Ref(carnath), t(-300))
        .unwrap();
    w.retract(edwin, "present", Value::Ref(carnath), t(-250))
        .unwrap();
    let edge = w.edge_id(edwin, "present", carnath).unwrap().into();
    Ents {
        edwin,
        mara,
        carnath,
        edge,
    }
}

fn fx() -> (World, Ents) {
    let mut w = World::in_memory(schema()).unwrap();
    let e = setup(&mut w);
    (w, e)
}

/// fx plus the CLAUDE.md disguise history on the edge.
fn disguised() -> (World, Ents) {
    let (mut w, e) = fx();
    w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    w.retract(e.edge, "disguised", b(true), t(-280)).unwrap();
    w.assert(e.edge, "disguised", b(false), t(-280)).unwrap();
    (w, e)
}

fn b(x: bool) -> Value {
    Value::Bool(x)
}

fn holds(w: &World, e: EntityId, v: bool, tick: i64) -> bool {
    w.as_of_value(e, "disguised", &b(v), Tick(tick)).unwrap()
}

// ---------------------------------------------------------------------------
// as_of / as_of_value / history on the edge entity
// ---------------------------------------------------------------------------

#[test]
fn as_of_on_the_edge_follows_the_claude_md_disguise_example_at_every_boundary() {
    let (w, e) = disguised();
    let at = |x: i64| w.as_of(e.edge, "disguised", Tick(x)).unwrap();
    assert_eq!(at(i64::MIN), vec![]);
    assert_eq!(at(-301), vec![]);
    assert_eq!(at(-300), vec![b(true)]);
    assert_eq!(at(-299), vec![b(true)]);
    assert_eq!(at(-281), vec![b(true)]);
    assert_eq!(at(-280), vec![b(false)]);
    assert_eq!(at(-279), vec![b(false)]);
    assert_eq!(at(-250), vec![b(false)]);
    assert_eq!(at(0), vec![b(false)]);
    assert_eq!(at(i64::MAX), vec![b(false)]);
}

#[test]
fn as_of_value_on_the_edge_is_half_open_at_the_change_points() {
    let (w, e) = disguised();
    assert!(!holds(&w, e.edge, true, -301));
    assert!(holds(&w, e.edge, true, -300));
    assert!(holds(&w, e.edge, true, -281));
    assert!(!holds(&w, e.edge, true, -280));
    assert!(!holds(&w, e.edge, false, -281));
    assert!(holds(&w, e.edge, false, -280));
    assert!(holds(&w, e.edge, false, i64::MAX));
}

#[test]
fn history_of_an_edge_property_lists_its_change_points_in_tick_order() {
    let (w, e) = disguised();
    let h = w.history(e.edge, "disguised").unwrap();
    let ticks: Vec<i64> = h.iter().map(|c| c.tick.0).collect();
    assert_eq!(ticks, vec![-300, -280, -280], "{h:?}");
    assert_eq!(
        h[0],
        Change {
            tick: Tick(-300),
            op: Op::Assert,
            value: b(true),
            precision: Precision::Exact
        }
    );
    let at280: Vec<(Op, Value)> = h[1..].iter().map(|c| (c.op, c.value.clone())).collect();
    assert!(at280.contains(&(Op::Retract, b(true))), "{h:?}");
    assert!(at280.contains(&(Op::Assert, b(false))), "{h:?}");
}

#[test]
fn history_of_an_attribute_the_edge_never_had_is_empty() {
    let (w, e) = fx();
    assert_eq!(w.history(e.edge, "disguised").unwrap(), vec![]);
    assert_eq!(w.as_of(e.edge, "disguised", Tick(0)).unwrap(), vec![]);
}

// ---------------------------------------------------------------------------
// independence from the edge's own history
// ---------------------------------------------------------------------------

#[test]
fn property_history_is_independent_of_the_edges_own_assert_and_retract() {
    let (w, e) = disguised();
    // the edge itself holds on [-300, -250) ...
    assert_eq!(
        w.as_of(e.edwin, "present", Tick(-260)).unwrap(),
        vec![Value::Ref(e.carnath)]
    );
    assert_eq!(w.as_of(e.edwin, "present", Tick(-250)).unwrap(), vec![]);
    // ... but its property carries on past the retract
    assert_eq!(
        w.as_of(e.edge, "disguised", Tick(-250)).unwrap(),
        vec![b(false)]
    );
    assert_eq!(
        w.as_of(e.edge, "disguised", Tick(0)).unwrap(),
        vec![b(false)]
    );
    // and the edge's history has no property rows, nor the reverse
    let ticks: Vec<i64> = w
        .history(e.edwin, "present")
        .unwrap()
        .iter()
        .map(|c| c.tick.0)
        .collect();
    assert_eq!(ticks, vec![-300, -250]);
    assert_eq!(w.history(e.edge, "present").unwrap(), vec![]);
}

#[test]
fn writing_properties_does_not_change_the_edges_id_or_its_ends_state() {
    let (mut w, e) = fx();
    let before = w.history(e.edwin, "present").unwrap();
    w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    w.assert(e.edge, "note", Value::Str("cloak".into()), t(-290))
        .unwrap();
    assert_eq!(w.history(e.edwin, "present").unwrap(), before);
    assert_eq!(
        w.edge_id(e.edwin, "present", e.carnath).map(EntityId::from),
        Some(e.edge)
    );
    // two properties on one edge are independent attributes
    assert_eq!(w.as_of(e.edge, "note", Tick(-300)).unwrap(), vec![]);
    assert_eq!(
        w.as_of(e.edge, "note", Tick(-290)).unwrap(),
        vec![Value::Str("cloak".into())]
    );
}

#[test]
fn two_edges_keep_separate_property_histories() {
    let (mut w, e) = fx();
    w.assert(e.mara, "present", Value::Ref(e.carnath), t(-300))
        .unwrap();
    let other: EntityId = w.edge_id(e.mara, "present", e.carnath).unwrap().into();
    assert_ne!(other, e.edge);
    w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    assert_eq!(w.as_of(other, "disguised", Tick(-300)).unwrap(), vec![]);
    w.assert(other, "disguised", b(false), t(-300)).unwrap();
    assert_eq!(
        w.as_of(e.edge, "disguised", Tick(-300)).unwrap(),
        vec![b(true)]
    );
    assert_eq!(
        w.as_of(other, "disguised", Tick(-300)).unwrap(),
        vec![b(false)]
    );
}

// ---------------------------------------------------------------------------
// no-op and same-tick rules
// ---------------------------------------------------------------------------

#[test]
fn assert_of_a_value_that_already_holds_is_zero_changed() {
    let (mut w, e) = disguised();
    let tx = w.latest_tx();
    let r = w.assert(e.edge, "disguised", b(false), t(-100)).unwrap();
    assert_eq!((r.changed, r.tick), (0, Tick(-100)));
    let r = w.assert(e.edge, "disguised", b(true), t(-299)).unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(w.latest_tx(), tx, "a no-op consumes no tx");
}

#[test]
fn retract_of_a_value_that_does_not_hold_is_zero_changed() {
    let (mut w, e) = disguised();
    // true was retracted at -280; false not yet asserted at -290; nothing at -400
    assert_eq!(
        w.retract(e.edge, "disguised", b(true), t(-100))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.retract(e.edge, "disguised", b(false), t(-290))
            .unwrap()
            .changed,
        0
    );
    assert_eq!(
        w.retract(e.edge, "disguised", b(true), t(-400))
            .unwrap()
            .changed,
        0
    );
    let (mut w2, e2) = fx();
    assert_eq!(
        w2.retract(e2.edge, "disguised", b(true), t(0))
            .unwrap()
            .changed,
        0
    );
}

#[test]
fn out_of_order_retract_is_judged_at_the_writes_tick() {
    let (mut w, e) = disguised();
    // true held at -290 even though it no longer holds at the latest tick
    let r = w.retract(e.edge, "disguised", b(true), t(-290)).unwrap();
    assert_eq!(r.changed, 1);
    assert!(holds(&w, e.edge, true, -291));
    assert!(!holds(&w, e.edge, true, -290));
}

#[test]
fn same_tick_assert_vs_retract_on_an_edge_property_is_an_error() {
    let (mut w, e) = disguised();
    assert!(matches!(
        w.retract(e.edge, "disguised", b(true), t(-300)),
        Err(Error::SameTickConflict { .. })
    ));
    assert!(matches!(
        w.assert(e.edge, "disguised", b(true), t(-280)),
        Err(Error::SameTickConflict { .. })
    ));
    assert!(matches!(
        w.retract(e.edge, "disguised", b(false), t(-280)),
        Err(Error::SameTickConflict { .. })
    ));
    // nothing was stored by the failures
    assert_eq!(w.history(e.edge, "disguised").unwrap().len(), 3);
}

#[test]
fn repeating_the_same_op_at_the_same_tick_is_a_noop() {
    let (mut w, e) = disguised();
    let r = w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    assert_eq!(r.changed, 0);
    let r = w.retract(e.edge, "disguised", b(true), t(-280)).unwrap();
    assert_eq!(r.changed, 0);
    assert_eq!(w.history(e.edge, "disguised").unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// schema rules apply to edge properties
// ---------------------------------------------------------------------------

#[test]
fn unknown_attribute_on_an_edge_is_unknown_attribute() {
    let (mut w, e) = fx();
    assert_eq!(
        w.assert(e.edge, "no_such", b(true), t(0)).unwrap_err(),
        Error::UnknownAttribute("no_such".into())
    );
    assert_eq!(
        w.retract(e.edge, "no_such", b(true), t(0)).unwrap_err(),
        Error::UnknownAttribute("no_such".into())
    );
    assert_eq!(
        w.as_of(e.edge, "no_such", Tick(0)).unwrap_err(),
        Error::UnknownAttribute("no_such".into())
    );
}

#[test]
fn wrong_value_type_on_an_edge_property_is_wrong_value_type() {
    let (mut w, e) = fx();
    let err = w
        .assert(e.edge, "disguised", Value::I64(1), t(0))
        .unwrap_err();
    assert_eq!(
        err,
        Error::WrongValueType {
            attr: "disguised".into(),
            expected: ValueType::Bool,
            got: ValueType::I64
        }
    );
    assert!(matches!(
        w.assert(e.edge, "note", b(true), t(0)),
        Err(Error::WrongValueType { .. })
    ));
}

// ---------------------------------------------------------------------------
// undefined / unasserted edges
// ---------------------------------------------------------------------------

#[test]
fn property_on_an_undefined_edge_is_unknown_entity_in_world_and_txn() {
    let (mut w, e) = fx();
    let ghost: EntityId = EdgeId::new(2).unwrap().into(); // only edge#1 exists
    assert_eq!(
        w.assert(ghost, "disguised", b(true), t(0)).unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    assert_eq!(
        w.retract(ghost, "disguised", b(true), t(0)).unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    {
        let mut txn = w.begin();
        assert_eq!(
            txn.assert(ghost, "disguised", b(true), t(0)).unwrap_err(),
            Error::UnknownEntity(ghost)
        );
        assert_eq!(txn.commit().unwrap_err(), Error::TransactionAborted);
    }
    // nothing was consumed: the next new triple is still edge#2
    w.assert(e.mara, "present", Value::Ref(e.carnath), t(0))
        .unwrap();
    assert_eq!(w.edge_id(e.mara, "present", e.carnath).unwrap().seq(), 2);
}

#[test]
fn property_write_on_an_edge_in_an_empty_world_is_unknown_entity() {
    let mut w = World::in_memory(schema()).unwrap();
    let ghost: EntityId = EdgeId::new(1).unwrap().into();
    assert_eq!(
        w.assert(ghost, "disguised", b(true), t(0)).unwrap_err(),
        Error::UnknownEntity(ghost)
    );
}

#[test]
fn property_at_a_tick_where_the_edge_is_not_asserted_is_allowed() {
    let (mut w, e) = fx();
    // before the edge starts (-300) and after it ends (-250)
    for tick in [i64::MIN, -400, -301, -250, -249, 0, i64::MAX] {
        let r = w
            .assert(e.edge, "note", Value::Str(format!("n{tick}")), t(tick))
            .unwrap();
        assert_eq!(r.changed, 1, "tick {tick}");
        assert_eq!(r.tick, Tick(tick));
    }
    // cardinality one is only a label: earlier values stay (never retracted)
    let at = w.as_of(e.edge, "note", Tick(-400)).unwrap();
    assert!(at.contains(&Value::Str("n-400".into())), "{at:?}");
    assert!(!at.contains(&Value::Str("n-301".into())), "{at:?}");
}

#[test]
fn property_on_an_edge_staged_earlier_in_the_same_txn() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    let mut txn = w.begin();
    txn.assert(a, "present", Value::Ref(p), t(-300)).unwrap();
    let edge: EntityId = txn.edge_id(a, "present", p).unwrap().into();
    let r = txn.assert(edge, "disguised", b(true), t(-300)).unwrap();
    assert_eq!(r.changed, 1);
    // the txn's own write counts as stored for no-op judgement
    assert_eq!(
        txn.assert(edge, "disguised", b(true), t(-200))
            .unwrap()
            .changed,
        0
    );
    txn.commit().unwrap();
    assert_eq!(
        w.as_of(edge, "disguised", Tick(-300)).unwrap(),
        vec![b(true)]
    );
}

#[test]
fn a_dropped_txn_leaves_no_edge_and_no_property() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    {
        let mut txn = w.begin();
        txn.assert(a, "present", Value::Ref(p), t(0)).unwrap();
        let edge: EntityId = txn.edge_id(a, "present", p).unwrap().into();
        txn.assert(edge, "disguised", b(true), t(0)).unwrap();
    }
    let edge: EntityId = EdgeId::new(1).unwrap().into();
    assert_eq!(
        w.as_of(edge, "disguised", Tick(0)).unwrap_err(),
        Error::UnknownEntity(edge)
    );
}

// ---------------------------------------------------------------------------
// ref-valued properties
// ---------------------------------------------------------------------------

#[test]
fn ref_valued_edge_property_interns_its_own_edge_sourced_at_the_edge() {
    let (mut w, e) = fx();
    let r = w
        .assert(e.edge, "witness", Value::Ref(e.mara), t(-290))
        .unwrap();
    assert_eq!(r.changed, 1);
    let second = w.edge_id(e.edge, "witness", e.mara).expect("edge interned");
    assert_eq!(second.seq(), 2);
    assert_eq!(
        w.edge(second),
        Some((e.edge, "witness".to_string(), e.mara))
    );
    assert_eq!(
        w.as_of(e.edge, "witness", Tick(-290)).unwrap(),
        vec![Value::Ref(e.mara)]
    );
    assert_eq!(w.as_of(e.edge, "witness", Tick(-291)).unwrap(), vec![]);
    // retract and re-assert reuse the same edge
    w.retract(e.edge, "witness", Value::Ref(e.mara), t(-100))
        .unwrap();
    w.assert(e.edge, "witness", Value::Ref(e.mara), t(0))
        .unwrap();
    assert_eq!(w.edge_id(e.edge, "witness", e.mara), Some(second));
}

#[test]
fn a_property_that_is_itself_an_edge_can_carry_properties() {
    let (mut w, e) = fx();
    w.assert(e.edge, "witness", Value::Ref(e.mara), t(-290))
        .unwrap();
    let second: EntityId = w.edge_id(e.edge, "witness", e.mara).unwrap().into();
    w.assert(second, "note", Value::Str("saw the cloak".into()), t(-290))
        .unwrap();
    assert_eq!(
        w.as_of(second, "note", Tick(-290)).unwrap(),
        vec![Value::Str("saw the cloak".into())]
    );
}

#[test]
fn ref_property_pointing_at_an_undefined_entity_is_unknown_entity_and_interns_nothing() {
    let (mut w, e) = fx();
    let ghost = EntityId::new(NodeTypeId(1), 999).unwrap();
    assert_eq!(
        w.assert(e.edge, "witness", Value::Ref(ghost), t(0))
            .unwrap_err(),
        Error::UnknownEntity(ghost)
    );
    w.assert(e.edge, "witness", Value::Ref(e.mara), t(0))
        .unwrap();
    assert_eq!(w.edge_id(e.edge, "witness", e.mara).unwrap().seq(), 2);
}

// ---------------------------------------------------------------------------
// canon views, reopen, tick defaulting
// ---------------------------------------------------------------------------

#[test]
fn at_canon_shows_an_edge_property_only_from_its_tx_on() {
    let (mut w, e) = fx();
    let before = w.latest_tx();
    w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    let mid = w.latest_tx();
    w.retract(e.edge, "disguised", b(true), t(-280)).unwrap();
    w.assert(e.edge, "disguised", b(false), t(-280)).unwrap();
    let after = w.latest_tx();
    assert!(before < mid && mid < after);

    let v = w.at_canon(before);
    assert_eq!(v.as_of(e.edge, "disguised", Tick(-300)).unwrap(), vec![]);
    assert_eq!(v.history(e.edge, "disguised").unwrap(), vec![]);
    let v = w.at_canon(mid);
    assert_eq!(
        v.as_of(e.edge, "disguised", Tick(-290)).unwrap(),
        vec![b(true)]
    );
    assert_eq!(
        v.as_of(e.edge, "disguised", Tick(-280)).unwrap(),
        vec![b(true)]
    );
    assert_eq!(v.history(e.edge, "disguised").unwrap().len(), 1);
    let v = w.at_canon(after);
    assert_eq!(
        v.as_of(e.edge, "disguised", Tick(-280)).unwrap(),
        vec![b(false)]
    );
    assert_eq!(v.history(e.edge, "disguised").unwrap().len(), 3);
}

#[test]
fn a_prequel_property_write_is_visible_from_its_tx_not_its_tick() {
    let (mut w, e) = disguised();
    let tx = w.latest_tx();
    w.assert(e.edge, "note", Value::Str("early".into()), t(-1000))
        .unwrap();
    assert_eq!(
        w.at_canon(tx).as_of(e.edge, "note", Tick(-1000)).unwrap(),
        vec![]
    );
    assert_eq!(
        w.as_of(e.edge, "note", Tick(-1000)).unwrap(),
        vec![Value::Str("early".into())]
    );
}

#[test]
fn edge_properties_survive_reopen_with_every_canon() {
    let dir = tempfile::tempdir().unwrap();
    let p: PathBuf = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let e = setup(&mut w);
    w.assert(e.edge, "disguised", b(true), t(-300)).unwrap();
    w.retract(e.edge, "disguised", b(true), t(-280)).unwrap();
    w.assert(e.edge, "disguised", b(false), t(-280)).unwrap();
    w.assert(e.edge, "witness", Value::Ref(e.mara), t(-290))
        .unwrap();
    let latest = w.latest_tx();
    let snap = |w: &World| -> String {
        let mut s = String::new();
        for tx in 0..=latest.0 {
            let v = w.at_canon(TxId(tx));
            for a in ["disguised", "witness", "note"] {
                s += &format!("{tx} {a} {:?}\n", v.history(e.edge, a));
                for k in [i64::MIN, -301, -300, -290, -280, -250, 0, i64::MAX] {
                    s += &format!("{tx} {a} @{k} {:?}\n", v.as_of(e.edge, a, Tick(k)));
                }
            }
        }
        s
    };
    let before = snap(&w);
    drop(w);
    for _ in 0..2 {
        let w = World::open(&p).unwrap();
        assert_eq!(w.latest_tx(), latest);
        assert_eq!(snap(&w), before);
        assert_eq!(w.edge_id(e.edge, "witness", e.mara).unwrap().seq(), 2);
        drop(w);
    }
    // and the world keeps accepting property writes after reopen
    let mut w = World::open(&p).unwrap();
    let r = w
        .assert(e.edge, "note", Value::Str("x".into()), t(5))
        .unwrap();
    assert_eq!(r.changed, 1);
}

#[test]
fn property_write_without_a_tick_uses_the_worlds_latest_tick_and_reports_it() {
    let (mut w, e) = fx();
    // setup's highest tick is -250 (the retract)
    assert_eq!(w.latest_tick(), Tick(-250));
    let r = w.assert(e.edge, "disguised", b(true), None).unwrap();
    assert_eq!((r.changed, r.tick), (1, Tick(-250)));
    assert!(holds(&w, e.edge, true, -250));
    assert!(!holds(&w, e.edge, true, -251));
    // later facts move the latest tick; the next defaulted write follows it
    w.assert(e.mara, "present", Value::Ref(e.carnath), t(40))
        .unwrap();
    let r = w
        .assert(e.edge, "note", Value::Str("late".into()), None)
        .unwrap();
    assert_eq!(r.tick, Tick(40));
    // a property write's own tick moves the latest tick too
    w.assert(e.edge, "note", Value::Str("later".into()), t(90))
        .unwrap();
    assert_eq!(w.latest_tick(), Tick(90));
}

#[test]
fn property_write_in_a_txn_uses_the_txn_default_then_the_start_latest_tick() {
    let (mut w, e) = fx();
    let mut txn = w.begin();
    txn.set_default_tick(Tick(-208));
    let r = txn.assert(e.edge, "disguised", b(true), None).unwrap();
    assert_eq!(r.tick, Tick(-208));
    let r = txn
        .assert(e.edge, "note", Value::Str("n".into()), t(7))
        .unwrap();
    assert_eq!(r.tick, Tick(7));
    txn.commit().unwrap();

    let mut txn = w.begin(); // latest tick is now 7
    let r = txn.assert(e.edge, "disguised", b(false), None).unwrap();
    assert_eq!(r.tick, Tick(7));
    txn.commit().unwrap();
    assert!(holds(&w, e.edge, false, 7));
    assert!(!holds(&w, e.edge, false, 6));
}
