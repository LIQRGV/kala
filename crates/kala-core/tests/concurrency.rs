//! Transactions sharing a file serialize their commits, not their handles
//! (slice 15b, stage 2): the writer lock is taken per commit with a bounded
//! wait, an open handle catches up with what other handles appended, and
//! recovery cuts happen only under the lock.

mod common;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Span, spans};
use kala_core::doctor::{DoctorOptions, doctor, doctor_log};
use kala_core::log::LogFault;
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
        .attribute(
            AttrDef::new("located_in", ValueType::Ref, Kind::State, Cardinality::One).directed(),
        )
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
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

fn tag(s: &str) -> Value {
    Value::Str(s.into())
}

fn tags(w: &World, e: EntityId) -> Vec<Value> {
    w.as_of(e, "tag", Tick(i64::MAX)).unwrap()
}

fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn assert_clean(w: &World) {
    let report = doctor(w, &DoctorOptions::default());
    assert!(report.is_clean(), "{report:?}");
}

/// Holds the log file's writer lock the way another process would: an
/// exclusive advisory lock on its own open file. Dropping it releases it.
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

/// Everything a handle can be asked, as one comparable value.
fn state(w: &World) -> (IndexDump, Vec<EntryInfo>, TxId, Tick, Option<log::ChainTip>) {
    (
        w.index_dump(),
        w.entries().collect(),
        w.latest_tx(),
        w.latest_tick(),
        w.chain_tip(),
    )
}

// ------------------------------------------------------------ two handles

#[test]
fn a_second_handle_commits_while_the_first_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = Schema::new().node_type("CHARACTER").attribute(AttrDef::new(
        "tag",
        ValueType::Str,
        Kind::State,
        Cardinality::Many,
    ));
    let mut first = World::create(&path, schema).unwrap();
    let greg = first.create_entity("CHARACTER", Some("greg")).unwrap();
    let opened = World::open(&path);
    assert!(
        opened.is_ok(),
        "an idle writable handle must leave the writer lock available: {opened:?}"
    );
    let mut second = opened.unwrap();

    first
        .assert(greg, "tag", Value::Str("first".into()), Some(Tick(-10)))
        .unwrap();
    second
        .assert(greg, "tag", Value::Str("second".into()), Some(Tick(0)))
        .unwrap();

    let reader = World::open_read_only(&path).unwrap();
    assert_eq!(
        reader.as_of(greg, "tag", Tick(i64::MAX)).unwrap(),
        vec![Value::Str("first".into()), Value::Str("second".into())]
    );
    assert_eq!(reader.latest_tx(), TxId(3));
    assert!(kala_core::doctor::doctor(&reader, &Default::default()).is_clean());
}

#[test]
fn two_handles_commit_in_turn_and_both_see_both_after_catch_up() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();
    for i in 0..5i64 {
        let a = first
            .assert(greg, "tag", tag(&format!("first-{i}")), Some(Tick(i)))
            .unwrap();
        assert_eq!(a.changed, 1);
        let b = second
            .assert(greg, "tag", tag(&format!("second-{i}")), Some(Tick(i)))
            .unwrap();
        assert_eq!(b.changed, 1);
    }
    // `second` wrote last, so it has applied everything; `first` is one
    // transaction behind until it catches up.
    assert_eq!(second.catch_up().unwrap(), 0);
    assert_eq!(first.catch_up().unwrap(), 1);
    assert_eq!(first.catch_up().unwrap(), 0, "nothing new the second time");
    assert_eq!(first.latest_tx(), TxId(11));
    assert_eq!(state(&first), state(&second));
    assert_eq!(tags(&first, greg).len(), 10);
    assert_clean(&first);
    assert_clean(&second);
    let fresh = World::open_read_only(&path).unwrap();
    assert_eq!(state(&fresh), state(&first));
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
}

#[test]
fn doctor_is_clean_on_a_handle_that_has_not_caught_up() {
    let (_d, path, mut first, greg) = world();
    let second = World::open(&path).unwrap();
    first.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    // `second` is behind the file: its indexes match the log it applied.
    assert_eq!(second.latest_tx(), TxId(1));
    assert_clean(&second);
}

// ------------------------------------------------------------ catch-up

