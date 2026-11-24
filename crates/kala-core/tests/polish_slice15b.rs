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

// ---- B: a damaged tail stays in the file until a handle that
// acknowledged it commits ----

/// `b` was opened before `tx N` (a tag on greg) was appended by another
/// handle; then tx N's body is damaged on disk. Returns the damaged span.
fn damaged_behind(path: &Path, greg: EntityId) -> (World, common::Span) {
    let b = World::open(path).unwrap();
    let mut third = World::open(path).unwrap();
    third
        .assert(greg, "tag", s("acknowledged-then-damaged"), Some(Tick(1)))
        .unwrap();
    drop(third);
    let span = damage_last_txn(path);
    (b, span)
}

#[test]
fn opening_a_damaged_world_reports_the_tail_and_leaves_it_in_the_file() {
    let (_dir, path, w, greg) = world();
    drop(w);
    let (_b, span) = damaged_behind(&path, greg);
    let damaged = bytes(&path);

    let mut a = World::open(&path).unwrap();
    assert_eq!(bytes(&path), damaged, "the open cut nothing");
    assert_eq!(quarantine_files(&path), Vec::<PathBuf>::new());
    let r = a.recovery().clone();
    assert_eq!(r.cause, Some(CutCause::Damaged));
    assert_eq!(r.bytes_cut, (span.end - span.start) as u64);
    assert_eq!(r.quarantine, None, "nothing is quarantined before the cut");
    // The damaged transaction is not applied.
    assert_eq!(a.as_of(greg, "tag", Tick(1)).unwrap(), vec![]);
    assert_eq!(a.latest_tx(), TxId(1));
    // Writes are refused until acknowledged; the file stays as it is.
    assert!(matches!(
        a.assert(greg, "tag", s("x"), Some(Tick(1))),
        Err(Error::RecoveryUnacknowledged)
    ));
    assert_eq!(bytes(&path), damaged);

    // Acknowledged: the next commit quarantines, cuts and commits.
    a.acknowledge_recovery();
    a.assert(greg, "tag", s("x"), Some(Tick(1))).unwrap();
    let q = quarantine_files(&path);
    assert_eq!(q.len(), 1);
    assert_eq!(bytes(&q[0]), damaged[span.start..]);
    assert_eq!(bytes(&path)[..span.start], damaged[..span.start]);
    let r = a.recovery();
    assert_eq!(r.cause, Some(CutCause::Damaged));
    assert_eq!(r.bytes_cut, (span.end - span.start) as u64);
    assert_eq!(r.quarantine.as_deref(), Some(q[0].as_path()));
    assert_eq!(a.latest_tx(), TxId(2));
    assert_clean(&a);
    drop(a);
    let again = World::open(&path).unwrap();
    assert_eq!(again.recovery().cause, None);
    assert_eq!(again.as_of(greg, "tag", Tick(1)).unwrap(), vec![s("x")]);
}

