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

// ---- D: the file and the handle must agree before anything is appended ----

/// Commits on `w` while `foreign` does something to the file between the
/// commit's catch-up (under the lock) and its append: what a program that
/// ignores the lock, or a file system whose locks do nothing, allows.
fn commit_with_foreign_change(foreign: impl FnOnce(&Path) + 'static) {
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "tag", s("a"), Some(Tick(1))).unwrap();
    let before = World::open_read_only(&path).unwrap();
    let p = path.clone();
    let changed = Rc::new(RefCell::new(Vec::new()));
    let out = changed.clone();
    on_log_hook(LogHook::BeforeAppend, move || {
        foreign(&p);
        *out.borrow_mut() = bytes(&p);
    });
    let res = w.assert(greg, "tag", s("b"), Some(Tick(2)));
    assert_eq!(res.unwrap_err(), Error::LogDiverged);
    assert!(
        bytes(&path) == *changed.borrow(),
        "a refused append writes nothing and cuts nothing"
    );
    // The handle's own state is what it was, and it appends no more.
    assert_eq!(w.latest_tx(), TxId(2));
    assert_eq!(state(&w, greg), state(&before, greg));
    assert_eq!(
        w.assert(greg, "tag", s("c"), Some(Tick(3))).unwrap_err(),
        Error::LogDiverged
    );
    assert!(bytes(&path) == *changed.borrow());
}

#[test]
fn an_append_is_refused_when_the_file_grew_under_the_lock() {
    commit_with_foreign_change(|path| {
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(b"foreign bytes").unwrap();
    });
}

#[test]
fn an_append_is_refused_when_the_file_shrank_under_the_lock() {
    commit_with_foreign_change(|path| {
        let b = bytes(path);
        let last = *common::spans(&b).last().unwrap();
        // Another writer's "rollback": the handle's last transaction is gone.
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_len(last.start as u64).unwrap();
    });
}

// ---- F: a writable open replays without the writer lock ----

#[test]
fn a_writable_open_lets_others_commit_while_it_replays_and_catches_up_with_them() {
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "tag", s("before"), Some(Tick(1))).unwrap();
    // Another handle commits when the open has replayed the log; it gives
    // up at once if the lock is held.
    w.set_busy_timeout(std::time::Duration::ZERO);
    let during = Rc::new(RefCell::new(None));
    let out = during.clone();
    on_log_hook(LogHook::OpenReplayed, move || {
        let res = w.assert(greg, "tag", s("during-open"), Some(Tick(2)));
        *out.borrow_mut() = Some(res.map(|r| r.entries));
    });
    let mut opened = World::open(&path).unwrap();
    assert_eq!(
        *during.borrow(),
        Some(Ok(vec![EntryId(10)])),
        "a commit during the replay is not kept out"
    );
    // The open ends at the file's state under the lock: it has the
    // transaction committed while it replayed.
    assert_eq!(opened.latest_tx(), TxId(3));
    assert_eq!(opened.recovery(), &RecoveryReport::default());
    let fresh = World::open_read_only(&path).unwrap();
    assert_eq!(state(&opened, greg), state(&fresh, greg));
    assert_eq!(opened.catch_up(), Ok(0));
    let res = opened
        .assert(greg, "tag", s("after"), Some(Tick(3)))
        .unwrap();
    assert_eq!(res.entries, vec![EntryId(12)]);
    assert_clean(&opened);
}

#[test]
fn a_writable_open_still_cuts_a_torn_tail_and_reports_a_damaged_one() {
    // Torn: cut at open, under the lock, as before.
    let (_dir, path, mut w, greg) = world();
    w.assert(greg, "tag", s("torn"), Some(Tick(1))).unwrap();
    drop(w);
    let full = bytes(&path);
    let last = *common::spans(&full).last().unwrap();
    std::fs::write(&path, &full[..full.len() - 3]).unwrap();
    let opened = World::open(&path).unwrap();
    assert_eq!(opened.recovery().cause, Some(CutCause::Torn));
    assert_eq!(opened.latest_tx(), TxId(1));
    assert!(bytes(&path) == full[..last.start]);
    let q = quarantine_files(&path);
    assert!(bytes(&q[0]) == full[last.start..full.len() - 3]);
    // A torn tail written while the open replayed is cut too.
    drop(opened);
    let cut = bytes(&path);
    let p2 = path.clone();
    let torn = full[last.start..full.len() - 3].to_vec();
    on_log_hook(LogHook::OpenReplayed, move || {
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(&p2).unwrap();
        f.write_all(&torn).unwrap();
    });
    let opened = World::open(&path).unwrap();
    assert_eq!(opened.recovery().cause, Some(CutCause::Torn));
    assert!(bytes(&path) == cut);
    assert_eq!(quarantine_files(&path).len(), 2);
    assert_clean(&opened);
}

