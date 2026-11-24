//! Polish of slice 15b (PR #20): what an independent code review and a
//! security review found in handles sharing one log file.

mod common;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use kala_core::doctor::{DoctorOptions, doctor};
use kala_core::*;

fn schema() -> Schema {
    Schema::new()
        .node_type("CHARACTER")
        .node_type("PLACE")
        .attribute(AttrDef::new(
            "tag",
            ValueType::Str,
            Kind::State,
            Cardinality::Many,
        ))
        .attribute(AttrDef::new(
            "age",
            ValueType::I64,
            Kind::State,
            Cardinality::One,
        ))
}

/// A new file-backed world with one entity `greg` (tx 1).
fn world() -> (tempfile::TempDir, PathBuf, World, EntityId) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let mut w = World::create(&path, schema()).unwrap();
    let greg = w.create_entity("CHARACTER", Some("greg")).unwrap();
    (dir, path, w, greg)
}

fn s(v: &str) -> Value {
    Value::Str(v.into())
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn assert_clean(w: &World) {
    let report = doctor(w, &DoctorOptions::default());
    assert!(report.is_clean(), "{report:?}");
}

/// Everything two handles on one log must agree on, as text.
fn state(w: &World, e: EntityId) -> String {
    let mut out = format!("tx={:?} tick={:?}\n", w.latest_tx(), w.latest_tick());
    for attr in ["tag", "age"] {
        for t in [0, 1, 5, 100] {
            out += &format!("{attr}@{t}={:?}\n", w.as_of(e, attr, Tick(t)));
        }
        out += &format!("{attr} history={:?}\n", w.history(e, attr));
    }
    let mut id = 1;
    while let Some(entry) = w.entry(EntryId(id)) {
        out += &format!("{entry:?}\n");
        id += 1;
    }
    out
}

/// Holds the log file's writer lock the way another process would.
#[allow(dead_code)]
fn hold_lock(path: &Path) -> File {
    let f = OpenOptions::new().read(true).open(path).unwrap();
    f.try_lock().expect("the lock is free");
    f
}

fn quarantine_files(path: &Path) -> Vec<PathBuf> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut found: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{name}.torn-"))
        })
        .collect();
    found.sort();
    found
}

fn to_str() -> MigrationOptions {
    MigrationOptions {
        value_type: Some(ValueType::Str),
        ..MigrationOptions::default()
    }
}

/// Damages the body of the file's last transaction (all its bytes stay).
fn damage_last_txn(path: &Path) -> common::Span {
    let mut b = bytes(path);
    let last = *common::spans(&b).last().unwrap();
    b[last.body_start + 6] ^= 0xFF;
    std::fs::write(path, &b).unwrap();
    last
}

// ---- A: replaying a migration is not a write ----

#[test]
fn a_read_only_handle_catches_up_over_a_migration() {
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "age", Value::I64(40), Some(Tick(1)))
        .unwrap();
    let mut r = World::open_read_only(&path).unwrap();
    let mut tx = w.begin();
    tx.migrate_attribute("age", to_str()).unwrap();
    tx.commit().unwrap();
    w.assert(greg, "tag", s("after"), Some(Tick(5))).unwrap();

    assert_eq!(r.catch_up().unwrap(), 2);
    let fresh = World::open_read_only(&path).unwrap();
    assert_eq!(state(&r, greg), state(&fresh, greg));
    assert_eq!(r.as_of(greg, "age", Tick(1)).unwrap(), vec![s("40")]);
    // Still following the file, still read-only.
    assert_eq!(r.catch_up().unwrap(), 0);
    assert!(matches!(
        r.assert(greg, "tag", s("x"), Some(Tick(1))),
        Err(Error::ReadOnly)
    ));
    assert_clean(&r);
}

#[test]
fn a_writer_with_an_unacknowledged_recovery_catches_up_over_a_migration() {
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "age", Value::I64(40), Some(Tick(1)))
        .unwrap();
    w.assert(greg, "tag", s("doomed"), Some(Tick(1))).unwrap();
    drop(w);
    damage_last_txn(&path);
    // `u` refuses writes until its damaged tail is acknowledged.
    let mut u = World::open(&path).unwrap();
    assert_eq!(u.recovery().cause, Some(CutCause::Damaged));
    assert!(matches!(
        u.assert(greg, "tag", s("x"), Some(Tick(1))),
        Err(Error::RecoveryUnacknowledged)
    ));
    // Another handle acknowledges, cuts and commits a migration.
    let mut other = World::open(&path).unwrap();
    other.acknowledge_recovery();
    let mut tx = other.begin();
    tx.migrate_attribute("age", to_str()).unwrap();
    tx.commit().unwrap();

    assert_eq!(u.catch_up().unwrap(), 1);
    let fresh = World::open_read_only(&path).unwrap();
    assert_eq!(state(&u, greg), state(&fresh, greg));
    assert_eq!(u.as_of(greg, "age", Tick(1)).unwrap(), vec![s("40")]);
    assert!(matches!(
        u.assert(greg, "tag", s("x"), Some(Tick(1))),
        Err(Error::RecoveryUnacknowledged)
    ));
}

// ---- L: doctor on a handle doesn't judge another handle's append in
// progress ----

#[test]
fn doctor_on_a_handle_ignores_an_incomplete_tail_past_what_it_applied() {
    use kala_core::doctor::{Finding, doctor_log};
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "tag", s("a"), Some(Tick(1))).unwrap();
    let clean = bytes(&path);
    // Another handle commits; its transaction, cut short, is what a commit
    // in progress looks like to anyone reading the file meanwhile.
    let mut other = World::open(&path).unwrap();
    other.assert(greg, "tag", s("b"), Some(Tick(2))).unwrap();
    drop(other);
    let full = bytes(&path);
    let is_tail = |f: &Finding| matches!(f, Finding::IncompleteTail { .. });
    for (what, cut) in [
        ("torn", 5usize),
        ("begin only", full.len() - clean.len() - 10),
    ] {
        std::fs::write(&path, &full[..full.len() - cut]).unwrap();
        let on_handle = doctor(&w, &DoctorOptions::default());
        assert!(on_handle.is_clean(), "{what}: {on_handle:?}");
        // The file as a whole still reports it.
        let on_file = doctor_log(&path, &DoctorOptions::default()).unwrap();
        assert!(on_file.findings.iter().any(is_tail), "{what}: {on_file:?}");
    }
    // All there, checksum wrong (an append whose pages aren't all in yet).
    let mut damaged = full.clone();
    let last = *common::spans(&full).last().unwrap();
    damaged[last.body_start + 6] ^= 0xFF;
    std::fs::write(&path, &damaged).unwrap();
    assert_clean(&w);
    // A handle that has applied the transaction is told when it goes bad.
    std::fs::write(&path, &full).unwrap();
    assert_eq!(w.catch_up().unwrap(), 1);
    assert_clean(&w);
    std::fs::write(&path, &full[..full.len() - 5]).unwrap();
    let report = doctor(&w, &DoctorOptions::default());
    assert!(report.findings.iter().any(is_tail), "{report:?}");
}