/// One of everything a transaction can hold, written through `w`.
fn write_one_of_everything(w: &mut World, greg: EntityId) -> (EntityId, EdgeId, EntryId) {
    let varrow = w.create_entity("PLACE", Some("varrow")).unwrap();
    let mut txn = w.begin();
    let mara = txn.create_entity("CHARACTER", Some("mara")).unwrap();
    txn.assert(mara, "tag", tag("ally"), Some(Tick(-5)))
        .unwrap();
    txn.assert(greg, "located_in", Value::Ref(varrow), Some(Tick(-300)))
        .unwrap();
    txn.declare_node_type("FACTION").unwrap();
    txn.declare_attribute(AttrDef::new(
        "age",
        ValueType::I64,
        Kind::State,
        Cardinality::One,
    ))
    .unwrap();
    txn.world_setting("schema", "strict").unwrap();
    txn.commit().unwrap();
    let edge = w.edge_id(greg, "located_in", varrow).expect("interned");
    // An edge property, a rename, and a correction that moves a tick.
    w.assert(
        edge.into(),
        "disguised",
        Value::Bool(true),
        Some(Tick(-300)),
    )
    .unwrap();
    let mut txn = w.begin();
    txn.rename_attribute("tag", "label").unwrap();
    txn.commit().unwrap();
    let left = w
        .retract(greg, "located_in", Value::Ref(varrow), Some(Tick(-250)))
        .unwrap();
    let entry = left.entries[0];
    w.correct(
        entry,
        Correction::Replace {
            tick: Some(Tick(-280)),
            value: None,
            op: None,
            precision: Some(Precision::Year),
        },
    )
    .unwrap();
    (varrow, edge, entry)
}

#[test]
fn catch_up_applies_exactly_what_the_other_handle_committed() {
    let (_d, path, mut writer, greg) = world();
    let mut follower = World::open(&path).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let before = writer.latest_tx();
    let (varrow, edge, entry) = write_one_of_everything(&mut writer, greg);
    let appended = (writer.latest_tx().0 - before.0) as usize;
    assert!(appended >= 6, "{appended}");

    for (name, handle) in [("writable", &mut follower), ("read-only", &mut reader)] {
        assert_eq!(handle.latest_tx(), before, "{name}: not seen yet");
        assert_eq!(handle.catch_up().unwrap(), appended, "{name}");
        assert_eq!(state(handle), state(&writer), "{name}");
        // IDs, schema, settings and corrections, each by its own read.
        assert_eq!(handle.entity_by_handle("varrow"), Some(varrow), "{name}");
        assert!(handle.entity_by_handle("mara").is_some(), "{name}");
        assert_eq!(
            handle.edge_id(greg, "located_in", varrow),
            Some(edge),
            "{name}"
        );
        assert_eq!(
            handle.as_of(edge.into(), "disguised", Tick(-300)).unwrap(),
            vec![Value::Bool(true)],
            "{name}"
        );
        assert!(handle.node_types().any(|t| t == "FACTION"), "{name}");
        assert!(handle.attribute("age").is_some(), "{name}");
        assert!(handle.attribute("label").is_some(), "{name}: renamed");
        assert!(handle.attribute("tag").is_none(), "{name}: old name gone");
        assert_eq!(handle.world_setting("schema"), Some("strict"), "{name}");
        assert_eq!(
            handle.entry(entry).map(|e| e.id),
            Some(entry),
            "{name}: entry IDs agree"
        );
        // The corrected retract: gone at -270 (moved to -280), by revision.
        assert!(
            handle
                .as_of(greg, "located_in", Tick(-270))
                .unwrap()
                .is_empty(),
            "{name}"
        );
        assert_eq!(
            handle.as_of(greg, "located_in", Tick(-290)).unwrap(),
            vec![Value::Ref(varrow)],
            "{name}"
        );
        assert_clean(handle);
    }
    // The writable follower writes on what it caught up with.
    let res = follower
        .assert(greg, "label", tag("after"), Some(Tick(5)))
        .unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(writer.catch_up().unwrap(), 1);
    assert_eq!(state(&writer), state(&follower));
    assert_clean(&writer);
}

#[test]
fn catch_up_reads_only_the_tail() {
    let (_d, path, mut writer, greg) = world();
    // A long log, then two handles opened on it.
    for i in 0..400i64 {
        writer
            .assert(greg, "tag", tag(&format!("old-{i}")), Some(Tick(i)))
            .unwrap();
    }
    let mut follower = World::open(&path).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let log_len = bytes(&path).len() as u64;
    let tip_len = {
        let tip = follower.chain_tip().unwrap();
        tip.end - tip.offset
    };
    writer
        .assert(greg, "tag", tag("new-1"), Some(Tick(1000)))
        .unwrap();
    writer
        .assert(greg, "tag", tag("new-2"), Some(Tick(1001)))
        .unwrap();
    let tail = bytes(&path).len() as u64 - log_len;
    assert!(log_len > 50 * tail, "the log dwarfs the tail");

    for (name, handle) in [("writable", &mut follower), ("read-only", &mut reader)] {
        let read_before = handle.log_bytes_read();
        assert_eq!(handle.catch_up().unwrap(), 2, "{name}");
        let read = handle.log_bytes_read() - read_before;
        // The header, the handle's last transaction (to prove the prefix is
        // unchanged) and the tail; never the rest of the log.
        assert!(read >= tail, "{name}: read {read} of a {tail}-byte tail");
        assert!(
            read <= tail + tip_len + 12,
            "{name}: read {read} bytes for a {tail}-byte tail after a {tip_len}-byte tip \
             (the log is {log_len} bytes)"
        );
        // With nothing appended: the tip check only.
        let read_before = handle.log_bytes_read();
        assert_eq!(handle.catch_up().unwrap(), 0, "{name}");
        let read = handle.log_bytes_read() - read_before;
        assert!(read <= tip_len + 12 + tail, "{name}: {read}");
    }
}