#[test]
fn a_handle_already_open_cannot_reuse_a_damaged_tails_entry_ids_unacknowledged() {
    let (_dir, path, w, greg) = world();
    drop(w);
    let (mut b, span) = damaged_behind(&path, greg);
    let damaged = bytes(&path);

    // "Process A" opens the damaged world (and never acknowledges).
    let mut a = World::open(&path).unwrap();
    assert_eq!(a.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(bytes(&path), damaged);

    // B's tip is before the damaged transaction. Its commit must not land
    // where that transaction's entry IDs were without anyone acknowledging.
    for _ in 0..2 {
        match b.assert(greg, "tag", s("from-b"), Some(Tick(2))) {
            Err(Error::RecoveryRequired { bytes }) => {
                assert_eq!(bytes, (span.end - span.start) as u64);
            }
            other => panic!("expected RecoveryRequired, got {other:?}"),
        }
        assert_eq!(bytes(&path), damaged, "a refused commit touches nothing");
        assert_eq!(quarantine_files(&path), Vec::<PathBuf>::new());
    }

    // B acknowledges: its next commit quarantines, cuts and commits.
    b.acknowledge_recovery();
    let res = b.assert(greg, "tag", s("from-b"), Some(Tick(2))).unwrap();
    let q = quarantine_files(&path);
    assert_eq!(q.len(), 1, "the damaged bytes were saved before the cut");
    assert_eq!(bytes(&q[0]), damaged[span.start..]);
    assert_eq!(b.recovery().cause, Some(CutCause::Damaged));
    assert_eq!(b.recovery().quarantine.as_deref(), Some(q[0].as_path()));
    assert_eq!(res.entries.len(), 1);
    assert_eq!(b.latest_tx(), TxId(2));
    assert_clean(&b);

    // A never acknowledged what its open reported: still refused.
    let committed = bytes(&path);
    assert!(matches!(
        a.assert(greg, "tag", s("from-a"), Some(Tick(3))),
        Err(Error::RecoveryUnacknowledged)
    ));
    assert_eq!(bytes(&path), committed);
    // Once it does, it writes on top of B's commit.
    a.acknowledge_recovery();
    a.assert(greg, "tag", s("from-a"), Some(Tick(3))).unwrap();
    assert_eq!(a.latest_tx(), TxId(3));
    assert_eq!(quarantine_files(&path).len(), 1);
    assert_clean(&a);
}

#[test]
fn an_acknowledged_damaged_tail_can_be_cut_without_a_commit() {
    let (_dir, path, w, greg) = world();
    drop(w);
    let (_b, span) = damaged_behind(&path, greg);
    let damaged = bytes(&path);

    let mut a = World::open(&path).unwrap();
    // Not acknowledged: refused, nothing touched.
    assert!(matches!(
        a.cut_acknowledged_tail(),
        Err(Error::RecoveryUnacknowledged)
    ));
    assert_eq!(bytes(&path), damaged);
    a.acknowledge_recovery();
    let report = a.cut_acknowledged_tail().unwrap().clone();
    assert_eq!(report.cause, Some(CutCause::Damaged));
    let q = quarantine_files(&path);
    assert_eq!(report.quarantine.as_deref(), Some(q[0].as_path()));
    assert_eq!(bytes(&q[0]), damaged[span.start..]);
    assert_eq!(bytes(&path), damaged[..span.start]);
    // Again: nothing more to cut, the report stays.
    assert_eq!(a.cut_acknowledged_tail().unwrap(), &report);
    assert_eq!(quarantine_files(&path).len(), 1);
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

#[test]
fn an_acknowledgement_covers_only_the_damaged_tail_that_was_reported() {
    let (_dir, path, w, greg) = world();
    drop(w);
    let (mut b, _) = damaged_behind(&path, greg);
    // A reports the tail at open and acknowledges it, but doesn't commit yet.
    let mut a = World::open(&path).unwrap();
    a.acknowledge_recovery();
    // B is refused, acknowledges, cuts and commits; a third writer's commit
    // on top is then damaged: another tail, which A has never seen.
    assert!(matches!(
        b.assert(greg, "tag", s("b"), Some(Tick(2))),
        Err(Error::RecoveryRequired { .. })
    ));
    b.acknowledge_recovery();
    b.assert(greg, "tag", s("b"), Some(Tick(2))).unwrap();
    b.assert(greg, "tag", s("second-loss"), Some(Tick(3)))
        .unwrap();
    drop(b);
    damage_last_txn(&path);
    let damaged = bytes(&path);
    // A's old acknowledgement doesn't license cutting it.
    assert!(matches!(
        a.assert(greg, "tag", s("a"), Some(Tick(4))),
        Err(Error::RecoveryRequired { .. })
    ));
    assert!(bytes(&path) == damaged, "nothing cut");
    assert_eq!(quarantine_files(&path).len(), 1);
    a.acknowledge_recovery();
    a.assert(greg, "tag", s("a"), Some(Tick(4))).unwrap();
    assert_eq!(quarantine_files(&path).len(), 2);
    assert_clean(&a);
}

// ---- C: a reader never applies a transaction before it is fsynced ----

use kala_core::log::{LogFault, LogHook, on_log_hook};
use std::cell::RefCell;
use std::rc::Rc;

/// What a second handle (`r`, writable or read-only) saw when it looked at
/// the file between a writer's write and its fsync.
#[derive(Debug, Default)]
struct Seen {
    file_grew: bool,
    catch_up: Option<Result<usize, Error>>,
    latest_tx: Option<TxId>,
    begin_snapshot: Option<TxId>,
}

fn look_before_sync(path: &Path, r: &Rc<RefCell<World>>, writer_len: u64) -> Rc<RefCell<Seen>> {
    let seen = Rc::new(RefCell::new(Seen::default()));
    let (r, out, path) = (r.clone(), seen.clone(), path.to_owned());
    on_log_hook(LogHook::BeforeSync, move || {
        let mut r = r.borrow_mut();
        let mut seen = out.borrow_mut();
        seen.file_grew = std::fs::metadata(&path).unwrap().len() > writer_len;
        seen.catch_up = Some(r.catch_up());
        // `begin` looks at the file too.
        seen.begin_snapshot = Some(r.begin().snapshot_tx());
        seen.latest_tx = Some(r.latest_tx());
    });
    seen
}

fn nothing_applied_before_the_fsync(read_only: bool) {
    let (_dir, path, mut w, greg) = world();
    let r = Rc::new(RefCell::new(if read_only {
        World::open_read_only(&path).unwrap()
    } else {
        World::open(&path).unwrap()
    }));
    let before = bytes(&path);

    // The fsync fails: the writer cuts its transaction back off the file.
    let seen = look_before_sync(&path, &r, before.len() as u64);
    w.inject_log_fault(LogFault::FailNextSync);
    assert!(w.assert(greg, "tag", s("lost"), Some(Tick(1))).is_err());
    let seen = seen.borrow();
    assert!(seen.file_grew, "the hook ran with the bytes in the file");
    assert_eq!(seen.catch_up, Some(Ok(0)), "not yet: {seen:?}");
    assert_eq!(seen.latest_tx, Some(TxId(1)));
    assert_eq!(seen.begin_snapshot, Some(TxId(1)));
    assert!(bytes(&path) == before, "rolled back");

    // The reader never saw it, so it has not diverged.
    let mut r = Rc::try_unwrap(r).unwrap().into_inner();
    assert_eq!(r.catch_up(), Ok(0));
    assert_eq!(r.as_of(greg, "tag", Tick(1)).unwrap(), vec![]);
    if !read_only {
        let res = r.assert(greg, "tag", s("mine"), Some(Tick(1))).unwrap();
        assert_eq!(res.entries, vec![EntryId(8)]);
        assert_eq!(r.latest_tx(), TxId(2));
    }
    assert_clean(&r);
}

#[test]
fn a_writable_handle_applies_nothing_between_a_write_and_its_failed_fsync() {
    nothing_applied_before_the_fsync(false);
}

#[test]
fn a_read_only_handle_applies_nothing_between_a_write_and_its_failed_fsync() {
    nothing_applied_before_the_fsync(true);
}

#[test]
fn a_handle_sees_a_transaction_once_its_fsync_succeeded() {
    for read_only in [false, true] {
        let (_dir, path, mut w, greg) = world();
        let r = Rc::new(RefCell::new(if read_only {
            World::open_read_only(&path).unwrap()
        } else {
            World::open(&path).unwrap()
        }));
        let before = bytes(&path).len() as u64;
        let seen = look_before_sync(&path, &r, before);
        w.assert(greg, "tag", s("kept"), Some(Tick(1))).unwrap();
        let seen = seen.borrow();
        assert!(seen.file_grew);
        assert_eq!(seen.catch_up, Some(Ok(0)), "not yet: {seen:?}");
        assert_eq!(seen.latest_tx, Some(TxId(1)));
        let mut r = r.borrow_mut();
        assert_eq!(r.catch_up(), Ok(1));
        assert_eq!(r.as_of(greg, "tag", Tick(1)).unwrap(), vec![s("kept")]);
    }
}

#[test]
fn readers_looping_catch_up_never_keep_a_writer_out() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    let (_dir, path, mut w, greg) = world();
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..8)
        .map(|i| {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let mut r = if i % 2 == 0 {
                    World::open_read_only(&path).unwrap()
                } else {
                    World::open(&path).unwrap()
                };
                let mut applied = 0;
                while !done.load(Ordering::Relaxed) {
                    applied += r.catch_up().unwrap();
                }
                applied += r.catch_up().unwrap();
                (applied, r.latest_tx())
            })
        })
        .collect();
    // The watchdog: a writer starved of the lock fails with WorldLocked
    // after its busy timeout, and the whole run must stay far below what
    // 200 timeouts would take.
    w.set_busy_timeout(Duration::from_secs(5));
    let started = Instant::now();
    for i in 0..200 {
        w.assert(greg, "tag", s(&format!("t{i}")), Some(Tick(i)))
            .unwrap();
    }
    let took = started.elapsed();
    done.store(true, Ordering::Relaxed);
    for r in readers {
        let (applied, latest) = r.join().unwrap();
        assert_eq!(latest, TxId(201));
        // Less than 200 if the reader opened after the first commits.
        assert!(applied <= 200);
    }
    assert!(took < Duration::from_secs(60), "200 commits took {took:?}");
    assert_clean(&w);
}
