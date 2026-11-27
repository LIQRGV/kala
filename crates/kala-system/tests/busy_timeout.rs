//! `apply_waiting`: an account change waits for the system world's writer
//! lock as long as its caller says (`kala user … --busy-timeout`), then
//! reports `Busy` having written nothing.
use std::time::{Duration, Instant};

use kala_system::SystemError;
use kala_system::system::{Change, Recovery, apply, apply_waiting, list};

const GRANT: Change<'static> = Change::Grant {
    account: "root@localhost",
    privilege: "read",
    db: "story",
};

#[test]
fn a_held_lock_is_busy_after_the_given_timeout() {
    let dir = tempfile::tempdir().unwrap();
    list(dir.path()).unwrap();
    let system = dir.path().join("system.kala");
    let before = std::fs::read(&system).unwrap();
    let held = std::fs::File::open(&system).unwrap();
    held.try_lock().expect("the lock is free");
    for (timeout, at_most) in [
        (Duration::ZERO, Duration::from_millis(1000)),
        (Duration::from_millis(300), Duration::from_millis(2500)),
    ] {
        let started = Instant::now();
        let applied = apply_waiting(dir.path(), GRANT, Recovery::Refuse, timeout);
        let took = started.elapsed();
        assert!(
            matches!(applied.result, Err(SystemError::Busy)),
            "{applied:?}"
        );
        assert!(took >= timeout, "{timeout:?}: gave up after {took:?}");
        assert!(
            took < at_most,
            "{timeout:?}: waited {took:?} (the default is 5 s)"
        );
        assert_eq!(std::fs::read(&system).unwrap(), before);
    }
    drop(held);
    let report = apply_waiting(dir.path(), GRANT, Recovery::Refuse, Duration::ZERO)
        .result
        .unwrap();
    assert_eq!(report.changed, 1);
    // `apply` is the same with kala-core's default wait.
    let again = apply(dir.path(), GRANT, Recovery::Refuse).result.unwrap();
    assert_eq!(again.changed, 0);
}
