//! Slice 4, item 3 edge cases (sdet, confirm phase): crafted logs at the entry-size
//! limit, never-stored values, dropped transactions under total hash collision,
//! tampered `E` keys, doctor `since` on unindexed rows.
mod common;

use kala_core::log::{EntryBody, MAX_ENTRY_BYTES, read_log};
use kala_core::*;
use std::path::PathBuf;

fn t(x: i64) -> Option<Tick> {
    Some(Tick(x))
}
fn s(x: &str) -> Value {
    Value::Str(x.into())
}
fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .attribute(AttrDef::new(
            "name",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new("bio", ValueType::Str, Kind::State, Cardinality::One).unindexed())
        .attribute(
            AttrDef::new("notes", ValueType::Str, Kind::State, Cardinality::Many).unindexed(),
        )
}
fn clean(w: &World) {
    let rep = doctor(w, &DoctorOptions::default());
    assert!(rep.is_clean(), "{:#?}", rep.findings);
}
fn file(dir: &tempfile::TempDir) -> (World, PathBuf, EntityId, EntityId) {
    let p = dir.path().join("w.kala");
    let mut w = World::create(&p, schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    (w, p, a, b)
}
fn mem() -> (World, EntityId, EntityId) {
    let mut w = World::in_memory(schema()).unwrap();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    (w, a, b)
}

fn put_body(bytes: &[u8], txn_idx: usize, body: &[u8]) -> Vec<u8> {
    let sp = common::spans(bytes)[txn_idx];
    let mut out = bytes[..sp.start].to_vec();
    out.extend_from_slice(&common::txn(sp.tx, body));
    out.extend_from_slice(&bytes[sp.end..]);
    out
}

/// Same helper as tests/unindexed.rs: stretch the unindexed value of the Fact
/// that starts txn `txn_idx` so the Fact payload is exactly `payload_len`.
fn stretch_fact(bytes: &[u8], txn_idx: usize, payload_len: usize) -> Vec<u8> {
    let sp = common::spans(bytes)[txn_idx];
    let frames = common::frame_spans(bytes, sp);
    let (fs, fe, kind) = frames[0];
    assert_eq!(kind, 4);
    let (cs, _ce, ckind) = *frames.last().unwrap();
    assert_eq!(ckind, 5);
    let payload = &bytes[fs + 4..fe];
    let run = payload.windows(4).position(|w| w == b"xxxx").unwrap();
    let mut np = payload[..run].to_vec();
    np.extend(std::iter::repeat_n(b'x', payload_len - payload.len()));
    np.extend_from_slice(&payload[run..]);
    assert_eq!(np.len(), payload_len);
    let mut prior = (np.len() as u32).to_le_bytes().to_vec();
    prior.extend_from_slice(&np);
    prior.extend_from_slice(&bytes[fe..cs]);
    let commit_id = u64::from_le_bytes(bytes[cs + 5..cs + 13].try_into().unwrap());
    let commit = common::commit_frame(&prior, commit_id, sp.tx);
    let mut body = prior;
    body.extend_from_slice(&commit);
    put_body(bytes, txn_idx, &body)
}

/// A file world whose `b.bio @1` Fact frame is exactly MAX_ENTRY_BYTES.
/// Returns (path, the crafted entry's id).
fn crafted(dir: &tempfile::TempDir) -> (PathBuf, EntryId) {
    let (mut w, p, a, b) = file(dir);
    w.assert(a, "bio", s(&"x".repeat(10)), t(0)).unwrap();
    w.assert(b, "bio", s(&"x".repeat(11)), t(1)).unwrap();
    w.assert(a, "notes", s(&"x".repeat(12)), t(2)).unwrap();
    drop(w);
    let bytes = std::fs::read(&p).unwrap();
    let n = common::spans(&bytes).len();
    let out = stretch_fact(&bytes, n - 2, MAX_ENTRY_BYTES);
    std::fs::write(&p, out).unwrap();
    let id = read_log(&p)
        .unwrap()
        .iter()
        .find(|e| matches!(&e.body, EntryBody::Fact { value: Value::Str(v), tick, .. } if v.len() > 1_000_000 && *tick == Tick(1)))
        .expect("the crafted fact")
        .id;
    (p, id)
}

fn refused_untouched(p: &PathBuf, w: &World, id: EntryId, c: Correction) -> Error {
    let before = std::fs::read(p).unwrap();
    let tx = w.latest_tx();
    let _ = (id, &c, tx, before);
    unreachable!()
}

/// Opens the crafted world, tries `c` on the crafted entry, and checks the
/// refusal left no trace: file bytes, latest tx, answers, doctor, writability.
fn try_correct(c: Correction) -> Error {
    let dir = tempfile::tempdir().unwrap();
    let (p, id) = crafted(&dir);
    let mut w = World::open(&p).unwrap();
    let before = std::fs::read(&p).unwrap();
    let tx = w.latest_tx();
    let b = w.entity_by_handle("b").unwrap();
    let held = w.as_of(b, "bio", Tick(1)).unwrap();
    let err = w.correct(id, c).unwrap_err();
    assert_eq!(std::fs::read(&p).unwrap(), before, "nothing written");
    assert_eq!(w.latest_tx(), tx);
    assert_eq!(w.as_of(b, "bio", Tick(1)).unwrap(), held, "still readable");
    clean(&w);
    // the world keeps working after the refusal
    let a = w.entity_by_handle("a").unwrap();
    assert_eq!(w.assert(a, "notes", s("fine"), t(9)).unwrap().changed, 1);
    drop(w);
    let w = World::open(&p).unwrap();
    assert_eq!(w.as_of(b, "bio", Tick(1)).unwrap(), held);
    clean(&w);
    let _ = refused_untouched;
    err
}

#[test]
fn replacing_a_crafted_max_size_value_is_entry_too_large_and_writes_nothing() {
    // the value is kept (tick moves / no-op replace): the Revised carries it
    for c in [
        Correction::Replace {
            value: None,
            tick: t(5),
            op: None,
            precision: None,
        },
        Correction::Replace {
            value: None,
            tick: None,
            op: None,
            precision: None,
        },
    ] {
        let err = try_correct(c.clone());
        assert!(
            matches!(&err, Error::EntryTooLarge { attr, len, max }
                if attr == "bio" && *len > *max && *max == MAX_ENTRY_BYTES),
            "{c:?}: {err:?}"
        );
    }
}

#[test]
fn deleting_a_crafted_max_size_value_is_refused_with_entry_too_large_and_writes_nothing() {
    // By the "judged against a Revised entry" rule the value can never be
    // written in a correction entry, so a delete is refused too, and (docs:
    // "an entry whose payload would exceed MAX_ENTRY_BYTES is refused
    // before anything is written: Err(EntryTooLarge)") with that error.
    let err = try_correct(Correction::Delete);
    assert!(matches!(err, Error::EntryTooLarge { .. }), "{err:?}");
}

#[test]
fn replacing_a_crafted_max_size_value_with_a_small_one_is_refused_with_entry_too_large() {
    // the DELETED marker for the old key carries the old (huge) value
    let err = try_correct(Correction::Replace {
        value: Some(s("small")),
        tick: None,
        op: None,
        precision: None,
    });
    assert!(matches!(err, Error::EntryTooLarge { .. }), "{err:?}");
}

#[test]
fn a_refused_correction_of_a_crafted_max_size_value_inside_a_txn_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (p, id) = crafted(&dir);
    let mut w = World::open(&p).unwrap();
    let before = std::fs::read(&p).unwrap();
    let tx = w.latest_tx();
    let a = w.entity_by_handle("a").unwrap();
    let mut txn = w.begin();
    txn.assert(a, "notes", s("staged"), t(7)).unwrap();
    // refused either when staged or at commit, never half-applied
    let staged = txn.correct(id, Correction::Delete);
    let outcome = if staged.is_err() {
        Err(())
    } else {
        txn.commit().map(|_| ()).map_err(|_| ())
    };
    assert!(outcome.is_err(), "the transaction must not commit");
    assert_eq!(std::fs::read(&p).unwrap(), before);
    assert_eq!(w.latest_tx(), tx);
    assert!(!w.as_of(a, "notes", Tick(7)).unwrap().contains(&s("staged")));
    clean(&w);
    // the world still writes
    assert_eq!(w.assert(a, "notes", s("later"), t(8)).unwrap().changed, 1);
}

