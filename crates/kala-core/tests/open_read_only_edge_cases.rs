//! Edge cases of `World::open_read_only`, `Error::ReadOnly` and `View::edge`
//! (CONFIRM step for the read-only open).
mod common;

use kala_core::log::EntryBody;
use kala_core::*;
use kala_fixture::load_fixture_to;
use std::path::{Path, PathBuf};

const VARROW: &str = include_str!("fixtures/varrow.yaml");

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "knows",
            ValueType::Ref,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "located_in",
            ValueType::Ref,
            Kind::State,
            Cardinality::One,
        ))
}

fn new_world() -> (tempfile::TempDir, PathBuf, World) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    let w = World::create(&p, schema()).unwrap();
    (dir, p, w)
}

fn copy_to(src: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    std::fs::copy(src, &p).unwrap();
    (dir, p)
}

#[test]
fn an_edge_whose_only_assert_was_deleted_stays_visible_from_its_defining_tx() {
    let (_d, p, mut w) = new_world();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "knows", Value::Ref(b), Some(Tick(5))).unwrap();
    let define_tx = w.latest_tx();
    let fact = w
        .entries()
        .find(|e| matches!(e.body, EntryBody::Fact { .. }))
        .unwrap()
        .id;
    w.correct(fact, Correction::Delete).unwrap();
    let id = w
        .edge_id(a, "knows", b)
        .expect("DefineEdge stays in the log");
    drop(w);

    let ro = World::open_read_only(&p).unwrap();
    let want = Some((a, "knows".to_string(), b));
    // the correction removed the fact, not the edge's identity
    assert!(!ro.as_of_value(a, "knows", &Value::Ref(b), Tick(5)).unwrap());
    for canon in [define_tx, ro.latest_tx()] {
        let v = ro.at_canon(canon);
        assert_eq!(v.edge(id), want, "canon {canon:?}");
        assert_eq!(v.edge_id(a, "knows", b), Some(id), "canon {canon:?}");
    }
    assert_eq!(ro.at_canon(TxId(define_tx.0 - 1)).edge(id), None);
    assert_eq!(ro.edge(id), want);
}

#[test]
fn a_zero_filled_tail_longer_than_the_valid_part_is_ignored_and_untouched() {
    let (_d, p, w) = new_world();
    drop(w);
    let mut b = bytes(&p);
    let valid = b.len();
    b.extend(std::iter::repeat_n(0u8, valid * 4 + 4096));
    std::fs::write(&p, &b).unwrap();

    let (_d2, copy) = copy_to(&p);
    let expect = World::open(&copy).unwrap();
    let ro = World::open_read_only(&p).unwrap();
    assert_eq!(ro.latest_tx(), expect.latest_tx());
    assert_eq!(ro.latest_tick(), expect.latest_tick());
    assert_eq!(ro.recovery().bytes_cut, expect.recovery().bytes_cut);
    assert_eq!(ro.recovery().cause, expect.recovery().cause);
    assert_eq!(ro.recovery().quarantine, None);
    assert_eq!(ro.recovery().bytes_cut, (b.len() - valid) as u64);
    drop(ro);
    assert_eq!(bytes(&p), b, "file untouched");
}

#[test]
fn set_default_tick_then_commit_on_a_read_only_world_is_refused_and_appends_nothing() {
    let (_d, p) = {
        let (d, p, w) = new_world();
        drop(w);
        (d, p)
    };
    let before = bytes(&p);
    let mut ro = World::open_read_only(&p).unwrap();
    let tx = ro.latest_tx();
    let mut txn = ro.begin();
    txn.set_default_tick(Tick(-208));
    assert_eq!(txn.commit().unwrap_err(), Error::ReadOnly);
    assert_eq!(ro.latest_tx(), tx);
    drop(ro);
    assert_eq!(bytes(&p), before);
}

#[test]
fn doctor_on_a_read_only_clean_world_is_clean_and_equals_doctor_of_open() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("w.kala");
    drop(load_fixture_to(VARROW, &p).unwrap());
    let (_d, copy) = copy_to(&p);
    let opened = World::open(&copy).unwrap();
    let ro = World::open_read_only(&p).unwrap();
    let a = doctor(&ro, &DoctorOptions::default());
    let b = doctor(&opened, &DoctorOptions::default());
    assert_eq!(a, b);
    assert!(a.errors().is_empty(), "{:?}", a.findings);
}

#[test]
fn doctor_on_a_read_only_world_reports_the_same_schema_warnings_as_open() {
    let (_d, p, mut w) = new_world();
    let g = w.create_entity("CHARACTER", Some("greg")).unwrap();
    let v = w.create_entity("PLACE", Some("varrow")).unwrap();
    let s = w.create_entity("PLACE", Some("ashford")).unwrap();
    w.assert(g, "located_in", Value::Ref(v), Some(Tick(200)))
        .unwrap();
    w.assert(g, "located_in", Value::Ref(s), Some(Tick(601)))
        .unwrap();
    drop(w);

    let (_d2, copy) = copy_to(&p);
    let opened = World::open(&copy).unwrap();
    let ro = World::open_read_only(&p).unwrap();
    let a = doctor(&ro, &DoctorOptions::default());
    let b = doctor(&opened, &DoctorOptions::default());
    assert!(!a.findings.is_empty(), "the overlap is a warning");
    assert!(a.errors().is_empty(), "{:?}", a.findings);
    assert_eq!(a, b);
}

#[test]
fn a_writer_appending_between_two_read_only_opens_shows_only_in_the_second() {
    let (_d, p, mut w) = new_world();
    let a = w.create_entity("CHARACTER", Some("a")).unwrap();
    let b = w.create_entity("CHARACTER", Some("b")).unwrap();
    w.assert(a, "knows", Value::Ref(b), Some(Tick(1))).unwrap();

    let first = World::open_read_only(&p).unwrap();
    let tx1 = first.latest_tx();
    w.retract(a, "knows", Value::Ref(b), Some(Tick(7))).unwrap();
    let second = World::open_read_only(&p).unwrap();

    assert_eq!(first.latest_tx(), tx1, "first is unchanged");
    assert_eq!(first.latest_tick(), Tick(1));
    assert!(
        first
            .as_of_value(a, "knows", &Value::Ref(b), Tick(9))
            .unwrap()
    );
    assert!(second.latest_tx() > tx1);
    assert_eq!(second.latest_tick(), Tick(7));
    assert!(
        !second
            .as_of_value(a, "knows", &Value::Ref(b), Tick(9))
            .unwrap()
    );
    assert_eq!(second.latest_tx(), w.latest_tx());
}
