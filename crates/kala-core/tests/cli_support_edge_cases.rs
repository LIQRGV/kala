//! Edge cases for the slice 5 CLI accessors (`View::latest_tick` at older
//! canons, `parse_id`, `handle_of`, `entries`). Spec: CLAUDE.md "Core ideas"
//! (latest tick) and "How corrections are written".

use kala_core::log::EntryBody;
use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
}

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn replace_tick(x: i64) -> Correction {
    Correction::Replace {
        value: None,
        tick: Some(Tick(x)),
        op: None,
        precision: None,
    }
}

/// `latest_tick` at every canon from 0 to the latest tx.
fn per_canon(w: &World) -> Vec<Tick> {
    (0..=w.latest_tx().0)
        .map(|n| w.at_canon(TxId(n)).latest_tick())
        .collect()
}

fn ticks(v: &[i64]) -> Vec<Tick> {
    v.iter().map(|&x| Tick(x)).collect()
}

struct Cast {
    greg: EntityId,
    p1: EntityId,
    p2: EntityId,
    p3: EntityId,
}

fn cast(w: &mut World) -> Cast {
    Cast {
        greg: w.create_entity("CHARACTER", Some("greg")).unwrap(),
        p1: w.create_entity("PLACE", None).unwrap(),
        p2: w.create_entity("PLACE", None).unwrap(),
        p3: w.create_entity("PLACE", None).unwrap(),
    }
}

// ---------- View::latest_tick at older canons ----------

#[test]
fn latest_tick_delete_then_reassert_same_key_every_canon() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = cast(&mut w);
    let base = w.latest_tx().0; // entity-creating txs: latest tick 0
    let _ = w
        .assert(c.greg, "located_in", Value::Ref(c.p1), t(10))
        .unwrap(); // base+1
    let b = w
        .assert(c.greg, "located_in", Value::Ref(c.p2), t(50))
        .unwrap(); // base+2
    w.correct(b.entries[0], Correction::Delete).unwrap(); // base+3
    w.assert(c.greg, "located_in", Value::Ref(c.p2), t(50))
        .unwrap(); // base+4: same key again
    let got = per_canon(&w);
    let tail = &got[base as usize..];
    assert_eq!(tail, &ticks(&[0, 10, 50, 10, 50])[..], "all: {got:?}");
    assert!(got[..base as usize].iter().all(|x| *x == Tick(0)));
    assert_eq!(w.latest_tick(), Tick(50));
}

#[test]
fn latest_tick_replace_onto_a_tick_another_live_fact_holds() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = cast(&mut w);
    let base = w.latest_tx().0;
    w.assert(c.greg, "located_in", Value::Ref(c.p1), t(10))
        .unwrap(); // +1
    let b = w
        .assert(c.greg, "located_in", Value::Ref(c.p2), t(50))
        .unwrap(); // +2
    let cc = w
        .assert(c.greg, "located_in", Value::Ref(c.p3), t(30))
        .unwrap(); // +3
    w.correct(cc.entries[0], replace_tick(50)).unwrap(); // +4: two live facts at 50
    w.correct(b.entries[0], Correction::Delete).unwrap(); // +5: one left at 50
    w.correct(cc.entries[0], Correction::Delete).unwrap(); // +6: none
    let got = per_canon(&w);
    assert_eq!(
        &got[base as usize..],
        &ticks(&[0, 10, 50, 50, 50, 50, 10])[..],
        "all: {got:?}"
    );
}

#[test]
fn latest_tick_counts_edge_property_facts() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = cast(&mut w);
    let base = w.latest_tx().0;
    let r = w
        .assert(c.greg, "located_in", Value::Ref(c.p1), t(10))
        .unwrap(); // +1
    let edge = EntityId::from(r.edges[0]);
    let p = w
        .assert(edge, "disguised", Value::Bool(true), t(99))
        .unwrap(); // +2
    let got = per_canon(&w);
    assert_eq!(got[base as usize + 1], Tick(10));
    assert_eq!(got[base as usize + 2], Tick(99));
    assert_eq!(w.latest_tick(), Tick(99));
    // deleting the property fact takes it back out at that canon
    w.correct(p.entries[0], Correction::Delete).unwrap();
    let last = w.latest_tx();
    assert_eq!(w.at_canon(last).latest_tick(), Tick(10));
    assert_eq!(w.at_canon(TxId(last.0 - 1)).latest_tick(), Tick(99));
}

