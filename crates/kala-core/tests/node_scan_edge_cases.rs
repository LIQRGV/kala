//! Slice 4, item 4 edge cases (sdet, confirm phase): `entities_of_type` after a hash
//! rebuild and after a Damaged cut (acknowledged or not), the precedence of
//! `TxnKeyConflict` over `EntryTooLarge` at the size boundary, and plain values planted
//! on a ref attribute's `E`/`E_latest` keys (reads must skip them, never panic).
mod common;

use kala_core::log::MAX_ENTRY_BYTES;
use kala_core::*;
use std::path::PathBuf;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .node_type("GHOST")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
        .attribute(
            AttrDef::new("ally_of", ValueType::Ref, Kind::State, Cardinality::Many).bidirectional(),
        )
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

fn scan(w: &World, ty: &str) -> Vec<EntityId> {
    w.entities_of_type(ty).unwrap().collect()
}

// ------------------------------------------------------------ set_content_hash rebuild

#[test]
fn entities_of_type_is_unchanged_by_a_content_hash_rebuild_at_every_canon() {
    let mut w = World::in_memory(schema()).unwrap();
    let mut cs = vec![];
    let mut ps = vec![];
    for i in 0..6 {
        cs.push(w.create_entity("CHARACTER", None).unwrap());
        if i % 2 == 0 {
            ps.push(w.create_entity("PLACE", Some(&format!("p{i}"))).unwrap());
        }
        w.assert(cs[i], "name", Value::Str(format!("n{i}")), t(i as i64))
            .unwrap();
    }
    w.assert(cs[0], "located_in", Value::Ref(ps[0]), t(1))
        .unwrap();
    let latest = w.latest_tx();
    let snapshot = |w: &World| {
        let mut out = vec![];
        for tx in 0..=latest.0 {
            for ty in ["CHARACTER", "PLACE", "GHOST"] {
                out.push((
                    tx,
                    ty,
                    w.at_canon(TxId(tx))
                        .entities_of_type(ty)
                        .unwrap()
                        .collect::<Vec<_>>(),
                ));
            }
        }
        out
    };
    let before = snapshot(&w);
    assert_eq!(scan(&w, "CHARACTER"), cs);
    assert_eq!(scan(&w, "PLACE"), ps);
    w.set_content_hash(|_| 0);
    assert_eq!(snapshot(&w), before);
    assert_eq!(scan(&w, "CHARACTER"), cs);
    // still extendable after the rebuild, and the new entity is listed last
    let c7 = w.create_entity("CHARACTER", None).unwrap();
    assert_eq!(scan(&w, "CHARACTER").last(), Some(&c7));
    assert_eq!(scan(&w, "CHARACTER").len(), cs.len() + 1);
    clean(&w);
}

// ------------------------------------------------------------ Damaged cut

