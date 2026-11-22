//! Slice 4 item 1, confirm round: edge cases of the exact V index
//! (docs/slice-4-decisions.md "Item 1" and "Decisions added during execution").
//!
//! - `-0.0` and `0.0` are the same value for lookup, whichever was written.
//! - an `.unindexed()` ref attribute still answers `lookup` from R.
//! - a Replace that flips only the op keeps V, V_latest and the canon reads right.
//! - doctor `since` judges V / V_latest rows by tx, as it does E rows.
//! - short / garbage V and V_latest keys never panic `lookup` and are an
//!   `IndexDecode` for doctor.

use kala_core::*;

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
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "weight",
            ValueType::F64,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(
            AttrDef::new("knows", ValueType::Ref, Kind::State, Cardinality::Many).unindexed(),
        )
}

fn world() -> (World, EntityId, EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    let p = w.create_entity("PLACE", Some("p")).unwrap();
    (w, a, b, p)
}

fn lk(w: &World, attr: &str, v: Value, at: i64) -> Vec<EntityId> {
    w.lookup(attr, &v, Tick(at)).unwrap()
}

fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}

// ---------------------------------------------------------------- -0.0

#[test]
fn lookup_of_negative_zero_finds_a_value_written_as_positive_zero() {
    let (mut w, a, _, _) = world();
    w.assert(a, "weight", Value::F64(0.0), t(0)).unwrap();
    assert_eq!(lk(&w, "weight", Value::F64(-0.0), 0), vec![a]);
    assert_eq!(lk(&w, "weight", Value::F64(0.0), 0), vec![a]);
    let tx = w.latest_tx();
    let main = w
        .at_canon_main(tx)
        .lookup("weight", &Value::F64(-0.0), Tick(0))
        .unwrap();
    assert_eq!(main, vec![a]);
    clean(&w);
}

#[test]
fn lookup_of_positive_zero_finds_a_value_written_as_negative_zero() {
    let (mut w, a, b, _) = world();
    w.assert(a, "weight", Value::F64(-0.0), t(0)).unwrap();
    w.assert(b, "weight", Value::F64(0.0), t(0)).unwrap();
    assert_eq!(lk(&w, "weight", Value::F64(0.0), 0), vec![a, b]);
    assert_eq!(lk(&w, "weight", Value::F64(-0.0), 0), vec![a, b]);
    // retracting through the other spelling ends the same value
    w.retract(a, "weight", Value::F64(0.0), t(3)).unwrap();
    assert_eq!(lk(&w, "weight", Value::F64(-0.0), 3), vec![b]);
    assert_eq!(lk(&w, "weight", Value::F64(-0.0), 2), vec![a, b]);
    clean(&w);
}

// ---------------------------------------------------------------- unindexed ref

#[test]
fn lookup_on_an_unindexed_ref_attribute_still_answers_from_r() {
    let (mut w, a, b, p) = world();
    w.assert(a, "knows", Value::Ref(p), t(1)).unwrap();
    w.assert(b, "knows", Value::Ref(p), t(2)).unwrap();
    w.retract(a, "knows", Value::Ref(p), t(5)).unwrap();
    assert_eq!(lk(&w, "knows", Value::Ref(p), 0), vec![]);
    assert_eq!(lk(&w, "knows", Value::Ref(p), 1), vec![a]);
    assert_eq!(lk(&w, "knows", Value::Ref(p), 2), vec![a, b]);
    assert_eq!(lk(&w, "knows", Value::Ref(p), 5), vec![b]);
    assert_eq!(lk(&w, "knows", Value::Ref(a), 5), vec![]);
    // no V rows for a ref attribute, indexed or not
    assert!(w.index_dump().v.is_empty());
    clean(&w);
}

// ---------------------------------------------------------------- op-only Replace

fn flip(op: Op) -> Correction {
    Correction::Replace {
        value: None,
        tick: None,
        op: Some(op),
        precision: None,
    }
}