#[test]
fn latest_tick_same_on_reopen_at_every_canon() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let before = {
        let mut w = World::create(&p, schema()).unwrap();
        let c = cast(&mut w);
        let a = w
            .assert(c.greg, "located_in", Value::Ref(c.p1), t(-7))
            .unwrap();
        let b = w
            .assert(c.greg, "located_in", Value::Ref(c.p2), t(40))
            .unwrap();
        w.correct(b.entries[0], replace_tick(5)).unwrap();
        w.correct(a.entries[0], Correction::Delete).unwrap();
        w.assert(c.greg, "located_in", Value::Ref(c.p3), t(i64::MIN))
            .unwrap();
        per_canon(&w)
    };
    let w = World::open(&p).unwrap();
    assert_eq!(per_canon(&w), before);
    assert!(before.contains(&Tick(40)) && before.contains(&Tick(5)));
}

#[test]
fn latest_tick_unchanged_by_define_only_transactions() {
    let mut w = World::in_memory(schema()).unwrap();
    let g = w.create_entity("CHARACTER", None).unwrap();
    w.assert(g, "name", Value::Str("x".into()), t(77)).unwrap();
    let before = w.latest_tx();
    let _ = w.create_entity("PLACE", Some("a")).unwrap();
    let _ = w.create_entity("CHARACTER", None).unwrap();
    assert!(w.latest_tx().0 > before.0);
    for n in before.0..=w.latest_tx().0 {
        assert_eq!(w.at_canon(TxId(n)).latest_tick(), Tick(77), "canon {n}");
    }
    assert_eq!(w.latest_tick(), Tick(77));
}

#[test]
fn latest_tick_at_canon_u64_max_equals_latest() {
    let mut w = World::in_memory(schema()).unwrap();
    assert_eq!(w.at_canon(TxId(u64::MAX)).latest_tick(), w.latest_tick());
    let c = cast(&mut w);
    w.assert(c.greg, "located_in", Value::Ref(c.p1), t(i64::MAX))
        .unwrap();
    let v = w.at_canon(TxId(u64::MAX));
    assert_eq!(v.latest_tick(), Tick(i64::MAX));
    assert_eq!(v.latest_tick(), w.latest_tick());
    // and canon 0 on a populated world is the empty world's tick
    assert_eq!(w.at_canon(TxId(0)).latest_tick(), Tick(0));
}

// ---------- parse_id ----------

#[test]
fn parse_id_round_trips_type_name_containing_hash() {
    let s = Schema::new().node_type("A#B").node_type("CHARACTER");
    let mut w = match World::in_memory(s) {
        Ok(w) => w,
        Err(e) => {
            // Schema refuses '#': nothing to round-trip.
            println!("schema refuses '#' in a node type name: {e}");
            return;
        }
    };
    let id = w.create_entity("A#B", None).unwrap();
    let text = w.format_id(id);
    assert_eq!(text, "A#B#1");
    assert_eq!(w.parse_id(&text), Some(id));
    // a prefix of the name is not a type
    assert_eq!(w.parse_id("A#1"), None);
    assert_eq!(w.parse_id("B#1"), None);
}

/// FINDING (for the PM, not asserted as a bug): a node type named `edge`
/// collides with the `edge#N` form. This documents current behaviour.
#[test]
fn parse_id_node_type_named_edge_is_shadowed_by_the_edge_form() {
    let s = Schema::new().node_type("edge").node_type("CHARACTER");
    let mut w = match World::in_memory(s) {
        Ok(w) => w,
        Err(e) => {
            println!("schema refuses a node type named `edge`: {e}");
            return;
        }
    };
    let id = w.create_entity("edge", None).unwrap();
    assert!(!id.is_edge());
    assert_eq!(w.format_id(id), "edge#1");
    // Documented current behaviour: parse_id reads `edge#1` as the EDGE,
    // so the entity's own formatted ID does not round-trip.
    let parsed = w.parse_id("edge#1").unwrap();
    assert!(parsed.is_edge());
    assert_ne!(parsed, id);
}