#[test]
fn a_single_writer_reads_one_transaction_per_commit() {
    let (_d, path, mut writer, greg) = world();
    for i in 0..50i64 {
        writer
            .assert(greg, "tag", tag(&format!("t-{i}")), Some(Tick(i)))
            .unwrap();
    }
    let log_len = bytes(&path).len() as u64;
    let tip = writer.chain_tip().unwrap();
    let read_before = writer.log_bytes_read();
    writer
        .assert(greg, "tag", tag("one more"), Some(Tick(99)))
        .unwrap();
    let read = writer.log_bytes_read() - read_before;
    // Per commit: the header and the previous transaction, to prove under
    // the lock that the log is still the one this handle applied.
    assert!(read > 0, "the commit checked the file");
    assert!(
        read <= (tip.end - tip.offset) + 12,
        "read {read} bytes of a {log_len}-byte log"
    );
}

// ------------------------------------------------------------ stale Txn

#[test]
fn a_txn_begun_before_another_commit_is_stale_and_writes_nothing() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();

    let mut txn = second.begin();
    txn.assert(greg, "tag", tag("mine"), Some(Tick(1))).unwrap();
    // A schema edit too: its provisional state must not leak either.
    txn.declare_node_type("FACTION").unwrap();
    first
        .assert(greg, "tag", tag("theirs"), Some(Tick(2)))
        .unwrap();
    let before = bytes(&path);
    let err = txn.commit().unwrap_err();
    assert_eq!(err, Error::SnapshotStale { applied: 1 });
    assert_eq!(bytes(&path), before, "a stale commit writes nothing");

    // The handle is caught up and holds none of the stale transaction.
    assert_eq!(second.latest_tx(), first.latest_tx());
    assert_eq!(tags(&second, greg), vec![tag("theirs")]);
    assert!(second.node_types().all(|t| t != "FACTION"));
    assert_eq!(state(&second), state(&first));
    assert_clean(&second);

    // A new transaction, begun afterwards, commits.
    let mut txn = second.begin();
    txn.assert(greg, "tag", tag("mine"), Some(Tick(1))).unwrap();
    let res = txn.commit().unwrap();
    assert!(res.committed);
    assert_eq!(res.tx, TxId(3));
    assert_eq!(first.catch_up().unwrap(), 1);
    assert_eq!(tags(&first, greg), vec![tag("mine"), tag("theirs")]);
    assert_clean(&first);
}

#[test]
fn begin_and_auto_commit_start_from_the_latest_committed_state() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();
    first
        .assert(greg, "tag", tag("a"), Some(Tick(600)))
        .unwrap();
    let mara = first.create_entity("CHARACTER", Some("mara")).unwrap();

    // An auto-committed write on a handle that is behind simply lands on
    // the latest state: no stale error, the next tx, the latest tick.
    let res = second.assert(greg, "tag", tag("b"), None).unwrap();
    assert_eq!((res.changed, res.tick), (1, Tick(600)));
    assert_eq!(second.latest_tx(), TxId(4));
    // So does a create (the next sequence number, not a duplicate).
    let third = second.create_entity("CHARACTER", Some("third")).unwrap();
    assert_ne!(third, mara);
    assert_eq!(
        second.create_entity("CHARACTER", Some("mara")).unwrap_err(),
        Error::DuplicateHandle("mara".into())
    );

    // An explicit transaction begins on the latest state too.
    first
        .assert(greg, "tag", tag("c"), Some(Tick(700)))
        .unwrap();
    let mut txn = second.begin();
    let staged = txn.assert(greg, "tag", tag("d"), None).unwrap();
    assert_eq!(staged.tick, Tick(700), "the latest tick at begin");
    assert!(txn.commit().unwrap().committed);
    assert_eq!(first.catch_up().unwrap(), 1);
    assert_eq!(state(&first), state(&second));
    assert_clean(&first);
}

// ------------------------------------------------------------ busy timeout

#[test]
fn a_held_lock_fails_the_commit_after_about_the_busy_timeout() {
    let (_d, path, mut w, greg) = world();
    w.set_busy_timeout(Duration::from_millis(300));
    let held = hold_lock(&path);
    let before = bytes(&path);
    let started = Instant::now();
    let err = w
        .assert(greg, "tag", tag("blocked"), Some(Tick(1)))
        .unwrap_err();
    let waited = started.elapsed();
    assert_eq!(err, Error::WorldLocked);
    assert!(
        waited >= Duration::from_millis(300),
        "gave up early: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "waited far too long: {waited:?}"
    );
    assert_eq!(bytes(&path), before);
    // An explicit transaction waits at its commit the same way.
    let mut txn = w.begin();
    txn.assert(greg, "tag", tag("blocked"), Some(Tick(1)))
        .unwrap();
    assert_eq!(txn.commit().unwrap_err(), Error::WorldLocked);
    assert_eq!(bytes(&path), before);
    drop(held);
    // The handle is not poisoned by having waited.
    assert_eq!(
        w.assert(greg, "tag", tag("free"), Some(Tick(1)))
            .unwrap()
            .changed,
        1
    );
}