#[test]
fn a_replace_that_flips_only_the_op_updates_v_and_keeps_older_canon() {
    let (mut w, a, _, _) = world();
    w.assert(a, "name", s("Edwin"), t(1)).unwrap();
    let asserted = w.latest_tx();
    let r = w.assert(a, "name", s("Greg"), t(5)).unwrap();
    let greg_entry = r.entries[0];
    let before = w.latest_tx();
    assert_eq!(lk(&w, "name", s("Greg"), 5), vec![a]);

    // Greg's assert at 5 becomes a retract at 5
    let res = w.correct(greg_entry, flip(Op::Retract)).unwrap();
    assert_eq!(res.changed, 1, "same-key Revised");
    assert_eq!(lk(&w, "name", s("Greg"), 5), vec![]);
    assert_eq!(lk(&w, "name", s("Greg"), 4), vec![]);
    assert_eq!(lk(&w, "name", s("Edwin"), 5), vec![a]);

    // older canon still sees the assert; main and latest paths agree
    assert_eq!(
        w.at_canon(before)
            .lookup("name", &s("Greg"), Tick(5))
            .unwrap(),
        vec![a]
    );
    for tx in [asserted, before, w.latest_tx()] {
        for at in [0, 1, 4, 5, 6] {
            for v in ["Edwin", "Greg"] {
                assert_eq!(
                    w.at_canon(tx).lookup("name", &s(v), Tick(at)).unwrap(),
                    w.at_canon_main(tx).lookup("name", &s(v), Tick(at)).unwrap(),
                    "{v} at {at} canon {tx:?}"
                );
            }
        }
    }
    // V mirrors E for the string attribute
    let d = w.index_dump();
    let string_e = d.e.iter().filter(|r| matches!(r.2, Value::Str(_))).count();
    assert_eq!(d.v.len(), string_e);
    clean(&w);

    // and back: flip the same chain to an assert again
    let res = w.correct(greg_entry, flip(Op::Assert));
    // the chain's current key is judged like any write; either it is stored
    // or it is a no-op, but never an error and never a doctor finding
    assert!(res.is_ok(), "{res:?}");
    clean(&w);
}

// ---------------------------------------------------------------- doctor since

fn since(n: u64) -> DoctorOptions {
    DoctorOptions {
        canon: None,
        since: Some(TxId(n)),
    }
}

fn mismatch_on(rep: &Report, which: IndexName) -> bool {
    rep.findings
        .iter()
        .any(|f| matches!(f, Finding::IndexMismatch { index, .. } if *index == which))
}

/// name facts in three separate txs (no corrections: one row per key)
fn three_names() -> (World, Vec<TxId>) {
    let (mut w, a, b, _) = world();
    let mut txs = vec![];
    for (e, n, at) in [(a, "Edwin", 1), (b, "Greg", 2), (a, "Mara", 3)] {
        w.assert(e, "name", s(n), t(at)).unwrap();
        txs.push(w.latest_tx());
    }
    (w, txs)
}

#[test]
fn doctor_since_judges_v_rows_by_their_tx() {
    for which in [IndexName::V, IndexName::VLatest] {
        let (base, txs) = three_names();
        let dump = base.index_dump();
        for (i, tx) in txs.iter().enumerate() {
            let mut w = three_names().0;
            let nth = dump.v.iter().position(|r| r.4 == *tx).unwrap();
            w.tamper_index(Tamper::RemoveRow { index: which, nth });
            let rep = doctor(&w, &DoctorOptions::default());
            assert!(mismatch_on(&rep, which), "{which:?} row {i}");
            // reported while since < tx
            assert!(mismatch_on(&doctor(&w, &since(tx.0 - 1)), which));
            // hidden from since >= tx, and nothing else shows
            for n in [tx.0, tx.0 + 1, 99] {
                let rep = doctor(&w, &since(n));
                assert!(
                    rep.is_clean(),
                    "{which:?} row {i} since {n}: {:#?}",
                    rep.findings
                );
            }
        }
    }
}