// ---------- handle_of ----------

#[test]
fn handle_of_unicode_handles_after_reopen_match_define_entries() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let handles = ["édwin", "Ωmega", "日本語", "e\u{301}x", "😀", "plain"];
    let mut ids = Vec::new();
    {
        let mut w = World::create(&p, schema()).unwrap();
        for h in handles {
            ids.push(w.create_entity("CHARACTER", Some(h)).unwrap());
        }
        ids.push(w.create_entity("PLACE", None).unwrap());
    }
    let w = World::open(&p).unwrap();
    for (i, h) in handles.iter().enumerate() {
        assert_eq!(w.handle_of(ids[i]), Some(*h));
        assert_eq!(w.entity_by_handle(h), Some(ids[i]));
    }
    assert_eq!(w.handle_of(*ids.last().unwrap()), None);
    // every entity's handle equals the DefineEntity handle in the log
    let mut seen = 0;
    for e in w.entries() {
        if let EntryBody::DefineEntity { id, handle } = e.body {
            assert_eq!(w.handle_of(id), handle.as_deref());
            seen += 1;
        }
    }
    assert_eq!(seen, ids.len());
}

// ---------- entries ----------

#[test]
fn entries_dense_increasing_and_each_tx_ends_in_its_commit() {
    let mut w = World::in_memory(schema()).unwrap();
    let c = cast(&mut w);
    let a = w
        .assert(c.greg, "located_in", Value::Ref(c.p1), t(1))
        .unwrap();
    // multi-write transaction
    let mut txn = w.begin();
    txn.assert(c.greg, "located_in", Value::Ref(c.p2), t(2))
        .unwrap();
    txn.assert(c.greg, "name", Value::Str("G".into()), t(2))
        .unwrap();
    txn.commit().unwrap();
    // failed writes: autocommit error, poisoned txn, dropped txn
    assert!(w.assert(c.greg, "nope", Value::Bool(true), t(3)).is_err());
    {
        let mut txn = w.begin();
        txn.assert(c.greg, "located_in", Value::Ref(c.p3), t(3))
            .unwrap();
        assert!(txn.assert(c.greg, "name", Value::Bool(true), t(3)).is_err());
        assert!(txn.commit().is_err());
    }
    {
        let mut txn = w.begin();
        txn.create_entity("PLACE", Some("ghost")).unwrap();
        txn.assert(c.greg, "located_in", Value::Ref(c.p3), t(4))
            .unwrap();
        drop(txn);
    }
    // no-op write
    w.assert(c.greg, "located_in", Value::Ref(c.p1), t(1))
        .unwrap();
    // corrections
    w.correct(a.entries[0], replace_tick(9)).unwrap();
    w.correct(a.entries[0], Correction::Delete).unwrap();
    assert!(w.entity_by_handle("ghost").is_none());

    let all: Vec<EntryInfo> = w.entries().collect();
    assert!(!all.is_empty());
    for pair in all.windows(2) {
        assert_eq!(pair[1].id.0, pair[0].id.0 + 1, "dense and increasing");
    }
    // group by tx: contiguous, ends in matching Commit, only one Commit
    let mut i = 0;
    let mut last_tx: Option<u64> = None;
    while i < all.len() {
        let tx = all[i].tx;
        let mut j = i;
        while j < all.len() && all[j].tx == tx {
            j += 1;
        }
        if let Some(l) = last_tx {
            assert!(tx.0 > l, "tx increasing");
        }
        last_tx = Some(tx.0);
        let group = &all[i..j];
        for e in &group[..group.len() - 1] {
            assert!(
                !matches!(e.body, EntryBody::Commit { .. }),
                "early commit in {tx:?}"
            );
        }
        match &group[group.len() - 1].body {
            EntryBody::Commit { tx: ctx, .. } => assert_eq!(*ctx, tx),
            other => panic!("tx {tx:?} does not end in Commit: {other:?}"),
        }
        i = j;
    }
    assert_eq!(last_tx, Some(w.latest_tx().0));
}