#[test]
fn a_zero_busy_timeout_fails_at_once() {
    let (_d, path, mut w, greg) = world();
    w.set_busy_timeout(Duration::ZERO);
    let held = hold_lock(&path);
    let started = Instant::now();
    let err = w
        .assert(greg, "tag", tag("blocked"), Some(Tick(1)))
        .unwrap_err();
    assert_eq!(err, Error::WorldLocked);
    assert!(started.elapsed() < Duration::from_millis(250));
    // Opening needs the lock too, and waits no longer than asked.
    let started = Instant::now();
    assert_eq!(
        World::open_waiting(&path, Duration::ZERO).unwrap_err(),
        Error::WorldLocked
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    let started = Instant::now();
    assert_eq!(
        World::open_waiting(&path, Duration::from_millis(200)).unwrap_err(),
        Error::WorldLocked
    );
    assert!(started.elapsed() >= Duration::from_millis(200));
    drop(held);
}

#[test]
fn a_commit_succeeds_when_the_holder_releases_within_the_timeout() {
    let (_d, path, mut w, greg) = world();
    w.set_busy_timeout(Duration::from_secs(20));
    let held = hold_lock(&path);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        drop(held);
    });
    let started = Instant::now();
    let res = w.assert(greg, "tag", tag("waited"), Some(Tick(1))).unwrap();
    let waited = started.elapsed();
    release.join().unwrap();
    assert_eq!(res.changed, 1);
    assert!(waited >= Duration::from_millis(200), "{waited:?}");
    assert!(waited < Duration::from_secs(10), "{waited:?}");
    assert_clean(&w);
}

#[test]
fn readers_and_catch_up_never_wait_for_the_lock() {
    let (_d, path, mut w, greg) = world();
    let mut other = World::open(&path).unwrap();
    w.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let held = hold_lock(&path);
    let started = Instant::now();
    assert_eq!(other.catch_up().unwrap(), 1);
    let mut reader = World::open_read_only(&path).unwrap();
    assert_eq!(reader.catch_up().unwrap(), 0);
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(held);
}

#[test]
fn with_writer_lock_keeps_commits_out_for_its_length_only() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();
    second.set_busy_timeout(Duration::ZERO);
    let before = bytes(&path);
    let inside = first
        .with_writer_lock(|| second.assert(greg, "tag", tag("x"), Some(Tick(1))))
        .unwrap();
    assert_eq!(inside.unwrap_err(), Error::WorldLocked);
    assert_eq!(bytes(&path), before);
    // Released when the closure returned.
    assert_eq!(
        second
            .assert(greg, "tag", tag("x"), Some(Tick(1)))
            .unwrap()
            .changed,
        1
    );
    // And not taken over another holder's head.
    let held = hold_lock(&path);
    first.set_busy_timeout(Duration::ZERO);
    let mut ran = false;
    assert_eq!(
        first.with_writer_lock(|| ran = true).unwrap_err(),
        Error::WorldLocked
    );
    assert!(!ran);
    drop(held);
}

// ------------------------------------------------------------ recovery

/// The bytes of one more committed transaction on the world at `path`
/// (an assert of `value` on `greg.tag`), built by letting a handle on a
/// copy of the file commit it.
fn next_txn_bytes(path: &Path, greg: EntityId, value: &str, tick: i64) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("copy.kala");
    let before = bytes(path);
    std::fs::write(&copy, &before).unwrap();
    let mut w = World::open(&copy).unwrap();
    w.assert(greg, "tag", tag(value), Some(Tick(tick))).unwrap();
    drop(w);
    bytes(&copy)[before.len()..].to_vec()
}