fn damaged(dir: &tempfile::TempDir) -> (PathBuf, Vec<EntityId>) {
    let path = dir.path().join("w.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap(); // tx 1
    let b = w.create_entity("CHARACTER", Some("b")).unwrap(); // tx 2
    let p = w.create_entity("PLACE", Some("p")).unwrap(); // tx 3
    let c = w.create_entity("CHARACTER", Some("c")).unwrap(); // tx 4: cut
    assert_eq!(w.latest_tx(), TxId(4));
    drop(w);
    let mut b_ = std::fs::read(&path).unwrap();
    let last = *common::spans(&b_).last().unwrap();
    b_[last.body_start + 6] ^= 0xFF;
    std::fs::write(&path, &b_).unwrap();
    (path, vec![a, b, p, c])
}

#[test]
fn entities_of_type_after_a_damaged_cut_lists_only_the_committed_entities_acknowledged_or_not() {
    let dir = tempfile::tempdir().unwrap();
    let (path, ids) = damaged(&dir);
    let mut w = World::open(&path).unwrap();
    assert_eq!(w.recovery().cause, Some(CutCause::Damaged));
    // unacknowledged: reads work
    assert_eq!(scan(&w, "CHARACTER"), vec![ids[0], ids[1]]);
    assert_eq!(scan(&w, "PLACE"), vec![ids[2]]);
    assert!(scan(&w, "GHOST").is_empty());
    assert_eq!(
        w.at_canon(TxId(1))
            .entities_of_type("CHARACTER")
            .unwrap()
            .collect::<Vec<_>>(),
        vec![ids[0]]
    );
    // a canon past the cut sees no more than the latest
    assert_eq!(
        w.at_canon(TxId(99))
            .entities_of_type("CHARACTER")
            .unwrap()
            .collect::<Vec<_>>(),
        vec![ids[0], ids[1]]
    );
    // acknowledged: same answers, and the next entity reuses the cut sequence
    w.acknowledge_recovery();
    assert_eq!(scan(&w, "CHARACTER"), vec![ids[0], ids[1]]);
    let c = w.create_entity("CHARACTER", Some("c2")).unwrap();
    assert_eq!(c, ids[3], "the cut entity's number was never committed");
    assert_eq!(scan(&w, "CHARACTER"), vec![ids[0], ids[1], c]);
    clean(&w);
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(scan(&w, "CHARACTER"), vec![ids[0], ids[1], c]);
    assert_eq!(scan(&w, "PLACE"), vec![ids[2]]);
}

#[test]
fn entities_of_type_after_a_damaged_cut_is_stable_under_a_hash_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let (path, ids) = damaged(&dir);
    let mut w = World::open(&path).unwrap();
    w.set_content_hash(|_| 0);
    assert_eq!(scan(&w, "CHARACTER"), vec![ids[0], ids[1]]);
    assert_eq!(scan(&w, "PLACE"), vec![ids[2]]);
}

// ------------------------------------------------------------ size boundary vs TxnKeyConflict

/// A bio value of the largest size the world accepts (found by probing).
fn biggest_bio(w: &mut World, e: EntityId, tick: i64) -> (usize, EntryId) {
    let mut n = MAX_ENTRY_BYTES - 40;
    loop {
        match w.assert(e, "bio", Value::Str("x".repeat(n)), t(tick)) {
            Ok(r) => return (n, r.entries[0]),
            Err(Error::EntryTooLarge { .. }) => n -= 1,
            Err(e) => panic!("unexpected {e:?}"),
        }
        assert!(n > MAX_ENTRY_BYTES - 200, "probe ran away");
    }
}

#[test]
fn a_second_correction_of_a_max_accepted_value_reports_txn_key_conflict_not_entry_too_large() {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", None).unwrap();
    let (n, entry) = biggest_bio(&mut w, a, 1);
    // one byte more is refused
    assert!(matches!(
        w.assert(a, "bio", Value::Str("x".repeat(n + 1)), t(2)),
        Err(Error::EntryTooLarge { .. })
    ));
    // the same chain twice in one txn: the conflict is judged before the size
    // (a failed txn write aborts the txn, so each case gets its own)
    for c in [
        Correction::Delete,
        Correction::Replace {
            value: None,
            tick: t(5),
            op: None,
            precision: None,
        },
    ] {
        let mut txn = w.begin();
        // the first correction fits (the value is at the limit, not past it)
        txn.correct(entry, Correction::Delete).unwrap();
        let err = txn.correct(entry, c.clone()).unwrap_err();
        assert!(
            matches!(err, Error::TxnKeyConflict { .. }),
            "{c:?}: {err:?}"
        );
    }
}

// ------------------------------------------------------------ plain value planted on a ref attribute

fn ref_world() -> (World, EntityId, EntityId, AttrId) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    w.assert(a, "located_in", Value::Ref(p), t(0)).unwrap();
    w.assert(a, "ally_of", Value::Ref(b), t(0)).unwrap();
    let attr = |name: &str| {
        let d = w.index_dump();
        let row = d
            .e
            .iter()
            .find(
                |r| matches!(&r.2, Value::Ref(x) if *x == if name == "located_in" { p } else { b }),
            )
            .unwrap();
        row.1
    };
    let li = attr("located_in");
    (w, a, p, li)
}