/// Opens the world (read-only or for writing) from another thread while a
/// commit sits between its write and its failing fsync for `pause`.
fn open_during_a_failing_commit(read_only: bool, pause: std::time::Duration) -> World {
    use std::sync::mpsc::channel;
    let (_dir, path, w, greg) = world();
    let (written_tx, written_rx) = channel();
    let writer = std::thread::spawn(move || {
        let mut w = w;
        on_log_hook(LogHook::BeforeSync, move || {
            written_tx.send(()).unwrap();
            std::thread::sleep(pause);
        });
        w.inject_log_fault(LogFault::FailNextSync);
        assert!(w.assert(greg, "tag", s("lost"), Some(Tick(1))).is_err());
    });
    written_rx.recv().unwrap();
    let opened = if read_only {
        World::open_read_only(&path).unwrap()
    } else {
        World::open(&path).unwrap()
    };
    writer.join().unwrap();
    std::mem::forget(_dir);
    opened
}

#[test]
fn an_open_during_a_commit_whose_fsync_fails_does_not_include_it() {
    for read_only in [true, false] {
        let mut opened =
            open_during_a_failing_commit(read_only, std::time::Duration::from_millis(30));
        assert_eq!(opened.latest_tx(), TxId(1), "read_only={read_only}");
        assert_eq!(opened.catch_up(), Ok(0), "not diverged");
        assert_eq!(opened.recovery(), &RecoveryReport::default());
        assert_clean(&opened);
    }
}

// ---- E: the writer lock is the lock of the file the path names ----

#[test]
fn with_writer_lock_refuses_when_the_path_names_another_file() {
    let (dir, path, mut w, greg) = world();
    w.assert(greg, "tag", s("a"), Some(Tick(1))).unwrap();
    // Another process replaced the world (a forced load renames a new log
    // over the path): this handle's lock is the old, unlinked file's.
    let other = dir.path().join("other.kala");
    drop(World::create(&other, schema()).unwrap());
    std::fs::rename(&other, &path).unwrap();
    let replaced = bytes(&path);
    let mut ran = false;
    let res = w.with_writer_lock(|| ran = true);
    assert_eq!(res, Err(Error::LogDiverged));
    assert!(!ran, "nothing runs under the lock of a file the path left");
    assert!(bytes(&path) == replaced);
    // The lock was released: the new world is free to write.
    let mut new = World::open(&path).unwrap();
    new.create_entity("CHARACTER", Some("x")).unwrap();
}

#[test]
fn a_file_is_locked_for_replacing_only_while_the_path_names_it() {
    use kala_core::log::with_file_lock;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-world.kala");
    std::fs::write(&path, b"not a kala log").unwrap();
    // Free: runs under the lock.
    assert_eq!(with_file_lock(&path, Duration::ZERO, || 7), Ok(Some(7)));
    // Held by someone else: refused after the wait, nothing run.
    let held = hold_lock(&path);
    let mut ran = false;
    assert_eq!(
        with_file_lock(&path, Duration::from_millis(50), || ran = true),
        Err(Error::WorldLocked)
    );
    assert!(!ran);
    drop(held);
    // While it runs, a writer of that file is kept out.
    let inside = with_file_lock(&path, Duration::ZERO, || {
        let f = OpenOptions::new().read(true).open(&path).unwrap();
        f.try_lock().is_err()
    });
    assert_eq!(inside, Ok(Some(true)));
    // Nothing there: nothing to lock.
    let missing = dir.path().join("missing.kala");
    assert_eq!(with_file_lock(&missing, Duration::ZERO, || 1), Ok(None));
}

// ---- J: weak mode refuses a correction of an entry corrected meanwhile ----

fn move_to(tick: i64) -> Correction {
    Correction::Replace {
        value: None,
        tick: Some(Tick(tick)),
        op: None,
        precision: None,
    }
}