fn append_raw(path: &Path, more: &[u8]) {
    let mut f = OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(more).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn a_torn_tail_seen_without_the_lock_is_not_yet() {
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let mut follower = World::open(&path).unwrap();
    let txn = next_txn_bytes(&path, greg, "half", 2);

    // Half a transaction: another process is in the middle of its append.
    for cut in [1, 10, 25, 26, txn.len() - 1] {
        let (_d2, p2) = {
            let d = tempfile::tempdir().unwrap();
            let p = d.path().join("w.kala");
            std::fs::write(&p, bytes(&path)).unwrap();
            (d, p)
        };
        let mut r = World::open_read_only(&p2).unwrap();
        let mut f = World::open(&p2).unwrap();
        append_raw(&p2, &txn[..cut]);
        let torn = bytes(&p2);
        let seen = state(&r);
        assert_eq!(r.catch_up().unwrap(), 0, "cut={cut}");
        assert_eq!(f.catch_up().unwrap(), 0, "cut={cut}: writable, no lock");
        assert_eq!(state(&r), seen, "cut={cut}: nothing applied");
        assert_eq!(bytes(&p2), torn, "cut={cut}: file untouched");
        assert!(quarantine_files(&p2).is_empty(), "cut={cut}");
        // The writer finishes its append: now it is there.
        append_raw(&p2, &txn[cut..]);
        assert_eq!(r.catch_up().unwrap(), 1, "cut={cut}");
        assert_eq!(f.catch_up().unwrap(), 1, "cut={cut}");
        assert_eq!(tags(&r, greg), vec![tag("a"), tag("half")], "cut={cut}");
        assert_clean(&r);
        assert_clean(&f);
    }
    // And on the shared file, with a live writer handle around.
    append_raw(&path, &txn[..30]);
    let torn = bytes(&path);
    assert_eq!(reader.catch_up().unwrap(), 0);
    assert_eq!(follower.catch_up().unwrap(), 0);
    assert_eq!(bytes(&path), torn);
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn a_commit_over_a_torn_tail_cuts_it_under_the_lock_then_commits() {
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let clean = bytes(&path);
    let txn = next_txn_bytes(&path, greg, "crashed", 2);
    // Another writer died mid-append.
    append_raw(&path, &txn[..txn.len() - 3]);
    assert_eq!(writer.recovery(), &RecoveryReport::default());

    let res = writer.assert(greg, "tag", tag("b"), Some(Tick(3))).unwrap();
    assert_eq!(res.changed, 1);
    assert_eq!(writer.latest_tx(), TxId(3), "the torn tx#3 never counted");

    // Cut, quarantined and reported, exactly as `World::open` does.
    let q = quarantine_files(&path);
    assert_eq!(q.len(), 1, "{q:?}");
    assert_eq!(bytes(&q[0]), txn[..txn.len() - 3].to_vec());
    let report = writer.recovery().clone();
    assert_eq!(report.bytes_cut, (txn.len() - 3) as u64);
    assert_eq!(report.cause, Some(CutCause::Torn));
    assert_eq!(report.quarantine.as_deref(), Some(q[0].as_path()));
    // The log is the clean prefix plus the new commit, nothing torn left.
    let after = bytes(&path);
    assert_eq!(after[..clean.len()], clean[..]);
    let all: Vec<Span> = spans(&after);
    assert_eq!(all.last().unwrap().tx, 3);
    assert_clean(&writer);
    let fresh = World::open(&path).unwrap();
    assert_eq!(fresh.recovery(), &RecoveryReport::default());
    assert_eq!(tags(&fresh, greg), vec![tag("a"), tag("b")]);
}

#[test]
fn a_damaged_tail_fails_the_commit_until_acknowledged() {
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let mut damaged = next_txn_bytes(&path, greg, "lost", 2);
    // Every byte is there, one is wrong: it may have been acknowledged.
    damaged[30] ^= 0x01;
    append_raw(&path, &damaged);
    let with_damage = bytes(&path);

    // Without the lock nobody may judge it: not yet, nothing touched.
    assert_eq!(writer.catch_up().unwrap(), 0);
    assert_eq!(bytes(&path), with_damage);

    for attempt in 0..2 {
        let err = writer
            .assert(greg, "tag", tag("b"), Some(Tick(3)))
            .unwrap_err();
        assert_eq!(
            err,
            Error::RecoveryRequired {
                bytes: damaged.len() as u64
            },
            "attempt {attempt}"
        );
        assert_eq!(bytes(&path), with_damage, "nothing cut, nothing written");
        assert!(quarantine_files(&path).is_empty());
    }
    let mut txn = writer.begin();
    txn.assert(greg, "tag", tag("b"), Some(Tick(3))).unwrap();
    assert_eq!(
        txn.commit().unwrap_err(),
        Error::RecoveryRequired {
            bytes: damaged.len() as u64
        }
    );
    assert_eq!(bytes(&path), with_damage);

    writer.acknowledge_recovery();
    let res = writer.assert(greg, "tag", tag("b"), Some(Tick(3))).unwrap();
    assert_eq!(res.changed, 1);
    let q = quarantine_files(&path);
    assert_eq!(q.len(), 1);
    assert_eq!(bytes(&q[0]), damaged);
    assert_eq!(writer.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(writer.recovery().bytes_cut, damaged.len() as u64);
    assert_eq!(tags(&writer, greg), vec![tag("a"), tag("b")]);
    assert_clean(&writer);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
}

#[test]
fn acknowledging_in_advance_does_not_license_a_later_cut() {
    let (_d, path, mut writer, greg) = world();
    // Nothing to acknowledge: this must not arm anything.
    writer.acknowledge_recovery();
    let mut damaged = next_txn_bytes(&path, greg, "lost", 2);
    damaged[30] ^= 0x01;
    append_raw(&path, &damaged);
    let with_damage = bytes(&path);
    assert!(matches!(
        writer.assert(greg, "tag", tag("b"), Some(Tick(3))),
        Err(Error::RecoveryRequired { .. })
    ));
    assert_eq!(bytes(&path), with_damage);
}

/// A world at `path` whose last transaction a handle has applied and which
/// is then replaced on disk by another valid transaction of the same
/// length: what a cut (recovery or failed fsync) followed by another
/// writer's commit leaves behind. Returns the handle and the file's bytes.
fn diverged(writable: bool) -> (tempfile::TempDir, PathBuf, World, EntityId, Vec<u8>) {
    let (d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    drop(writer);
    let base = bytes(&path);
    let seen = next_txn_bytes(&path, greg, "seen", 2);
    let other = next_txn_bytes(&path, greg, "else", 2);
    assert_eq!(seen.len(), other.len());
    assert_ne!(seen, other);
    append_raw(&path, &seen);
    let handle = if writable {
        World::open(&path).unwrap()
    } else {
        World::open_read_only(&path).unwrap()
    };
    assert_eq!(tags(&handle, greg), vec![tag("a"), tag("seen")]);
    let rewritten = [base, other].concat();
    std::fs::write(&path, &rewritten).unwrap();
    (d, path, handle, greg, rewritten)
}

#[test]
fn a_rewritten_tip_is_divergence_for_catch_up_and_commit() {
    for writable in [true, false] {
        let (_d, path, mut handle, greg, rewritten) = diverged(writable);
        let seen = state(&handle);
        assert_eq!(handle.catch_up().unwrap_err(), Error::LogDiverged);
        assert_eq!(handle.catch_up().unwrap_err(), Error::LogDiverged, "sticky");
        assert_eq!(state(&handle), seen, "the handle applied nothing");
        let write = handle.assert(greg, "tag", tag("b"), Some(Tick(3)));
        let expected = if writable {
            Error::LogDiverged
        } else {
            Error::ReadOnly
        };
        assert_eq!(write.unwrap_err(), expected);
        assert_eq!(bytes(&path), rewritten, "never written");
        assert!(quarantine_files(&path).is_empty());
    }
}

#[test]
fn a_commit_finds_divergence_by_itself_and_writes_nothing() {
    // No catch_up first: the commit's own check under the lock.
    let (_d, path, mut handle, greg, rewritten) = diverged(true);
    let mut txn = handle.begin();
    txn.assert(greg, "tag", tag("b"), Some(Tick(3))).unwrap();
    assert_eq!(txn.commit().unwrap_err(), Error::LogDiverged);
    assert_eq!(bytes(&path), rewritten);
    // From then on every write and every catch-up is refused.
    assert_eq!(
        handle.create_entity("CHARACTER", Some("mara")).unwrap_err(),
        Error::LogDiverged
    );
    assert_eq!(handle.catch_up().unwrap_err(), Error::LogDiverged);
    assert_eq!(bytes(&path), rewritten);
    // Reads still answer from what the handle had applied.
    assert_eq!(tags(&handle, greg), vec![tag("a"), tag("seen")]);
    // A reopened world reads the file as it is now.
    let fresh = World::open(&path).unwrap();
    assert_eq!(tags(&fresh, greg), vec![tag("a"), tag("else")]);
}

#[test]
fn a_shorter_or_replaced_file_is_divergence() {
    // Cut below the handle's tip (a recovery cut of what it had applied).
    let (_d, path, mut handle, greg, rewritten) = diverged(true);
    let last = *spans(&rewritten).last().unwrap();
    std::fs::write(&path, &rewritten[..last.start]).unwrap();
    assert_eq!(handle.catch_up().unwrap_err(), Error::LogDiverged);
    assert_eq!(
        handle
            .assert(greg, "tag", tag("b"), Some(Tick(3)))
            .unwrap_err(),
        Error::LogDiverged
    );
    assert_eq!(bytes(&path), rewritten[..last.start].to_vec());

    // The path now names another file (a restored copy renamed over it):
    // the handle's own descriptor still sees the old one.
    let (d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let copy = d.path().join("restored.kala");
    std::fs::write(&copy, bytes(&path)).unwrap();
    std::fs::rename(&copy, &path).unwrap();
    let replaced = bytes(&path);
    assert_eq!(
        writer
            .assert(greg, "tag", tag("b"), Some(Tick(3)))
            .unwrap_err(),
        Error::LogDiverged
    );
    assert_eq!(bytes(&path), replaced, "the new file is not written");
}

#[test]
fn a_tail_that_does_not_continue_the_chain_is_corrupt() {
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let before = state(&reader);
    // A valid transaction for this position, but chained from another log:
    // local checksums right, chain wrong.
    let foreign = {
        let (_d2, p2, mut w2, g2) = world();
        w2.assert(g2, "tag", tag("zzz"), Some(Tick(1))).unwrap();
        next_txn_bytes(&p2, g2, "foreign", 2)
    };
    append_raw(&path, &foreign);
    let on_disk = bytes(&path);
    for handle in [&mut reader, &mut writer] {
        let err = handle.catch_up().unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    }
    assert_eq!(state(&reader), before, "nothing applied");
    let err = writer
        .assert(greg, "tag", tag("b"), Some(Tick(3)))
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    assert_eq!(
        bytes(&path),
        on_disk,
        "corruption is never cut or written on"
    );
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn damage_followed_by_a_valid_transaction_is_corrupt() {
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let mut two = {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("copy.kala");
        let base = bytes(&path);
        std::fs::write(&copy, &base).unwrap();
        let mut w = World::open(&copy).unwrap();
        w.assert(greg, "tag", tag("b"), Some(Tick(2))).unwrap();
        w.assert(greg, "tag", tag("c"), Some(Tick(3))).unwrap();
        bytes(&copy)[base.len()..].to_vec()
    };
    two[30] ^= 0x01;
    append_raw(&path, &two);
    let on_disk = bytes(&path);
    for handle in [&mut reader, &mut writer] {
        let err = handle.catch_up().unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    }
    let err = writer
        .assert(greg, "tag", tag("d"), Some(Tick(4)))
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    assert_eq!(bytes(&path), on_disk);
}

#[test]
fn a_tail_no_live_write_could_produce_is_corrupt_and_applies_nothing() {
    // Framing and chain are right, the content is not: a transaction that
    // defines an entity and then repeats a fact that already holds (a
    // no-op no live write stores). The entity definition before it must
    // not stay applied.
    let (_d, path, mut writer, greg) = world();
    writer.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let mut reader = World::open_read_only(&path).unwrap();
    let mut follower = World::open(&path).unwrap();
    let before = state(&reader);

    let base = bytes(&path);
    let good = {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("copy.kala");
        std::fs::write(&copy, &base).unwrap();
        let mut w = World::open(&copy).unwrap();
        let mut txn = w.begin();
        txn.create_entity("CHARACTER", Some("mara")).unwrap();
        txn.assert(greg, "tag", tag("b"), Some(Tick(1))).unwrap();
        txn.commit().unwrap();
        drop(w);
        bytes(&copy)
    };
    // Turn the fact's value "b" into "a" (already held at tick 1) and
    // reseal the framing and the chain, as a writer would have.
    let mut bad = good.clone();
    let last = *spans(&bad).last().unwrap();
    let at = (last.body_start..last.end)
        .rev()
        .find(|&i| bad[i] == b'b')
        .unwrap();
    bad[at] = b'a';
    let last_index = spans(&bad).len() - 1;
    common::rechain(&mut bad, last_index);
    append_raw(&path, &bad[base.len()..]);
    let on_disk = bytes(&path);

    for (name, handle) in [("read-only", &mut reader), ("writable", &mut follower)] {
        let err = handle.catch_up().unwrap_err();
        assert!(matches!(err, Error::Corrupt(_)), "{name}: {err:?}");
        assert_eq!(state(handle), before, "{name}: rolled back whole");
        assert!(handle.entity_by_handle("mara").is_none(), "{name}");
        assert_clean_in_memory(handle);
        // And it stays refused.
        let again = handle.catch_up().unwrap_err();
        assert!(matches!(again, Error::Corrupt(_)), "{name}: {again:?}");
    }
    let err = follower
        .assert(greg, "tag", tag("c"), Some(Tick(5)))
        .unwrap_err();
    assert!(matches!(err, Error::Corrupt(_)), "{err:?}");
    assert_eq!(bytes(&path), on_disk);
}

/// The handle's indexes still match its own kept entries (the file is
/// corrupt, so doctor on the file would say so).
fn assert_clean_in_memory(w: &World) {
    let copy = w.preview_copy().expect("kept entries replay");
    assert_eq!(copy.index_dump(), w.index_dump());
}

// ------------------------------------------------------------ crash safety

#[test]
fn a_panic_under_the_lock_releases_it() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();
    second.set_busy_timeout(Duration::ZERO);
    let before = bytes(&path);
    first.inject_log_fault(LogFault::PanicAfterLock);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = first.assert(greg, "tag", tag("never"), Some(Tick(1)));
    }));
    assert!(panicked.is_err(), "the fault panics inside the commit");
    assert_eq!(bytes(&path), before, "nothing was appended");
    // The lock was released by the unwinding guard: no wait needed.
    assert_eq!(
        second
            .assert(greg, "tag", tag("next"), Some(Tick(2)))
            .unwrap()
            .changed,
        1
    );
    // The handle that panicked is usable too (the fault was one-shot).
    assert_eq!(
        first
            .assert(greg, "tag", tag("again"), Some(Tick(3)))
            .unwrap()
            .changed,
        1
    );
    assert_clean(&first);
}

/// Not a test of its own: the child process of
/// `a_writer_that_dies_holding_the_lock_does_not_block_the_next`. It takes
/// the writer lock for a commit and aborts before appending.
#[test]
fn child_dies_holding_the_lock() {
    let Some(path) = std::env::var_os("KALA_TEST_DIE_HOLDING_LOCK") else {
        return;
    };
    let mut w = World::open(Path::new(&path)).unwrap();
    let greg = w.entity_by_handle("greg").unwrap();
    w.inject_log_fault(LogFault::AbortAfterLock);
    let _ = w.assert(greg, "tag", tag("never"), Some(Tick(1)));
    unreachable!("the fault aborts the process under the lock");
}

#[test]
fn a_writer_that_dies_holding_the_lock_does_not_block_the_next() {
    let (_d, path, mut w, greg) = world();
    w.assert(greg, "tag", tag("a"), Some(Tick(1))).unwrap();
    let before = bytes(&path);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_dies_holding_the_lock", "--nocapture"])
        .env("KALA_TEST_DIE_HOLDING_LOCK", &path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success(), "the child died: {status:?}");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(6), "killed by its abort: {status:?}");
    }
    assert_eq!(bytes(&path), before, "it died before appending");
    // The lock died with it: the next writer proceeds without waiting.
    w.set_busy_timeout(Duration::ZERO);
    assert_eq!(
        w.assert(greg, "tag", tag("b"), Some(Tick(2)))
            .unwrap()
            .changed,
        1
    );
    assert_clean(&w);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
    assert!(quarantine_files(&path).is_empty());
}