#[test]
fn as_of_value_of_a_never_stored_unindexed_value_is_false_at_every_canon() {
    for collide in [false, true] {
        let (mut w, a, b) = mem();
        if collide {
            w.set_content_hash(|_| 0);
        }
        // empty table: nothing stored yet
        assert!(!w.as_of_value(a, "bio", &s("ghost"), Tick(0)).unwrap());
        w.assert(a, "bio", s("real"), t(0)).unwrap();
        let tx1 = w.latest_tx();
        w.assert(b, "notes", s("other"), t(0)).unwrap();
        for v in ["ghost", "", "rea", "reall", "REAL"] {
            for at in [i64::MIN, -1, 0, 1, i64::MAX] {
                assert!(
                    !w.as_of_value(a, "bio", &s(v), Tick(at)).unwrap(),
                    "{v:?}@{at}"
                );
                assert!(!w.as_of_value(b, "notes", &s(v), Tick(at)).unwrap());
                for canon in [TxId(0), tx1, w.latest_tx()] {
                    assert!(
                        !w.at_canon(canon)
                            .as_of_value(a, "bio", &s(v), Tick(at))
                            .unwrap(),
                        "{v:?}@{at} canon {canon:?}"
                    );
                    assert!(
                        !w.at_canon_main(canon)
                            .as_of_value(a, "bio", &s(v), Tick(at))
                            .unwrap()
                    );
                }
            }
        }
        // a stored value of another entity's attribute is not this entity's
        assert!(!w.as_of_value(b, "bio", &s("real"), Tick(0)).unwrap());
        assert!(!w.as_of_value(a, "notes", &s("real"), Tick(0)).unwrap());
        // the real one holds (and not before the first canon that has it)
        assert!(w.as_of_value(a, "bio", &s("real"), Tick(0)).unwrap());
        assert!(
            !w.at_canon(TxId(0))
                .as_of_value(a, "bio", &s("real"), Tick(0))
                .unwrap()
        );
        clean(&w);
    }
}