#[test]
fn a_weak_correction_of_an_entry_corrected_meanwhile_is_refused() {
    for (theirs, what) in [
        (move_to(580), "a replace over a replace"),
        (Correction::Delete, "a replace over a delete"),
    ] {
        let (_dir, path, mut a, greg) = world();
        let entry = a
            .assert(greg, "tag", s("x"), Some(Tick(600)))
            .unwrap()
            .entries[0];
        let mut b = World::open(&path).unwrap();
        let mut tb = b.begin();
        tb.set_concurrency(Concurrency::Weak);
        tb.correct(entry, move_to(590)).unwrap();
        a.correct(entry, theirs).unwrap();
        let before = bytes(&path);
        let err = tb.commit().unwrap_err();
        assert_eq!(err, Error::AlreadyCorrected { entry, rev: 1 }, "{what}");
        assert_eq!(
            err.to_string(),
            format!(
                "log entry#{} was already corrected by a newer transaction (now rev 1); \
                 nothing was written",
                entry.0
            )
        );
        assert!(bytes(&path) == before, "{what}: nothing written");
        // The handle is caught up and can decide again.
        assert_eq!(state(&b, greg), state(&a, greg), "{what}");
        assert_clean(&b);
    }
    // A delete over a delete, and a correction named by a revised entry.
    let (_dir, path, mut a, greg) = world();
    let entry = a
        .assert(greg, "tag", s("x"), Some(Tick(600)))
        .unwrap()
        .entries[0];
    let revised = a.correct(entry, move_to(595)).unwrap().entries[1];
    let mut b = World::open(&path).unwrap();
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    tb.correct(revised, Correction::Delete).unwrap();
    a.correct(entry, Correction::Delete).unwrap();
    assert_eq!(
        tb.commit().unwrap_err(),
        Error::AlreadyCorrected {
            entry: revised,
            rev: 2
        }
    );
    // Not moved: a weak correction over unrelated commits goes through.
    let (_dir, path, mut a, greg) = world();
    let entry = a
        .assert(greg, "tag", s("x"), Some(Tick(600)))
        .unwrap()
        .entries[0];
    let mut b = World::open(&path).unwrap();
    let mut tb = b.begin();
    tb.set_concurrency(Concurrency::Weak);
    tb.correct(entry, move_to(590)).unwrap();
    a.assert(greg, "tag", s("y"), Some(Tick(601))).unwrap();
    assert_eq!(tb.commit().unwrap().changed, 2);
    assert_clean(&b);
}

// ---- N: a conflict on an edge names the committed edge, not a
// provisional number ----

#[test]
fn a_conflict_on_a_merged_edge_names_the_edge_it_turned_out_to_be() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("world.kala");
    let schema = schema()
        .attribute(
            AttrDef::new("present", ValueType::Ref, Kind::State, Cardinality::Many).directed(),
        )
        .attribute(AttrDef::new(
            "disguised",
            ValueType::Bool,
            Kind::State,
            Cardinality::One,
        ));
    let mut a = World::create(&path, schema).unwrap();
    let greg = a.create_entity("CHARACTER", Some("greg")).unwrap();
    let varrow = a.create_entity("PLACE", Some("varrow")).unwrap();
    let ashford = a.create_entity("PLACE", Some("ashford")).unwrap();
    let mut b = World::open(&path).unwrap();

    let mut tb = b.begin();
    tb.assert(greg, "present", Value::Ref(varrow), Some(Tick(1)))
        .unwrap();
    let provisional = tb.edge_id(greg, "present", varrow).unwrap();
    // It reads the new edge's properties (none) and decides from that.
    let meter = WorkMeter::bounded(1_000, None);
    let held = tb
        .values_at(provisional.into(), "disguised", Tick(1), 4, 1 << 10, &meter)
        .unwrap();
    assert!(held.values.is_empty());
    tb.assert(
        provisional.into(),
        "disguised",
        Value::Bool(true),
        Some(Tick(1)),
    )
    .unwrap();

    // Meanwhile: another edge takes the provisional number, and the same
    // triple is interned (with a property) under the next one.
    a.assert(greg, "present", Value::Ref(ashford), Some(Tick(1)))
        .unwrap();
    let other = a.edge_id(greg, "present", ashford).unwrap();
    assert_eq!(other, provisional, "the number now names another edge");
    a.assert(greg, "present", Value::Ref(varrow), Some(Tick(1)))
        .unwrap();
    let merged = a.edge_id(greg, "present", varrow).unwrap();
    a.assert(
        merged.into(),
        "disguised",
        Value::Bool(false),
        Some(Tick(1)),
    )
    .unwrap();

    let Err(Error::Conflict(conflict)) = tb.commit() else {
        panic!("expected a conflict");
    };
    assert_eq!(
        conflict.key,
        ConflictKey::EntityAttribute {
            entity: merged.into(),
            attr: "disguised".into()
        },
        "the edge the transaction's triple is, not the number it had"
    );
    assert_eq!(conflict.operation, 1, "the read was made before op 2");
}

// ---- O (round 2): a read that failed on an undeclared name read its
// absence ----