#[test]
fn a_failed_fsync_is_cut_under_the_lock_and_other_handles_carry_on() {
    let (_d, path, mut first, greg) = world();
    let mut second = World::open(&path).unwrap();
    let before = bytes(&path);
    first.inject_log_fault(LogFault::FailNextSync);
    assert!(matches!(
        first.assert(greg, "tag", tag("lost"), Some(Tick(1))),
        Err(Error::Io { .. })
    ));
    assert_eq!(
        bytes(&path),
        before,
        "cut back before the lock was released"
    );
    assert_eq!(second.catch_up().unwrap(), 0);
    assert_eq!(
        second
            .assert(greg, "tag", tag("kept"), Some(Tick(2)))
            .unwrap()
            .changed,
        1
    );
    assert_eq!(tags(&second, greg), vec![tag("kept")]);
    assert_clean(&second);
}

// ------------------------------------------------------------ no deadlock

#[test]
fn two_threads_committing_on_two_handles_always_finish() {
    const ROUNDS: i64 = 150;
    let (_d, path, first, greg) = world();
    let second = World::open(&path).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let workers: Vec<_> = [("a", first), ("b", second)]
        .into_iter()
        .map(|(name, mut w)| {
            let done = done_tx.clone();
            std::thread::spawn(move || {
                for i in 0..ROUNDS {
                    // Auto-committed writes, and explicit transactions
                    // retried when another commit landed first.
                    w.assert(greg, "tag", tag(&format!("{name}-auto-{i}")), Some(Tick(i)))
                        .unwrap();
                    loop {
                        let mut txn = w.begin();
                        txn.assert(greg, "tag", tag(&format!("{name}-txn-{i}")), Some(Tick(i)))
                            .unwrap();
                        match txn.commit() {
                            Ok(res) => {
                                assert!(res.committed);
                                break;
                            }
                            Err(Error::SnapshotStale { .. }) => continue,
                            Err(e) => panic!("{name}: {e:?}"),
                        }
                    }
                }
                done.send(()).unwrap();
                w
            })
        })
        .collect();
    drop(done_tx);
    // The watchdog: both loops finish well within this, or it is a hang.
    for _ in 0..2 {
        done_rx
            .recv_timeout(Duration::from_secs(120))
            .expect("a committing thread hung or died");
    }
    let mut handles: Vec<World> = workers.into_iter().map(|t| t.join().unwrap()).collect();
    for w in &mut handles {
        w.catch_up().unwrap();
    }
    // No transaction lost, none interleaved: every write is there once and
    // the tx numbers are consecutive.
    assert_eq!(handles[0].latest_tx(), TxId(1 + 4 * ROUNDS as u64));
    assert_eq!(tags(&handles[0], greg).len(), 4 * ROUNDS as usize);
    assert_eq!(state(&handles[0]), state(&handles[1]));
    let txs: Vec<u64> = spans(&bytes(&path)).iter().map(|s| s.tx).collect();
    assert_eq!(txs, (0..=1 + 4 * ROUNDS as u64).collect::<Vec<_>>());
    assert_clean(&handles[0]);
    assert!(
        doctor_log(&path, &DoctorOptions::default())
            .unwrap()
            .is_clean()
    );
}