#[test]
fn doctor_since_also_judges_a_flipped_v_row() {
    for which in [IndexName::V, IndexName::VLatest] {
        let (base, txs) = three_names();
        let nth = base
            .index_dump()
            .v
            .iter()
            .position(|r| r.4 == txs[1])
            .unwrap();
        let mut w = three_names().0;
        w.tamper_index(Tamper::FlipOp { index: which, nth });
        assert!(mismatch_on(&doctor(&w, &since(txs[1].0 - 1)), which));
        assert!(doctor(&w, &since(txs[1].0)).is_clean());
        assert!(mismatch_on(&doctor(&w, &since(0)), which));
    }
}

// ---------------------------------------------------------------- garbage keys

fn attr_id(w: &World) -> AttrId {
    let d = w.index_dump();
    d.v.iter().find(|r| matches!(r.1, Value::Str(_))).unwrap().0
}

fn garbage_v_keys(w: &World) -> Vec<Vec<u8>> {
    let attr = attr_id(w);
    let edwin = s("Edwin").encode().unwrap();
    let mut prefix = vec![b'V'];
    prefix.extend_from_slice(&attr.0.to_be_bytes());
    let mut out: Vec<Vec<u8>> = vec![
        b"V".to_vec(),
        b"V\x00".to_vec(),
        prefix.clone(), // attr only, shorter than any history prefix
        {
            let mut k = prefix.clone();
            k.push(edwin[0]); // only the value's type tag
            k
        },
        {
            let mut k = prefix.clone();
            k.extend_from_slice(&edwin[..edwin.len() - 1]); // value cut short
            k
        },
        {
            // inside the lookup prefix for "Edwin": value + 3 stray bytes
            let mut k = prefix.clone();
            k.extend_from_slice(&edwin);
            k.extend_from_slice(&[1, 2, 3]);
            k
        },
        {
            let mut k = prefix.clone();
            k.extend_from_slice(&edwin);
            k.extend_from_slice(&[0u8; 8 + 8 + 8 - 1]); // one byte short of a latest key
            k
        },
        {
            let mut k = prefix.clone();
            k.extend_from_slice(&edwin);
            k.extend_from_slice(&[0u8; 8 + 8 + 8 + 8 + 1]); // one byte too long
            k
        },
        vec![b'V', 0xFF, 0xFF, 0xFF, 0xFF, 0xEE],
        vec![0xFF; 40],
    ];
    out.push(vec![]);
    out
}

#[test]
fn garbage_v_and_v_latest_keys_never_panic_lookup_and_doctor_reports_index_decode() {
    for which in [IndexName::V, IndexName::VLatest] {
        let (base, _) = three_names();
        let keys = garbage_v_keys(&base);
        let a = base.entity_by_handle("a").unwrap();
        for key in keys {
            let (mut w, _) = three_names();
            let baseline = lk(&w, "name", s("Edwin"), 10);
            assert_eq!(baseline, vec![a]);
            w.tamper_index(Tamper::InsertRawKey {
                index: which,
                key: key.clone(),
            });
            // never panics, whatever it answers; if it answers, it is the
            // real answer
            for v in ["Edwin", "Greg", "", "zzz"] {
                for at in [i64::MIN, 0, 2, 10, i64::MAX] {
                    let _ = w.lookup("name", &s(v), Tick(at));
                    let _ = w.at_canon(TxId(2)).lookup("name", &s(v), Tick(at));
                    let _ = w
                        .at_canon_main(w.latest_tx())
                        .lookup("name", &s(v), Tick(at));
                }
            }
            if let Ok(got) = w.lookup("name", &s("Edwin"), Tick(10)) {
                assert_eq!(got, baseline, "{which:?} {key:?}");
            }
            let _ = w.index_dump();
            let rep = doctor(&w, &DoctorOptions::default());
            assert!(
                rep.findings.iter().any(|f| matches!(
                    f,
                    Finding::IndexDecode { index, key: k } if *index == which && *k == key
                )),
                "{which:?} {key:?}: {:#?}",
                rep.findings
            );
        }
    }
}