#[test]
fn a_read_of_an_undeclared_name_conflicts_once_a_newer_commit_declares_it() {
    type Read = fn(&Txn<'_>, EntityId) -> Result<(), Error>;
    let reads: [(&str, Read, &str); 6] = [
        ("note_read", |t, e| t.note_read(e, "mood"), "mood"),
        (
            "note_reverse_read",
            |t, e| t.note_reverse_read(e, "mood"),
            "mood",
        ),
        (
            "note_value_read",
            |t, _| t.note_value_read("mood", &s("grim")),
            "mood",
        ),
        (
            "note_read_attribute",
            |t, _| t.note_read_attribute("mood"),
            "mood",
        ),
        (
            "values_at",
            |t, e| {
                let meter = WorkMeter::bounded(1_000, None);
                t.values_at(e, "mood", Tick(1), 4, 1 << 10, &meter)
                    .map(|_| ())
            },
            "mood",
        ),
        (
            "note_type_scan",
            |t, _| t.note_type_scan("FACTION"),
            "FACTION",
        ),
    ];
    for (what, read, name) in reads {
        // The caller treats "no such name" as "nothing there" and writes.
        let (_dir, path, mut a, greg) = world();
        let mut b = World::open(&path).unwrap();
        let mut tb = b.begin();
        let err = read(&tb, greg).unwrap_err();
        assert!(
            matches!(err, Error::UnknownAttribute(_) | Error::UnknownNodeType(_)),
            "{what}: {err:?}"
        );
        tb.assert(greg, "tag", s("moodless"), Some(Tick(1)))
            .unwrap();
        // Meanwhile the name is declared and used.
        let mut ta = a.begin();
        ta.declare_attribute(AttrDef::new(
            "mood",
            ValueType::Str,
            Kind::State,
            Cardinality::One,
        ))
        .unwrap();
        ta.declare_node_type("FACTION").unwrap();
        ta.assert(greg, "mood", s("grim"), Some(Tick(1))).unwrap();
        ta.create_entity("FACTION", Some("ashen")).unwrap();
        ta.commit().unwrap();
        let before = bytes(&path);
        let outcome = tb.commit();
        let Err(Error::Conflict(conflict)) = outcome else {
            panic!("{what}: expected a conflict, got {outcome:?}");
        };
        assert_eq!(
            conflict.key,
            ConflictKey::Undeclared { name: name.into() },
            "{what}"
        );
        assert!(bytes(&path) == before, "{what}: nothing written");

        // Still undeclared at commit: nothing to conflict with.
        let (_dir, path, mut a, greg) = world();
        let mut b = World::open(&path).unwrap();
        let mut tb = b.begin();
        read(&tb, greg).unwrap_err();
        tb.assert(greg, "tag", s("moodless"), Some(Tick(1)))
            .unwrap();
        a.assert(greg, "tag", s("unrelated"), Some(Tick(2)))
            .unwrap();
        assert_eq!(tb.commit().unwrap().changed, 1, "{what}");
        assert_clean(&b);
    }
}

// ---- P (round 2): reads made before a switch to weak mode are kept ----

#[test]
fn reads_recorded_before_a_switch_to_weak_count_again_after_switching_back() {
    for (theirs, conflicts) in [("age", true), ("tag", false)] {
        let (_dir, path, mut a, greg) = world();
        let mut b = World::open(&path).unwrap();
        let mut tb = b.begin();
        // Read under optimistic mode, then weak and back with no read in
        // between: the read still counts.
        tb.note_read(greg, "age").unwrap();
        tb.set_concurrency(Concurrency::Weak);
        tb.set_concurrency(Concurrency::Optimistic);
        tb.assert(greg, "age", Value::I64(30), Some(Tick(5)))
            .unwrap();
        let value = if theirs == "age" {
            Value::I64(29)
        } else {
            s("unrelated")
        };
        a.assert(greg, theirs, value, Some(Tick(5))).unwrap();
        let outcome = tb.commit();
        if conflicts {
            let Err(Error::Conflict(conflict)) = outcome else {
                panic!("expected a conflict, got {outcome:?}");
            };
            assert_eq!(
                conflict.key,
                ConflictKey::EntityAttribute {
                    entity: greg,
                    attr: "age".into()
                }
            );
        } else {
            // Exactly what was read, not "everything".
            assert_eq!(outcome.unwrap().changed, 1);
            assert_clean(&b);
        }
    }
    // Committed while weak, the earlier read is not checked.
    let (_dir, path, mut a, greg) = world();
    let mut b = World::open(&path).unwrap();
    let mut tb = b.begin();
    tb.note_read(greg, "age").unwrap();
    tb.set_concurrency(Concurrency::Weak);
    tb.assert(greg, "age", Value::I64(30), Some(Tick(5)))
        .unwrap();
    a.assert(greg, "age", Value::I64(29), Some(Tick(5)))
        .unwrap();
    assert_eq!(tb.commit().unwrap().changed, 1);
}