fn answers(w: &World, a: EntityId, b: EntityId) -> Vec<String> {
    let mut out = Vec::new();
    for e in [a, b] {
        for attr in ["bio", "notes"] {
            out.push(format!("{attr} hist {:?}", w.history(e, attr).unwrap()));
            for at in [-1, 0, 1, 2, 3, 100] {
                out.push(format!(
                    "{attr}@{at} {:?}",
                    w.as_of(e, attr, Tick(at)).unwrap()
                ));
            }
            for v in ["v1", "v2", "v3"] {
                out.push(format!(
                    "{attr}={v} {}",
                    w.as_of_value(e, attr, &s(v), Tick(100)).unwrap()
                ));
            }
        }
    }
    out.push(format!("{:?}", w.overlaps("bio").unwrap()));
    out.push(format!("{:?}", w.index_dump().e));
    out
}

fn v(i: u8) -> Value {
    s(&format!("v{i}"))
}

#[test]
fn a_dropped_or_poisoned_txn_that_staged_colliding_values_leaves_no_trace_in_the_table() {
    for poison in [false, true] {
        // world X: stage v1 and v2 (same hash), drop, then commit only v2
        let (mut x, xa, xb) = mem();
        x.set_content_hash(|_| 0);
        {
            let mut txn = x.begin();
            txn.assert(xa, "notes", v(1), t(1)).unwrap();
            txn.assert(xb, "notes", v(2), t(1)).unwrap();
            if poison {
                assert!(txn.assert(xa, "nope", v(3), t(1)).is_err());
                assert!(txn.commit().is_err());
            } // else dropped here
        }
        let tx0 = x.latest_tx();
        x.assert(xb, "notes", v(2), t(2)).unwrap();
        assert_eq!(x.latest_tx().0, tx0.0 + 1);
        // world Y: only the committing write
        let (mut y, ya, yb) = mem();
        y.set_content_hash(|_| 0);
        y.assert(yb, "notes", v(2), t(2)).unwrap();
        assert_eq!(answers(&x, xa, xb), answers(&y, ya, yb), "poison={poison}");
        clean(&x);
        clean(&y);
        // v1 now gets the next reference; everything stays consistent
        x.assert(xa, "notes", v(1), t(3)).unwrap();
        y.assert(ya, "notes", v(1), t(3)).unwrap();
        assert_eq!(answers(&x, xa, xb), answers(&y, ya, yb));
        clean(&x);
        // retract v2 and re-assert v1: still consistent after a rebuild with the hook
        x.retract(xb, "notes", v(2), t(4)).unwrap();
        x.set_content_hash(|_| 0);
        clean(&x);
    }
}