fn planted_key(e: EntityId, attr: AttrId, part: &[u8], latest: bool) -> Vec<u8> {
    let mut k = vec![b'E'];
    k.extend_from_slice(&e.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(part);
    k.extend_from_slice(&Tick(0).encode_inverted());
    if !latest {
        k.extend_from_slice(&(!1u64).to_be_bytes());
    }
    k
}

fn reads(w: &World, a: EntityId, p: EntityId) {
    for at in [i64::MIN, -1, 0, 1, i64::MAX] {
        let tk = Tick(at);
        for attr in ["located_in", "ally_of"] {
            for dir in [Direction::Out, Direction::In, Direction::Both] {
                let _ = w.neighbors(a, attr, dir, tk);
                let _ = w.neighbors(p, attr, dir, tk);
            }
            let _ = w.as_of(a, attr, tk);
            let _ = w.history(a, attr);
            let _ = w.lookup(attr, &Value::Ref(p), tk);
            let _ = w.lookup(attr, &Value::Ref(a), tk);
        }
        for view in [
            w.at_canon(w.latest_tx()),
            w.at_canon(TxId(3)),
            w.at_canon_main(w.latest_tx()),
        ] {
            let _ = view.neighbors(a, "located_in", Direction::Out, tk);
            let _ = view.neighbors(a, "ally_of", Direction::Both, tk);
            let _ = view.as_of(a, "located_in", tk);
            let _ = view.lookup("located_in", &Value::Ref(p), tk);
            let _ = view.lookup("ally_of", &Value::Ref(a), tk);
        }
    }
    let _ = w.overlaps("located_in");
    let _ = w.index_dump();
}

#[test]
fn a_plain_value_planted_on_a_ref_attributes_e_keys_is_skipped_by_every_read() {
    let plains = [
        Value::Bool(true),
        Value::I64(7),
        Value::F64(1.5),
        Value::Str("not a ref".into()),
        Value::Str(String::new()),
    ];
    for index in [IndexName::E, IndexName::ELatest] {
        for plain in &plains {
            let (mut w, a, p, li) = ref_world();
            let baseline_n = w
                .neighbors(a, "located_in", Direction::Out, Tick(0))
                .unwrap();
            let baseline_l = w.lookup("located_in", &Value::Ref(p), Tick(0)).unwrap();
            let key = planted_key(a, li, &plain.encode().unwrap(), index == IndexName::ELatest);
            w.tamper_index(Tamper::InsertRawKey { index, key });
            // strict: nothing panics (the plain value is skipped), and the real answers stand
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reads(&w, a, p)));
            assert!(r.is_ok(), "{index:?} {plain:?}: a read panicked");
            assert_eq!(
                w.neighbors(a, "located_in", Direction::Out, Tick(0))
                    .unwrap(),
                baseline_n,
                "{index:?} {plain:?}"
            );
            assert_eq!(
                w.lookup("located_in", &Value::Ref(p), Tick(0)).unwrap(),
                baseline_l,
                "{index:?} {plain:?}"
            );
            // doctor reports (never panics)
            let _ = doctor(&w, &DoctorOptions::default());
        }
    }
}

#[test]
fn a_plain_value_planted_on_a_bidirectional_ref_attribute_is_skipped_by_every_read() {
    for index in [IndexName::E, IndexName::ELatest] {
        let (mut w, a, p, _) = ref_world();
        let ally = {
            let d = w.index_dump();
            d.e.iter()
                .find(|r| matches!(&r.2, Value::Ref(x) if x.node_type() == a.node_type()))
                .unwrap()
                .1
        };
        for who in [a, w.entity_by_handle("b").unwrap()] {
            let key = planted_key(
                who,
                ally,
                &Value::Bool(false).encode().unwrap(),
                index == IndexName::ELatest,
            );
            w.tamper_index(Tamper::InsertRawKey { index, key });
        }
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reads(&w, a, p)));
        assert!(r.is_ok(), "{index:?}: a read panicked");
    }
}
