//! The writer lock must not report `Busy` when nobody else writes.
//!
//! A process that spawns children from another thread briefly shares its
//! open files with each child (between fork and exec). A lock on a file
//! closed in that window is still held for a moment, so reopening the system
//! world right after bootstrapping it can find it locked although no other
//! writer exists. `kala`'s own test binaries do exactly this.
#![cfg(unix)]
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn bootstrap_then_write_is_not_busy_while_other_threads_spawn_processes() {
    let stop = Arc::new(AtomicBool::new(false));
    let spawners: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = Command::new("true").status();
                }
            })
        })
        .collect();
    let mut failures = Vec::new();
    for i in 0..300 {
        let dir = tempfile::tempdir().unwrap();
        if let Err(e) = kala_system::system::add(dir.path(), "author@localhost", None) {
            failures.push((i, e));
        }
    }
    stop.store(true, Ordering::Relaxed);
    for s in spawners {
        s.join().unwrap();
    }
    assert!(failures.is_empty(), "{failures:?}");
}

/// Several account commands at once on one directory (the writer lock is
/// taken per transaction, slice 15b): each one is applied, or reports
/// `Busy` having written nothing, and none is lost or half-applied.
#[test]
fn concurrent_account_changes_are_applied_or_busy_and_never_lost() {
    use kala_system::system::{self, SystemError};
    let dir = tempfile::tempdir().unwrap();
    system::ensure(dir.path()).unwrap();
    let workers: Vec<_> = (0..4)
        .map(|t| {
            let dir = dir.path().to_owned();
            std::thread::spawn(move || {
                let mut added = Vec::new();
                for i in 0..10 {
                    let account = format!("user{t}x{i}@localhost");
                    // `Busy` says "retry the command": do, a few times.
                    let mut tries = 0;
                    loop {
                        match system::add(&dir, &account, None) {
                            Ok(_) => {
                                added.push(account);
                                break;
                            }
                            Err(SystemError::Busy) if tries < 200 => tries += 1,
                            Err(e) => panic!("{account}: {e:?}"),
                        }
                    }
                }
                added
            })
        })
        .collect();
    let mut added: Vec<String> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    added.sort();
    assert_eq!(added.len(), 40);
    let mut listed: Vec<String> = system::list(dir.path())
        .unwrap()
        .into_iter()
        .map(|a| format!("{}@{}", a.user, a.host))
        .filter(|a| a.starts_with("user"))
        .collect();
    listed.sort();
    assert_eq!(listed, added);
}