#[test]
fn the_same_script_on_a_file_world_reopens_to_the_same_answers_as_a_fresh_world() {
    let dir = tempfile::tempdir().unwrap();
    let (mut w, p, a, b) = file(&dir);
    w.set_content_hash(|_| 0);
    {
        let mut txn = w.begin();
        txn.assert(a, "notes", v(1), t(1)).unwrap();
        txn.assert(b, "notes", v(2), t(1)).unwrap();
    } // dropped
    w.assert(b, "notes", v(2), t(2)).unwrap();
    w.assert(a, "notes", v(1), t(3)).unwrap();
    clean(&w);
    let live = answers(&w, a, b);
    drop(w);
    // reopen uses the real hash: same answers, doctor clean
    let w = World::open(&p).unwrap();
    assert_eq!(answers(&w, a, b), live);
    clean(&w);
    // and a fresh file world that only ran the committed writes agrees
    let dir2 = tempfile::tempdir().unwrap();
    let (mut f, _, fa, fb) = file(&dir2);
    f.assert(fb, "notes", v(2), t(2)).unwrap();
    f.assert(fa, "notes", v(1), t(3)).unwrap();
    assert_eq!(answers(&f, fa, fb), live);
}

fn e_key(entity: EntityId, attr: AttrId, part: &[u8]) -> Vec<u8> {
    let mut k = vec![b'E'];
    k.extend_from_slice(&entity.to_bytes());
    k.extend_from_slice(&attr.0.to_be_bytes());
    k.extend_from_slice(part);
    k.extend_from_slice(&Tick(5).encode_inverted());
    k.extend_from_slice(&(!1u64).to_be_bytes());
    k
}

fn attr_of(w: &World, with: &str) -> AttrId {
    w.index_dump()
        .e
        .iter()
        .find(|r| matches!(&r.2, Value::Str(x) if x == with))
        .unwrap()
        .1
}

#[test]
fn a_value_reference_on_an_indexed_attribute_and_a_plain_value_on_an_unindexed_one_are_index_decode()
 {
    let mut reference = vec![0x06u8];
    reference.extend_from_slice(&7u64.to_be_bytes());
    reference.extend_from_slice(&0u32.to_be_bytes());
    for index in [IndexName::E, IndexName::ELatest] {
        let (mut w, a, b) = mem();
        w.assert(a, "name", s("Edwin"), t(0)).unwrap();
        w.assert(b, "bio", s("a bio"), t(0)).unwrap();
        clean(&w);
        let name = attr_of(&w, "Edwin");
        let bio = attr_of(&w, "a bio");
        let plain = s("a plain value").encode().unwrap();
        let mut keys = vec![
            // indexed attribute carrying a reference
            (e_key(a, name, &reference), "ref on indexed"),
            // unindexed attribute carrying a plain encoding
            (e_key(b, bio, &plain), "plain on unindexed"),
        ];
        // latest-index keys have no ^tx
        if index == IndexName::ELatest {
            for (k, _) in keys.iter_mut() {
                k.truncate(k.len() - 8);
            }
        }
        for (key, what) in keys {
            let mut w2 = {
                let (mut w2, a2, b2) = mem();
                w2.assert(a2, "name", s("Edwin"), t(0)).unwrap();
                w2.assert(b2, "bio", s("a bio"), t(0)).unwrap();
                w2
            };
            w2.tamper_index(Tamper::InsertRawKey {
                index,
                key: key.clone(),
            });
            let _ = w2.index_dump();
            let rep = doctor(&w2, &DoctorOptions::default());
            assert!(
                rep.findings.iter().any(|f| matches!(f,
                    Finding::IndexDecode { index: i, key: k } if *i == index && *k == key)),
                "{index:?} {what}: {:#?}",
                rep.findings
            );
        }
        let _ = &mut w;
    }
}

#[test]
fn a_reference_nobody_stored_in_an_unindexed_attribute_is_index_decode_too() {
    let (mut w, a, _) = mem();
    w.assert(a, "bio", s("a bio"), t(0)).unwrap();
    let bio = attr_of(&w, "a bio");
    let mut reference = vec![0x06u8];
    reference.extend_from_slice(&u64::MAX.to_be_bytes());
    reference.extend_from_slice(&u32::MAX.to_be_bytes());
    let key = e_key(a, bio, &reference);
    w.tamper_index(Tamper::InsertRawKey {
        index: IndexName::E,
        key: key.clone(),
    });
    let _ = w.index_dump();
    let _ = w.history(a, "bio");
    let rep = doctor(&w, &DoctorOptions::default());
    assert!(
        rep.findings.iter().any(|f| matches!(f,
            Finding::IndexDecode { index: IndexName::E, key: k } if *k == key)),
        "{:#?}",
        rep.findings
    );
}

fn since(n: u64) -> DoctorOptions {
    DoctorOptions {
        canon: None,
        since: Some(TxId(n)),
    }
}

fn mismatch_on(rep: &Report, index: IndexName) -> bool {
    rep.findings
        .iter()
        .any(|f| matches!(f, Finding::IndexMismatch { index: i, .. } if *i == index))
}

#[test]
fn doctor_since_judges_e_latest_rows_of_unindexed_attributes_by_their_entrys_tx() {
    for collide in [false, true] {
        for flip in [true, false] {
            let (mut w, a, b) = mem();
            if collide {
                w.set_content_hash(|_| 0);
            }
            w.assert(a, "bio", s(&"first ".repeat(100)), t(0)).unwrap();
            let t1 = w.latest_tx().0;
            w.assert(b, "bio", s(&"second ".repeat(100)), t(1)).unwrap();
            let t2 = w.latest_tx().0;
            assert!(t2 > t1);
            clean(&w);
            // E_latest rows in key order: a's bio (nth 0, tx t1), b's bio (nth 1, tx t2)
            for (nth, tx) in [(0usize, t1), (1usize, t2)] {
                let mut w2 = {
                    let (mut w2, a2, b2) = mem();
                    if collide {
                        w2.set_content_hash(|_| 0);
                    }
                    w2.assert(a2, "bio", s(&"first ".repeat(100)), t(0))
                        .unwrap();
                    w2.assert(b2, "bio", s(&"second ".repeat(100)), t(1))
                        .unwrap();
                    w2
                };
                let index = IndexName::ELatest;
                w2.tamper_index(if flip {
                    Tamper::FlipOp { index, nth }
                } else {
                    Tamper::RemoveRow { index, nth }
                });
                assert!(mismatch_on(&doctor(&w2, &DoctorOptions::default()), index));
                assert!(
                    mismatch_on(&doctor(&w2, &since(tx - 1)), index),
                    "collide={collide} flip={flip} nth={nth}: since tx-1 sees it"
                );
                assert!(
                    !mismatch_on(&doctor(&w2, &since(tx)), index),
                    "collide={collide} flip={flip} nth={nth}: since tx does not"
                );
            }
            let _ = (a, b);
        }
    }
}

#[test]
fn reads_never_panic_after_an_unresolvable_value_reference_is_planted_in_e() {
    // Same tampering as above, then the public reads. Precedent: garbage E
    // keys are skipped by reads (doctor_integrity), never a panic.
    let mut reference = vec![0x06u8];
    reference.extend_from_slice(&7u64.to_be_bytes());
    reference.extend_from_slice(&0u32.to_be_bytes());
    let (mut w, a, b) = mem();
    w.assert(a, "name", s("Edwin"), t(0)).unwrap();
    w.assert(b, "bio", s("a bio"), t(0)).unwrap();
    let name = attr_of(&w, "Edwin");
    let bio = attr_of(&w, "a bio");
    let plain = s("plain").encode().unwrap();
    for (e, at, part) in [(a, name, &reference), (b, bio, &plain)] {
        for index in [IndexName::E, IndexName::ELatest] {
            let mut key = e_key(e, at, part);
            if index == IndexName::ELatest {
                key.truncate(key.len() - 8);
            }
            let mut w2 = {
                let (mut w2, a2, b2) = mem();
                w2.assert(a2, "name", s("Edwin"), t(0)).unwrap();
                w2.assert(b2, "bio", s("a bio"), t(0)).unwrap();
                w2
            };
            w2.tamper_index(Tamper::InsertRawKey { index, key });
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = w2.as_of(e, "name", Tick(5));
                let _ = w2.as_of(e, "bio", Tick(5));
                let _ = w2.history(e, "name");
                let _ = w2.history(e, "bio");
                let _ = w2.index_dump();
            }));
            assert!(r.is_ok(), "{index:?} entity {e:?}: a read panicked");
        }
    }
}
